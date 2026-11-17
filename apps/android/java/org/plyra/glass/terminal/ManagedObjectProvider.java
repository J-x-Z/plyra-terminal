package org.plyra.glass.terminal;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.Handler;
import android.os.Looper;
import android.os.ParcelFileDescriptor;
import android.os.ProxyFileDescriptorCallback;
import android.os.SystemClock;
import android.os.storage.StorageManager;
import android.provider.OpenableColumns;
import android.system.ErrnoException;
import android.system.OsConstants;
import android.webkit.MimeTypeMap;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.Locale;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicBoolean;

/** Explicitly shared immutable managed-text snapshots. No filesystem paths are accepted. */
public final class ManagedObjectProvider extends ContentProvider {
    private static final String AUTHORITY = "org.plyra.glass.terminal.objects";
    private static final int MAX_BYTES = 1024 * 1024;
    private static final int MAX_RETAINED_BYTES = 16 * MAX_BYTES;
    private static final int MAX_SNAPSHOTS = 32;
    private static final int MAX_OPEN_HANDLES = 32;
    private static final long LIFETIME_MS = 15 * 60 * 1000;
    private static final Map<String, Snapshot> snapshots = new LinkedHashMap<>();
    private static final Handler expirationHandler = new Handler(Looper.getMainLooper());
    private static int openHandles;

    private static final class Snapshot {
        final String name;
        final byte[] contents;
        final long expires;
        int readers;
        Runnable expiration;

        Snapshot(String name, byte[] contents) {
            this.name = name;
            this.contents = contents.clone();
            this.expires = SystemClock.elapsedRealtime() + LIFETIME_MS;
        }
    }

    static synchronized Uri snapshot(String name, byte[] contents) {
        if (name == null || name.isEmpty() || name.length() > 255 || name.startsWith(".")
                || name.indexOf('/') >= 0 || name.indexOf('\\') >= 0
                || name.chars().anyMatch(Character::isISOControl) || contents.length > MAX_BYTES) {
            throw new IllegalArgumentException("Invalid managed text snapshot");
        }
        prune(true, contents.length);
        if (snapshots.size() >= MAX_SNAPSHOTS
                || retainedBytes() + contents.length > MAX_RETAINED_BYTES) {
            throw new IllegalStateException("Close existing shared documents before opening another");
        }
        String token = UUID.randomUUID().toString();
        Snapshot value = new Snapshot(name, contents);
        value.expiration = () -> expire(token);
        snapshots.put(token, value);
        expirationHandler.postDelayed(value.expiration, LIFETIME_MS);
        return new Uri.Builder().scheme("content").authority(AUTHORITY).appendPath(token).build();
    }

    private static synchronized void expire(String token) {
        Snapshot value = snapshots.get(token);
        if (value != null && value.readers == 0) snapshots.remove(token);
    }

    static synchronized void discard(Uri uri) {
        Snapshot value = snapshots.get(uri.getLastPathSegment());
        if (value != null && value.readers == 0) {
            snapshots.remove(uri.getLastPathSegment());
            expirationHandler.removeCallbacks(value.expiration);
        }
    }

    private static int retainedBytes() {
        int bytes = 0;
        for (Snapshot value : snapshots.values()) bytes += value.contents.length;
        return bytes;
    }

    private static void prune(boolean makeRoom, int incoming) {
        long now = SystemClock.elapsedRealtime();
        Iterator<Snapshot> iterator = snapshots.values().iterator();
        while (iterator.hasNext()) {
            Snapshot value = iterator.next();
            if (value.readers == 0 && (value.expires <= now || (makeRoom
                    && (snapshots.size() >= MAX_SNAPSHOTS
                    || retainedBytes() + incoming > MAX_RETAINED_BYTES)))) {
                iterator.remove();
                expirationHandler.removeCallbacks(value.expiration);
            }
        }
    }

    private static synchronized Snapshot lookup(Uri uri) throws FileNotFoundException {
        prune(false, 0);
        if (!"content".equals(uri.getScheme()) || !AUTHORITY.equals(uri.getAuthority())
                || uri.getPathSegments().size() != 1 || uri.getQuery() != null) {
            throw new FileNotFoundException("Invalid shared-object URI");
        }
        Snapshot value = snapshots.get(uri.getLastPathSegment());
        if (value == null || value.expires <= SystemClock.elapsedRealtime()) {
            throw new FileNotFoundException("This managed snapshot has expired or was released");
        }
        return value;
    }

    static String mimeType(String name) {
        int dot = name.lastIndexOf('.');
        String mime = dot < 0 ? null : MimeTypeMap.getSingleton().getMimeTypeFromExtension(
                name.substring(dot + 1).toLowerCase(Locale.ROOT));
        return mime == null ? "text/plain" : mime;
    }

    @Override
    public boolean onCreate() { return true; }

    @Override
    public String getType(Uri uri) {
        try { return mimeType(lookup(uri).name); }
        catch (FileNotFoundException error) { return null; }
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String order) {
        final Snapshot value;
        try { value = lookup(uri); }
        catch (FileNotFoundException error) { throw new IllegalArgumentException(error.getMessage(), error); }
        String[] columns = projection == null
                ? new String[] { OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE } : projection;
        MatrixCursor cursor = new MatrixCursor(columns, 1);
        MatrixCursor.RowBuilder row = cursor.newRow();
        for (String column : columns) {
            if (OpenableColumns.DISPLAY_NAME.equals(column)) row.add(value.name);
            else if (OpenableColumns.SIZE.equals(column)) row.add(value.contents.length);
            else row.add(null);
        }
        return cursor;
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        if (!"r".equals(mode)) throw new FileNotFoundException("Managed snapshots are read-only");
        final Snapshot value;
        synchronized (ManagedObjectProvider.class) {
            value = lookup(uri);
            if (openHandles >= MAX_OPEN_HANDLES) throw new FileNotFoundException("Shared-document handle limit reached");
            value.readers++;
            openHandles++;
        }
        AtomicBoolean retired = new AtomicBoolean();
        Runnable retire = () -> { if (retired.compareAndSet(false, true)) release(value); };
        ProxyFileDescriptorCallback callback = new ProxyFileDescriptorCallback() {
            @Override
            public long onGetSize() { return value.contents.length; }

            @Override
            public int onRead(long offset, int size, byte[] data) throws ErrnoException {
                if (offset < 0 || size < 0) throw new ErrnoException("read", OsConstants.EINVAL);
                if (offset >= value.contents.length) return 0;
                int count = Math.min(Math.min(size, data.length), value.contents.length - (int) offset);
                System.arraycopy(value.contents, (int) offset, data, 0, count);
                return count;
            }

            @Override
            public int onWrite(long offset, int size, byte[] data) throws ErrnoException {
                throw new ErrnoException("write", OsConstants.EROFS);
            }

            @Override
            public void onRelease() { retire.run(); }
        };
        try {
            StorageManager manager = getContext().getSystemService(StorageManager.class);
            if (manager == null) throw new IOException("Android StorageManager is unavailable");
            return manager.openProxyFileDescriptor(ParcelFileDescriptor.MODE_READ_ONLY, callback,
                    new Handler(Looper.getMainLooper()));
        } catch (IOException | RuntimeException error) {
            retire.run();
            FileNotFoundException failure = new FileNotFoundException(error.getMessage());
            failure.initCause(error);
            throw failure;
        }
    }

    private static synchronized void release(Snapshot value) {
        value.readers--;
        openHandles--;
        prune(false, 0);
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) { throw new UnsupportedOperationException("Read-only snapshots"); }
    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] args) { throw new UnsupportedOperationException("Read-only snapshots"); }
    @Override
    public int delete(Uri uri, String selection, String[] args) { throw new UnsupportedOperationException("Read-only snapshots"); }
}

// FVPROV1 9124 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZDRmZDgxMmU1ZTRkYThlM2I5OWRlOGQ4MGZiMDQ0MDM2ZWYyZjVkNGNkNWExYzRmOTA3Njk1MDIyNWMyNGI2ZCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjdmZDMwNzQzOTE0MDBjZGUxMGY3YTVkYTg1YTBkMTNlMDM0ZThmZTIxOTI5NTUxZjk3MjM5MWY4YzA5YzFmYWY4MjBiY2VhMmMzZmIzOThjODBlMzQyODI5YzliODg2MDBhZDY1Y2JjNWRlNGNmNWVhOGI2Mzg4Njc5NjI4MDAifV19XQ== FVPROV1-END
