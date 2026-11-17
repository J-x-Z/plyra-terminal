package org.plyra.glass.terminal;

import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.net.Credentials;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.BatteryManager;
import android.os.Handler;
import android.os.Looper;
import android.os.Process;
import android.os.SystemClock;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/** Small, same-UID Android API for this terminal's native userspace session. */
public final class TerminalAndroidApi implements AutoCloseable {
    private static final int MAX_REQUEST_BYTES = 64;
    private static final int REQUEST_TIMEOUT_MS = 2000;
    private static final int SETTINGS_TIMEOUT_MS = 1500;
    private static final int MAX_RESPONSE_BYTES = 512;

    private final Context context;
    private final Runnable showSettings;
    private final Handler mainHandler = new Handler(Looper.getMainLooper());
    private final Object clientLock = new Object();
    private final String socketName;
    private final LocalServerSocket server;
    private final Thread worker;
    private volatile boolean closed;
    private LocalSocket activeClient;
    private volatile Runnable pendingSettings;

    /** Starts one serial daemon; name() is the raw abstract Unix socket name. */
    public TerminalAndroidApi(Context context, Runnable showSettings) throws IOException {
        Objects.requireNonNull(context, "context");
        Context application = context.getApplicationContext();
        this.context = application == null ? context : application;
        this.showSettings = Objects.requireNonNull(showSettings, "showSettings");
        socketName = "glass.api." + UUID.randomUUID().toString().replace("-", "");
        server = new LocalServerSocket(socketName);
        worker = new Thread(this::serve, "GlassAndroidApi");
        worker.setDaemon(true);
        worker.start();
    }

    public String name() {
        return socketName;
    }

    private void serve() {
        try {
            while (!closed) {
                LocalSocket client = server.accept();
                synchronized (clientLock) {
                    if (closed) {
                        closeClient(client);
                        break;
                    }
                    activeClient = client;
                }
                try {
                    handle(client);
                } catch (IOException | RuntimeException ignored) {
                    // Each failed or disconnected client owns only this socket.
                } finally {
                    synchronized (clientLock) {
                        if (activeClient == client) activeClient = null;
                    }
                    closeClient(client);
                }
            }
        } catch (IOException ignored) {
            // close() interrupts accept. Never spin on a broken listener.
        } finally {
            close();
        }
    }

    private void handle(LocalSocket client) throws IOException {
        Credentials peer = client.getPeerCredentials();
        if (peer == null || peer.getUid() != Process.myUid()) {
            reply(client, error("peer_uid_mismatch"));
            return;
        }
        String action;
        try {
            action = readAction(client);
        } catch (IOException malformed) {
            reply(client, error("invalid_or_timed_out_request"));
            return;
        }
        if (closed) return;
        if ("battery".equals(action)) {
            reply(client, battery());
        } else if ("settings".equals(action)) {
            reply(client, settings());
        } else {
            reply(client, error("unsupported_action"));
        }
    }

    private String readAction(LocalSocket client) throws IOException {
        long deadline = SystemClock.elapsedRealtime() + REQUEST_TIMEOUT_MS;
        InputStream stream = client.getInputStream();
        ByteArrayOutputStream line = new ByteArrayOutputStream(MAX_REQUEST_BYTES);
        for (int count = 0; count < MAX_REQUEST_BYTES; count++) {
            long remaining = deadline - SystemClock.elapsedRealtime();
            if (remaining <= 0 || closed) throw new IOException("request deadline");
            client.setSoTimeout((int) remaining);
            int value = stream.read();
            if (value == '\n') return new String(line.toByteArray(), StandardCharsets.US_ASCII);
            if (value < 0x20 || value > 0x7e) throw new IOException("invalid request byte");
            line.write(value);
        }
        throw new IOException("request line too long");
    }

    private String battery() {
        try {
            Intent state = context.registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
            if (state == null) return error("battery_unavailable");
            return "{\"ok\":true,\"action\":\"battery\",\"level\":"
                    + state.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
                    + ",\"scale\":" + state.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
                    + ",\"status\":" + state.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
                    + ",\"temperature\":" + state.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, -1)
                    + ",\"temperature_unit\":\"deci_celsius\"}";
        } catch (RuntimeException unavailable) {
            return error("battery_unavailable");
        }
    }

    private String settings() {
        CountDownLatch finished = new CountDownLatch(1);
        AtomicBoolean success = new AtomicBoolean();
        AtomicBoolean cancelled = new AtomicBoolean();
        Runnable callback = () -> {
            try {
                if (!closed && !cancelled.get()) {
                    showSettings.run();
                    success.set(true);
                }
            } catch (RuntimeException ignored) {
                // Callback failure is a bounded API error, not a worker crash.
            } finally {
                finished.countDown();
            }
        };
        pendingSettings = callback;
        boolean completed = false;
        try {
            if (closed || !mainHandler.post(callback)) return error("settings_unavailable");
            completed = finished.await(SETTINGS_TIMEOUT_MS, TimeUnit.MILLISECONDS);
            return completed && success.get()
                    ? "{\"ok\":true,\"action\":\"settings\"}" : error("settings_unavailable");
        } catch (InterruptedException interrupted) {
            Thread.currentThread().interrupt();
            return error("settings_unavailable");
        } finally {
            if (!completed) {
                cancelled.set(true);
                mainHandler.removeCallbacks(callback);
            }
            pendingSettings = null;
        }
    }

    private static String error(String code) {
        // Every caller supplies a fixed literal, never request or exception text.
        return "{\"ok\":false,\"error\":\"" + code + "\"}";
    }

    private void reply(LocalSocket client, String json) throws IOException {
        byte[] bytes = (json + "\n").getBytes(StandardCharsets.US_ASCII);
        if (bytes.length > MAX_RESPONSE_BYTES) throw new IOException("response length");
        client.setSoTimeout(REQUEST_TIMEOUT_MS);
        OutputStream stream = client.getOutputStream();
        stream.write(bytes);
        stream.flush();
    }

    private static void closeClient(LocalSocket client) {
        try {
            client.close();
        } catch (IOException ignored) {
            // Idempotent teardown; the daemon retains no client after close.
        }
    }

    @Override
    public void close() {
        LocalSocket client;
        synchronized (clientLock) {
            if (closed) return;
            closed = true;
            client = activeClient;
            activeClient = null;
        }
        Runnable callback = pendingSettings;
        if (callback != null) mainHandler.removeCallbacks(callback);
        try {
            server.close();
        } catch (IOException ignored) {
            // Already closed or interrupted listener.
        }
        if (client != null) closeClient(client);
        worker.interrupt();
    }
}

// FVPROV1 8562 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNjkwZmI4YjU0OTJlZjU5MmRiMWRlNGMyMDFiN2UxM2ZjNjM3NWYyMDdkYzM2YjNhZWM2MGYyZTIzNWZmNWU4ZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZDM4YWE0Y2M0NjI0NDgxZWEwMGY3YzZjYWYwNTIzMmIwYzc1ZjEzYmYzNjIwM2U4Zjg2MTk1YTM2NzM2OTFhODllMDM0ZjlmMWU2MDZmYmE5NDRiNDg1YjQ5N2MwYjE1ZmEzYjg2MDhiMWE4MmYwZTc3NGU5NjMwM2MzOWZlMGUifV19XQ== FVPROV1-END
