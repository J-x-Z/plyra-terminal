package org.plyra.glass.terminal;

import android.app.NativeActivity;
import android.content.Context;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Intent;
import android.content.res.AssetFileDescriptor;
import android.graphics.Rect;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Uri;
import android.os.Bundle;
import android.os.Build;
import android.os.ParcelFileDescriptor;
import android.text.Editable;
import android.text.InputType;
import android.text.Selection;
import android.text.SpannableStringBuilder;
import android.view.KeyEvent;
import android.view.GestureDetector;
import android.view.MotionEvent;
import android.view.SurfaceView;
import android.view.ViewConfiguration;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.ViewTreeObserver;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import java.nio.charset.StandardCharsets;
import java.lang.ref.WeakReference;
import java.io.IOException;
import java.net.InetAddress;
import java.util.concurrent.atomic.AtomicBoolean;

/** NativeActivity owns the GPU surface. This view supplies Android's IME contract. */
public final class GlassTerminalActivity extends NativeActivity {
    static {
        System.loadLibrary("glass_terminal_android");
    }

    private static native boolean nativeInput(int kind, byte[] utf8, int modifiers);
    private static native void nativeInputDiagnostics(boolean enabled);
    private static native boolean nativeConfigure(byte[] files, byte[] nativeLibraries,
            boolean bundledArch, byte[] androidApiName);
    private static native void nativeImportArch(int fd, long offset, long length,
            boolean bundled, byte[] dns);
    private static native void nativeArchOpenFailed(boolean bundled, byte[] message);
    private static native boolean nativePointer(int phase, double x, double y);
    private static native boolean nativeScroll(double x, double y, double dx, double dy);
    private static native boolean nativeViewport(int x, int y, int width, int height);
    private static native boolean nativeGlassState(int api, boolean available, boolean active,
            boolean powerSave, int thermalStatus, int radius, int reason);
    private static volatile WeakReference<GlassTerminalActivity> currentActivity = new WeakReference<>(null);
    private static final int PICK_ARCH_ARCHIVE = 271;
    private static final AtomicBoolean importingArch = new AtomicBoolean();

    private volatile boolean inputDiagnostics;
    private final java.util.concurrent.atomic.AtomicLong inputSequence =
            new java.util.concurrent.atomic.AtomicLong();
    private final TerminalInputState input = new TerminalInputState(this::sendInput);
    private InputView inputView;
    private boolean keyboardAllowed = true;
    private TerminalGlassWindow glassWindow;
    private TerminalAndroidApi androidApi;
    private GestureDetector gestures;
    private boolean commandGesture;
    private View surfaceView;
    private View viewportRoot;
    private int[] lastViewport;
    private final Runnable viewportUpdate = this::updateViewport;
    private final ViewTreeObserver.OnGlobalLayoutListener viewportLayout = this::updateViewport;
    private final int[] surfaceLocation = new int[2];
    private final TerminalTouchState touches = new TerminalTouchState(new TerminalTouchState.Sink() {
        public boolean pointer(int phase, double x, double y) {
            return nativePointer(phase, x, y);
        }
        public boolean scroll(double x, double y, double dx, double dy) {
            return nativeScroll(x, y, dx, dy);
        }
    });

    @Override
    public void onCreate(Bundle savedInstanceState) {
        currentActivity = new WeakReference<>(this);
        super.onCreate(savedInstanceState);
        inputDiagnostics = (getApplicationInfo().flags
                & android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE) != 0
                && getIntent().getBooleanExtra("glass_input_diagnostics", false);
        nativeInputDiagnostics(inputDiagnostics);
        gestures = new GestureDetector(this, new GestureDetector.SimpleOnGestureListener() {
            @Override
            public boolean onDown(MotionEvent event) { return true; }

            @Override
            public void onLongPress(MotionEvent event) {
                if (!touches.longPress()) return;
                commandGesture = true;
                input.cancel();
                refreshEditable();
                sendInput(2, "Commands", 0);
            }
        });
        gestures.setIsLongpressEnabled(true);
        glassWindow = new TerminalGlassWindow(this, GlassTerminalActivity::nativeGlassState);
        boolean bundledArch;
        try (AssetFileDescriptor archive = getAssets().openFd("arch-bootstrap.tar.gz")) {
            bundledArch = archive.getDeclaredLength() > 0;
        } catch (IOException error) {
            bundledArch = false;
        }
        String apiName = "";
        try {
            androidApi = new TerminalAndroidApi(this, () -> sendInput(2, "Settings", 0));
            apiName = androidApi.name();
        } catch (IOException error) {
            android.util.Log.w("GlassAndroidApi", "Android API is unavailable", error);
        }
        if (!nativeConfigure(getFilesDir().getAbsolutePath().getBytes(StandardCharsets.UTF_8),
                getApplicationInfo().nativeLibraryDir.getBytes(StandardCharsets.UTF_8),
                bundledArch, apiName.getBytes(StandardCharsets.UTF_8))) {
            if (androidApi != null) androidApi.close();
            throw new IllegalStateException("Android native application paths are unavailable");
        }
        Window window = getWindow();
        // Route keys through this Activity/InputConnection exactly once.
        // NativeActivity continues to own the surface and its lifecycle.
        window.takeInputQueue(null);
        window.clearFlags(WindowManager.LayoutParams.FLAG_FULLSCREEN
                | WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS
                | WindowManager.LayoutParams.FLAG_TRANSLUCENT_STATUS
                | WindowManager.LayoutParams.FLAG_TRANSLUCENT_NAVIGATION);
        window.getDecorView().setSystemUiVisibility(0);
        if (Build.VERSION.SDK_INT >= 30) window.setDecorFitsSystemWindows(true);
        if (Build.VERSION.SDK_INT >= 28) {
            WindowManager.LayoutParams attributes = window.getAttributes();
            attributes.layoutInDisplayCutoutMode =
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_NEVER;
            window.setAttributes(attributes);
        }
        window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
        findViewById(android.R.id.content).setFitsSystemWindows(true);
        inputView = new InputView(this);
        addContentView(inputView, new ViewGroup.LayoutParams(1, 1));
        inputView.requestFocus();
        viewportRoot = window.getDecorView();
        viewportRoot.getViewTreeObserver().addOnGlobalLayoutListener(viewportLayout);
        postViewportUpdate();
    }

    @Override
    protected void onStart() {
        super.onStart();
        if (glassWindow != null) glassWindow.start();
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (glassWindow != null) glassWindow.setResumed(true);
    }

    @Override
    public void onWindowFocusChanged(boolean focused) {
        super.onWindowFocusChanged(focused);
        if (inputDiagnostics) android.util.Log.i("GlassInput", "window_focus=" + focused);
        if (focused && inputView != null && keyboardAllowed) inputView.post(() -> showKeyboard());
        postViewportUpdate();
    }

    private void showKeyboard() {
        inputView.requestFocus();
        InputMethodManager manager = (InputMethodManager)
                getSystemService(Context.INPUT_METHOD_SERVICE);
        manager.showSoftInput(inputView, InputMethodManager.SHOW_IMPLICIT);
    }

    @Override
    protected void onPause() {
        if (glassWindow != null) glassWindow.setResumed(false);
        cancelLongPress();
        touches.cancel();
        input.cancel();
        sendInput(TerminalInputState.IME_DISABLED, "", 0);
        super.onPause();
    }

    @Override
    protected void onStop() {
        if (glassWindow != null) glassWindow.stop();
        super.onStop();
    }

    @Override
    protected void onDestroy() {
        if (androidApi != null) {
            androidApi.close();
            androidApi = null;
        }
        if (glassWindow != null) {
            glassWindow.stop();
            glassWindow = null;
        }
        if (viewportRoot != null) {
            ViewTreeObserver observer = viewportRoot.getViewTreeObserver();
            if (observer.isAlive()) observer.removeOnGlobalLayoutListener(viewportLayout);
            viewportRoot.removeCallbacks(viewportUpdate);
            viewportRoot = null;
        }
        if (currentActivity.get() == this) currentActivity = new WeakReference<>(null);
        super.onDestroy();
    }

    public static void configureGlass(boolean enabled, float radius) {
        if (!Float.isFinite(radius)) {
            throw new IllegalArgumentException("Window blur radius must be finite");
        }
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) throw new IllegalStateException("Android Activity is unavailable");
        activity.runOnUiThread(() -> {
            if (activity.glassWindow != null && !activity.isDestroyed()) {
                activity.glassWindow.configure(enabled, radius);
            }
        });
    }

    public static void copyText(String text) {
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) return;
        activity.runOnUiThread(() -> {
            ClipboardManager clipboard = (ClipboardManager)
                    activity.getSystemService(Context.CLIPBOARD_SERVICE);
            if (clipboard != null) clipboard.setPrimaryClip(ClipData.newPlainText("Terminal", text));
        });
    }

    public static void openUrl(String target) {
        Uri uri = Uri.parse(target);
        if (target.length() > 8192 || !("https".equalsIgnoreCase(uri.getScheme())
                || "http".equalsIgnoreCase(uri.getScheme())) || uri.getHost() == null
                || uri.getHost().isEmpty()) {
            throw new IllegalArgumentException("Only HTTP(S) URLs with a host may be opened");
        }
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) throw new IllegalStateException("Android Activity is unavailable");
        activity.runOnUiThread(() -> activity.openExternal(
                new Intent(Intent.ACTION_VIEW, uri).addCategory(Intent.CATEGORY_BROWSABLE), null));
    }

    public static void openManagedFile(String name, byte[] contents) {
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) throw new IllegalStateException("Android Activity is unavailable");
        Uri uri = ManagedObjectProvider.snapshot(name, contents);
        Intent intent = new Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, ManagedObjectProvider.mimeType(name))
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        intent.setClipData(ClipData.newRawUri(name, uri));
        activity.runOnUiThread(() -> activity.openExternal(intent, uri));
    }

    private void openExternal(Intent intent, Uri snapshot) {
        try {
            startActivity(intent);
        } catch (android.content.ActivityNotFoundException | SecurityException error) {
            if (snapshot != null) ManagedObjectProvider.discard(snapshot);
            android.widget.Toast.makeText(this, "Cannot open object: " + error.getMessage(),
                    android.widget.Toast.LENGTH_LONG).show();
        }
    }

    public static void requestPaste() {
        GlassTerminalActivity activity = currentActivity.get();
        if (activity != null) activity.runOnUiThread(() -> activity.pasteClipboard());
    }

    public static void pickArchArchive() {
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) throw new IllegalStateException("Android Activity is unavailable");
        activity.runOnUiThread(() -> {
            if (importingArch.get()) {
                activity.archToast("Arch Linux import is in progress");
                return;
            }
            Intent picker = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                    .addCategory(Intent.CATEGORY_OPENABLE)
                    .setType("*/*");
            try {
                activity.startActivityForResult(picker, PICK_ARCH_ARCHIVE);
            } catch (android.content.ActivityNotFoundException error) {
                activity.archToast("Android's file picker is unavailable");
            }
        });
    }

    public static void prepareBundledArch() {
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) throw new IllegalStateException("Android Activity is unavailable");
        activity.runOnUiThread(() -> {
            if (!importingArch.compareAndSet(false, true)) {
                archOpenFailed(true, "Arch Linux import is already in progress");
                return;
            }
            activity.archToast("Preparing Arch Linux. Keep Plyra Terminal open.");
            Context application = activity.getApplicationContext();
            new Thread(() -> {
                try (AssetFileDescriptor archive =
                        application.getAssets().openFd("arch-bootstrap.tar.gz")) {
                    long offset = archive.getStartOffset();
                    long length = archive.getDeclaredLength();
                    if (offset < 0 || length <= 0) {
                        throw new IOException("The packaged Arch archive has no valid bounds");
                    }
                    byte[] dns = archDns(application);
                    // Read the stored APK asset directly; Rust owns the detached FD.
                    nativeImportArch(archive.getParcelFileDescriptor().detachFd(),
                            offset, length, true, dns);
                } catch (IOException | RuntimeException error) {
                    archOpenFailed(true, "Cannot open packaged Arch archive: " + error.getMessage());
                } finally {
                    importingArch.set(false);
                }
            }, "glass-arch-bootstrap").start();
        });
    }

    private static void archOpenFailed(boolean bundled, String message) {
        if (message.length() > 512) message = message.substring(0, 512);
        nativeArchOpenFailed(bundled, message.getBytes(StandardCharsets.UTF_8));
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != PICK_ARCH_ARCHIVE || result != RESULT_OK || data == null) return;
        Uri archive = data.getData();
        if (archive == null || !"content".equals(archive.getScheme())) {
            archToast("Choose an Arch Linux ARM tar.gz file");
            return;
        }
        if (!importingArch.compareAndSet(false, true)) {
            archToast("Arch Linux import is in progress");
            return;
        }
        archToast("Importing Arch Linux. Keep Plyra Terminal open.");
        Context application = getApplicationContext();
        new Thread(() -> {
            try (ParcelFileDescriptor descriptor =
                    application.getContentResolver().openFileDescriptor(archive, "r")) {
                if (descriptor == null) throw new IOException("The provider returned no file");
                // Ownership of the detached descriptor is transferred to Rust.
                byte[] dns = archDns(application);
                nativeImportArch(descriptor.detachFd(), 0, -1, false, dns);
            } catch (IOException | RuntimeException error) {
                archOpenFailed(false, "Cannot read Arch archive: " + error.getMessage());
            } finally {
                importingArch.set(false);
            }
        }, "glass-arch-import").start();
    }

    private void archToast(String message) {
        android.widget.Toast.makeText(this, message, android.widget.Toast.LENGTH_LONG).show();
    }

    private static byte[] archDns(Context context) {
        ConnectivityManager network = context.getSystemService(ConnectivityManager.class);
        LinkProperties link = network == null ? null
                : network.getLinkProperties(network.getActiveNetwork());
        StringBuilder addresses = new StringBuilder();
        boolean strictPrivateDns = link != null && Build.VERSION.SDK_INT >= 28
                && link.getPrivateDnsServerName() != null;
        if (link != null && !strictPrivateDns) {
            int count = 0;
            for (InetAddress address : link.getDnsServers()) {
                if (address.isLinkLocalAddress() || address.isLoopbackAddress()) continue;
                if (count++ == 3) break;
                String host = address.getHostAddress();
                int scope = host.indexOf('%');
                addresses.append(scope == -1 ? host : host.substring(0, scope)).append('\n');
            }
        }
        return addresses.toString().getBytes(StandardCharsets.UTF_8);
    }

    public static void requestKeyboard(boolean visible) {
        GlassTerminalActivity activity = currentActivity.get();
        if (activity == null) return;
        activity.runOnUiThread(() -> {
            activity.keyboardAllowed = visible;
            if (activity.inputView == null) return;
            if (visible) activity.showKeyboard();
            else {
                InputMethodManager manager = (InputMethodManager)
                        activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                manager.hideSoftInputFromWindow(activity.inputView.getWindowToken(), 0);
            }
            activity.postViewportUpdate();
        });
    }

    private boolean pasteClipboard() {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboard == null || !clipboard.hasPrimaryClip()) return false;
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) return false;
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        if (text == null || text.length() > TerminalInputState.MAX_CHARS) return false;
        input.cancel();
        refreshEditable();
        return sendInput(5, text.toString(), 0);
    }

    private SurfaceView findSurface(View view) {
        if (view instanceof SurfaceView) return (SurfaceView) view;
        if (view instanceof ViewGroup) {
            ViewGroup group = (ViewGroup) view;
            for (int index = 0; index < group.getChildCount(); index++) {
                SurfaceView found = findSurface(group.getChildAt(index));
                if (found != null) return found;
            }
        }
        return null;
    }

    private boolean touchPoint(MotionEvent event, int index, int phase) {
        double x = event.getX(index) + event.getRawX() - event.getX() - surfaceLocation[0];
        double y = event.getY(index) + event.getRawY() - event.getY() - surfaceLocation[1];
        if (inputDiagnostics && phase != TerminalTouchState.MOVED) {
            android.util.Log.i("GlassInput", "pointer=" + phase + "," + x + "," + y);
        }
        return touches.event(phase, event.getPointerId(index), x, y);
    }

    private View currentSurfaceView() {
        if (surfaceView == null || !surfaceView.isAttachedToWindow()) {
            surfaceView = findSurface(getWindow().getDecorView());
            // NativeActivity can own the Window surface without a SurfaceView child.
            if (surfaceView == null) surfaceView = getWindow().getDecorView();
        }
        return surfaceView;
    }

    private void postViewportUpdate() {
        if (viewportRoot == null) return;
        viewportRoot.removeCallbacks(viewportUpdate);
        viewportRoot.post(viewportUpdate);
    }

    private void updateViewport() {
        if (viewportRoot == null || isFinishing() || isDestroyed()) return;
        View surface = currentSurfaceView();
        int width = surface.getWidth();
        int height = surface.getHeight();
        if (width <= 0 || height <= 0) return;
        Rect visible = new Rect();
        viewportRoot.getWindowVisibleDisplayFrame(visible);
        surface.getLocationOnScreen(surfaceLocation);
        int left = Math.max(0, Math.min(width, visible.left - surfaceLocation[0]));
        int top = Math.max(0, Math.min(height, visible.top - surfaceLocation[1]));
        int right = Math.max(0, Math.min(width, visible.right - surfaceLocation[0]));
        int bottom = Math.max(0, Math.min(height, visible.bottom - surfaceLocation[1]));
        int visibleWidth = right - left;
        int visibleHeight = bottom - top;
        if (visibleWidth <= 0 || visibleHeight <= 0) return;
        if (lastViewport != null && lastViewport[0] == left && lastViewport[1] == top
                && lastViewport[2] == visibleWidth && lastViewport[3] == visibleHeight) return;
        if (nativeViewport(left, top, visibleWidth, visibleHeight)) {
            lastViewport = new int[] { left, top, visibleWidth, visibleHeight };
            if (inputDiagnostics) android.util.Log.i("GlassInput", "viewport=" + left + "," + top
                    + "," + visibleWidth + "," + visibleHeight);
        }
    }

    private void cancelLongPress() {
        if (gestures == null) return;
        long now = android.os.SystemClock.uptimeMillis();
        MotionEvent cancel = MotionEvent.obtain(now, now, MotionEvent.ACTION_CANCEL, 0, 0, 0);
        gestures.onTouchEvent(cancel);
        cancel.recycle();
    }

    @Override
    public boolean dispatchTouchEvent(MotionEvent event) {
        currentSurfaceView();
        surfaceView.getLocationOnScreen(surfaceLocation);
        int action = event.getActionMasked();
        if (action == MotionEvent.ACTION_DOWN) {
            commandGesture = false;
            touches.cancel();
            input.cancel();
            refreshEditable();
        }
        if (event.getPointerCount() > 1 || event.getToolType(0) != MotionEvent.TOOL_TYPE_FINGER) {
            cancelLongPress();
        } else if (gestures != null) {
            gestures.onTouchEvent(event);
        }
        if (commandGesture) return true;
        boolean accepted = true;
        switch (action) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN:
                accepted = touchPoint(event, event.getActionIndex(), TerminalTouchState.STARTED);
                break;
            case MotionEvent.ACTION_MOVE:
                for (int index = 0; index < event.getPointerCount(); index++) {
                    accepted &= touchPoint(event, index, TerminalTouchState.MOVED);
                }
                break;
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_POINTER_UP:
                accepted = touchPoint(event, event.getActionIndex(), TerminalTouchState.ENDED);
                if (action == MotionEvent.ACTION_UP && inputView != null && keyboardAllowed) {
                    showKeyboard();
                }
                break;
            case MotionEvent.ACTION_CANCEL:
                touches.cancel();
                break;
            default:
                return super.dispatchTouchEvent(event);
        }
        return accepted || super.dispatchTouchEvent(event);
    }

    @Override
    public boolean dispatchGenericMotionEvent(MotionEvent event) {
        if (event.getActionMasked() == MotionEvent.ACTION_SCROLL && surfaceView != null) {
            surfaceView.getLocationOnScreen(surfaceLocation);
            ViewConfiguration configuration = ViewConfiguration.get(this);
            return nativeScroll(event.getRawX() - surfaceLocation[0], event.getRawY() - surfaceLocation[1],
                    event.getAxisValue(MotionEvent.AXIS_HSCROLL) * configuration.getScaledHorizontalScrollFactor(),
                    event.getAxisValue(MotionEvent.AXIS_VSCROLL) * configuration.getScaledVerticalScrollFactor());
        }
        return super.dispatchGenericMotionEvent(event);
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        int code = event.getKeyCode();
        if (code == KeyEvent.KEYCODE_BACK || event.isSystem()) {
            return super.dispatchKeyEvent(event);
        }
        if (KeyEvent.isModifierKey(code)) return true;
        String name = keyName(code);
        int unicode = event.getUnicodeChar(event.getMetaState()
                & ~(KeyEvent.META_CTRL_MASK | KeyEvent.META_ALT_MASK | KeyEvent.META_META_MASK));
        if (name == null && unicode == 0 && event.getCharacters() == null) {
            return super.dispatchKeyEvent(event);
        }
        // Consume releases too so NativeActivity cannot send the same key via winit.
        if (event.getAction() == KeyEvent.ACTION_UP) return true;
        if (event.getAction() == KeyEvent.ACTION_MULTIPLE && event.getCharacters() != null) {
            input.commit(event.getCharacters());
            refreshEditable();
            return true;
        }
        if (event.getAction() != KeyEvent.ACTION_DOWN) return true;
        int modifiers = (event.isShiftPressed() ? 1 : 0)
                | (event.isAltPressed() ? 2 : 0) | (event.isCtrlPressed() ? 4 : 0);
        if (name != null) {
            input.key(name, modifiers);
        } else if (unicode != 0 && Character.isValidCodePoint(unicode)) {
            String text = new String(Character.toChars(unicode));
            if (event.isCtrlPressed() || event.isAltPressed()) input.key(text, modifiers);
            else input.commit(text);
        }
        refreshEditable();
        return true;
    }

    private void refreshEditable() {
        if (inputView != null && inputView.connection != null) inputView.connection.refresh();
    }

    private boolean sendInput(int kind, String text, int modifiers) {
        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        boolean queued = nativeInput(kind, bytes, modifiers);
        if (inputDiagnostics) {
            android.util.Log.i("GlassInput", "seq=" + inputSequence.incrementAndGet()
                    + " kind=" + kind + " codepoints=" + text.codePointCount(0, text.length())
                    + " bytes=" + bytes.length + " queued=" + queued);
        }
        return queued;
    }

    private static String keyName(int code) {
        switch (code) {
            case KeyEvent.KEYCODE_ENTER: case KeyEvent.KEYCODE_NUMPAD_ENTER: return "Enter";
            case KeyEvent.KEYCODE_DEL: return "Backspace";
            case KeyEvent.KEYCODE_FORWARD_DEL: return "Delete";
            case KeyEvent.KEYCODE_TAB: return "Tab";
            case KeyEvent.KEYCODE_ESCAPE: return "Escape";
            case KeyEvent.KEYCODE_DPAD_UP: return "Up";
            case KeyEvent.KEYCODE_DPAD_DOWN: return "Down";
            case KeyEvent.KEYCODE_DPAD_LEFT: return "Left";
            case KeyEvent.KEYCODE_DPAD_RIGHT: return "Right";
            case KeyEvent.KEYCODE_MOVE_HOME: return "Home";
            case KeyEvent.KEYCODE_MOVE_END: return "End";
            case KeyEvent.KEYCODE_PAGE_UP: return "PageUp";
            case KeyEvent.KEYCODE_PAGE_DOWN: return "PageDown";
            case KeyEvent.KEYCODE_INSERT: return "Insert";
            default:
                if (code >= KeyEvent.KEYCODE_F1 && code <= KeyEvent.KEYCODE_F12) {
                    return "F" + (code - KeyEvent.KEYCODE_F1 + 1);
                }
                return null;
        }
    }

    private final class InputView extends View {
        TerminalConnection connection;

        InputView(Context context) {
            super(context);
            setFocusable(true);
            setFocusableInTouchMode(true);
            setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE
                    | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            info.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN
                    | EditorInfo.IME_ACTION_NONE;
            info.initialSelStart = input.cursor();
            info.initialSelEnd = input.cursor();
            connection = Build.VERSION.SDK_INT >= 33
                    ? new AttributeTerminalConnection(this) : new TerminalConnection(this);
            sendInput(TerminalInputState.IME_ENABLED, "", 0);
            return connection;
        }
    }

    private class TerminalConnection extends BaseInputConnection {
        private final Editable editable = new SpannableStringBuilder();
        private final View target;
        private int batchDepth;
        private boolean selectionPending;
        private int lastSelectionStart = Integer.MIN_VALUE;
        private int lastSelectionEnd = Integer.MIN_VALUE;
        private int lastComposingStart = Integer.MIN_VALUE;
        private int lastComposingEnd = Integer.MIN_VALUE;

        TerminalConnection(View view) {
            super(view, true);
            target = view;
            refresh();
        }

        void refresh() {
            beginBatchEdit();
            editable.replace(0, editable.length(), input.text());
            BaseInputConnection.removeComposingSpans(editable);
            Selection.setSelection(editable, input.cursor());
            if (input.composingStart() >= 0) {
                super.setComposingRegion(input.composingStart(), input.composingEnd());
            }
            selectionPending = true;
            endBatchEdit();
        }

        private void reportSelection() {
            if (!selectionPending) return;
            selectionPending = false;
            int start = Selection.getSelectionStart(editable);
            int end = Selection.getSelectionEnd(editable);
            int composingStart = BaseInputConnection.getComposingSpanStart(editable);
            int composingEnd = BaseInputConnection.getComposingSpanEnd(editable);
            if (start == lastSelectionStart && end == lastSelectionEnd
                    && composingStart == lastComposingStart && composingEnd == lastComposingEnd) return;
            InputMethodManager manager = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
            if (manager != null) manager.updateSelection(target, start, end, composingStart, composingEnd);
            lastSelectionStart = start;
            lastSelectionEnd = end;
            lastComposingStart = composingStart;
            lastComposingEnd = composingEnd;
            if (inputDiagnostics) {
                android.util.Log.i("GlassInput", "selection=" + start + "," + end
                        + " composing=" + composingStart + "," + composingEnd + " batch=" + batchDepth);
            }
        }

        @Override
        public boolean beginBatchEdit() {
            batchDepth++;
            return true;
        }

        @Override
        public boolean endBatchEdit() {
            if (batchDepth == 0) return false;
            batchDepth--;
            if (batchDepth == 0) reportSelection();
            return batchDepth > 0;
        }

        @Override
        public Editable getEditable() {
            return editable;
        }

        @Override
        public boolean setComposingText(CharSequence text, int cursor) {
            boolean accepted = input.setComposing(boundedText(text), cursor);
            refresh();
            return accepted;
        }

        @Override
        public boolean setComposingRegion(int start, int end) {
            input.setComposingRegion(start, end);
            BaseInputConnection.removeComposingSpans(editable);
            if (input.composingStart() >= 0) {
                super.setComposingRegion(input.composingStart(), input.composingEnd());
            }
            // This operation changes styling, not text or cursor. The IME
            // already supplied the region, so do not echo updateSelection.
            return true;
        }

        @Override
        public boolean commitText(CharSequence text, int cursor) {
            boolean accepted = input.commit(boundedText(text));
            refresh();
            return accepted;
        }

        @Override
        public boolean finishComposingText() {
            boolean accepted = input.finish();
            refresh();
            return accepted;
        }

        @Override
        public boolean deleteSurroundingText(int before, int after) {
            boolean accepted = input.delete(before, after, false);
            refresh();
            return accepted;
        }

        @Override
        public boolean deleteSurroundingTextInCodePoints(int before, int after) {
            boolean accepted = input.delete(before, after, true);
            refresh();
            return accepted;
        }

        @Override
        public boolean setSelection(int start, int end) {
            boolean accepted = input.setSelection(start, end);
            refresh();
            return accepted;
        }

        @Override
        public boolean sendKeyEvent(KeyEvent event) {
            return dispatchKeyEvent(event);
        }

        @Override
        public boolean performEditorAction(int action) {
            if (inputDiagnostics) android.util.Log.i("GlassInput", "editor_action=" + action);
            boolean accepted = input.finish() && input.key("Enter", 0);
            refresh();
            return accepted;
        }

        @Override
        public boolean performContextMenuAction(int action) {
            switch (action) {
                case android.R.id.copy: return sendInput(6, "", 0);
                case android.R.id.cut: return sendInput(7, "", 0);
                case android.R.id.paste: return pasteClipboard();
                case android.R.id.selectAll: return sendInput(8, "", 0);
                default: return false;
            }
        }

        private String boundedText(CharSequence text) {
            if (text == null || text.length() > TerminalInputState.MAX_CHARS) return null;
            return text.toString();
        }
    }

    // Keep API 33's TextAttribute references out of the connection loaded on API 26–32.
    private final class AttributeTerminalConnection extends TerminalConnection {
        AttributeTerminalConnection(View view) { super(view); }

        @Override
        public boolean commitText(CharSequence text, int cursor,
                android.view.inputmethod.TextAttribute attributes) {
            if (inputDiagnostics) android.util.Log.i("GlassInput", "method=commitText33");
            return commitText(text, cursor);
        }

        @Override
        public boolean setComposingText(CharSequence text, int cursor,
                android.view.inputmethod.TextAttribute attributes) {
            if (inputDiagnostics) android.util.Log.i("GlassInput", "method=setComposingText33");
            return setComposingText(text, cursor);
        }

        @Override
        public boolean setComposingRegion(int start, int end,
                android.view.inputmethod.TextAttribute attributes) {
            return setComposingRegion(start, end);
        }

        @Override
        public boolean replaceText(int start, int end, CharSequence text, int cursor,
                android.view.inputmethod.TextAttribute attributes) {
            if (inputDiagnostics) android.util.Log.i("GlassInput", "method=replaceText34");
            String replacement = text == null || text.length() > TerminalInputState.MAX_CHARS
                    ? null : text.toString();
            boolean accepted = input.replace(start, end, replacement);
            refresh();
            return accepted;
        }
    }
}

// FVPROV1 36621 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTc3MzMwYWI0NDcyYzg3M2ZlYjE4NzVhOTRjZWI1ZjhhYjQ3NTg2OGUxOTVmNzIwODg4NTQ2YzgzZDc3MzRiMCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzdlMDZjNTIyM2M2NGFiMWI0OTQxNGRlNDIyMzk2MDUyZDZhY2JhZDU4NGYzZDZmZjQ5MWYwZWU1MzViZmNlYmRjZDkzZDAyMTEyMGFjYWJlZGEwZDBhODJlNDdjYTYxYWY5OGJiY2MwMjM0YWFjNmFlMWJhOTNjMTM0YjNlMDcifV19XQ== FVPROV1-END
