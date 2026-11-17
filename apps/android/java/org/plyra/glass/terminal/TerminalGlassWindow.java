package org.plyra.glass.terminal;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.graphics.PixelFormat;
import android.os.Build;
import android.os.PowerManager;
import android.util.Log;
import android.view.Window;
import android.view.WindowManager;
import java.util.function.Consumer;

/** Owns Android's full-window compositor blur and its event-driven capability state. */
final class TerminalGlassWindow {
    interface StateSink {
        boolean changed(int api, boolean available, boolean active, boolean powerSave,
                int thermalStatus, int radius, int reason);
    }

    private static final int DISABLED = 0;
    private static final int BLUR_REQUESTED = 1;
    private static final int SYSTEM_DISABLED = 2;
    private static final int UNSUPPORTED_API = 3;
    private static final int POWER_SAVE = 4;
    private static final int THERMAL_LIMIT = 5;
    private static final int INACTIVE = 6;
    private static final int PLATFORM_ERROR = 7;
    private static final int CLEAR = 8;

    private final Activity activity;
    private final Window window;
    private final WindowManager manager;
    private final PowerManager power;
    private final StateSink sink;
    private boolean started;
    private boolean resumed;
    private boolean requested;
    private int requestedRadius;
    private boolean available;
    private boolean powerSave;
    private int thermalStatus;
    private boolean platformError;
    private boolean blurListenerRegistered;
    private boolean thermalListenerRegistered;
    private boolean powerReceiverRegistered;
    private int appliedRadius = -1;
    private boolean appliedBlur;
    private PowerManager.OnThermalStatusChangedListener thermalListener;

    private final Consumer<Boolean> blurListener = enabled -> {
        available = enabled;
        apply();
    };
    private final BroadcastReceiver powerReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            powerSave = power != null && power.isPowerSaveMode();
            apply();
        }
    };

    TerminalGlassWindow(Activity activity, StateSink sink) {
        this.activity = activity;
        this.window = activity.getWindow();
        this.manager = activity.getWindowManager();
        this.power = activity.getSystemService(PowerManager.class);
        this.sink = sink;
        window.setFormat(PixelFormat.TRANSLUCENT);
    }

    void start() {
        if (started) return;
        started = true;
        platformError = false;
        powerSave = power != null && power.isPowerSaveMode();
        try {
            if (Build.VERSION.SDK_INT >= 31) {
                available = manager.isCrossWindowBlurEnabled();
                manager.addCrossWindowBlurEnabledListener(activity.getMainExecutor(), blurListener);
                blurListenerRegistered = true;
            }
            if (power != null && Build.VERSION.SDK_INT >= 29) {
                thermalStatus = power.getCurrentThermalStatus();
                thermalListener = status -> {
                    thermalStatus = status;
                    apply();
                };
                power.addThermalStatusListener(activity.getMainExecutor(), thermalListener);
                thermalListenerRegistered = true;
            }
            IntentFilter filter = new IntentFilter(PowerManager.ACTION_POWER_SAVE_MODE_CHANGED);
            if (Build.VERSION.SDK_INT >= 33) {
                activity.registerReceiver(powerReceiver, filter, Context.RECEIVER_NOT_EXPORTED);
            } else {
                activity.registerReceiver(powerReceiver, filter);
            }
            powerReceiverRegistered = true;
        } catch (RuntimeException error) {
            platformError = true;
            Log.w("GlassMaterial", "Cannot register compositor state listener", error);
        }
        apply();
    }

    void setResumed(boolean value) {
        resumed = value;
        apply();
    }

    void configure(boolean enabled, float radius) {
        requested = enabled;
        requestedRadius = Math.round(Math.max(0.0f, Math.min(100.0f, radius)));
        apply();
    }

    private void apply() {
        int reason;
        if (!requested) reason = DISABLED;
        else if (!started || !resumed) reason = INACTIVE;
        else if (platformError) reason = PLATFORM_ERROR;
        else if (powerSave) reason = POWER_SAVE;
        else if (thermalStatus >= PowerManager.THERMAL_STATUS_MODERATE) reason = THERMAL_LIMIT;
        else if (Build.VERSION.SDK_INT < 31) reason = UNSUPPORTED_API;
        else if (!available) reason = SYSTEM_DISABLED;
        else reason = requestedRadius == 0 ? CLEAR : BLUR_REQUESTED;
        // A compositor without cross-window blur can still transmit the tinted
        // GPU surface. Only the blur request depends on that capability.
        boolean active = reason == BLUR_REQUESTED || reason == CLEAR
                || reason == SYSTEM_DISABLED || reason == UNSUPPORTED_API;
        int radius = reason == BLUR_REQUESTED ? requestedRadius : 0;
        boolean blur = radius > 0;
        if (Build.VERSION.SDK_INT >= 31
                && (radius != appliedRadius || blur != appliedBlur)) {
            try {
                WindowManager.LayoutParams attributes = window.getAttributes();
                if (blur) attributes.flags |= WindowManager.LayoutParams.FLAG_BLUR_BEHIND;
                else attributes.flags &= ~WindowManager.LayoutParams.FLAG_BLUR_BEHIND;
                attributes.setBlurBehindRadius(radius);
                window.setAttributes(attributes);
                appliedRadius = radius;
                appliedBlur = blur;
            } catch (RuntimeException error) {
                platformError = true;
                active = false;
                radius = 0;
                reason = PLATFORM_ERROR;
                Log.w("GlassMaterial", "Cannot apply compositor blur", error);
            }
        }
        if (sink.changed(Build.VERSION.SDK_INT, available, active, powerSave,
                thermalStatus, radius, reason)) {
            Log.i("GlassMaterial", "api=" + Build.VERSION.SDK_INT + " available=" + available
                    + " translucency=" + active + " blur_radius_px=" + radius
                    + " power_save=" + powerSave + " thermal_status=" + thermalStatus
                    + " state=" + reason);
        }
    }

    void stop() {
        resumed = false;
        started = false;
        apply();
        if (blurListenerRegistered) {
            blurListenerRegistered = false;
            try {
                manager.removeCrossWindowBlurEnabledListener(blurListener);
            } catch (RuntimeException error) {
                Log.w("GlassMaterial", "Cannot remove compositor state listener", error);
            }
        }
        if (thermalListenerRegistered) {
            thermalListenerRegistered = false;
            try {
                power.removeThermalStatusListener(thermalListener);
            } catch (RuntimeException error) {
                Log.w("GlassMaterial", "Cannot remove thermal state listener", error);
            }
            thermalListener = null;
        }
        if (powerReceiverRegistered) {
            powerReceiverRegistered = false;
            try {
                activity.unregisterReceiver(powerReceiver);
            } catch (RuntimeException error) {
                Log.w("GlassMaterial", "Cannot remove power state receiver", error);
            }
        }
    }
}

// FVPROV1 7682 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOTI0ZWM5ZDllZjAwMjBiZjMwM2RhNzAwYjMwY2Q5ODg1NTU3ZjUxYWEwNjlhNDJmNzNiMTJlOTIwYTU0MWNlMSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZGUxNDdiN2JlYzIxYTQ2MzRhNTE2MWZjYTQzMzE3MWQyYTVkYTNiZjQ5NjEzMGY3MzY3YjVmNDkyOWQ2M2VkNjgwMTNkM2Y0MDYwNTg1N2JlZjA4OGYzNTUzNDJjY2JiMmVhMGRkOWY3YmI4ZTBkYTQ5ZGI1NjE0ZmQyNGUyMGYifV19XQ== FVPROV1-END
