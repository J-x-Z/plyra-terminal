// SPDX-License-Identifier: GPL-3.0-only
//! Android compositor state supplied by the Activity without per-frame JNI calls.

use crate::GlassEffect;
use anyhow::{ensure, Context, Result};
use std::sync::{Arc, Mutex};

pub type MaterialHandler = Arc<dyn Fn(bool, f32) -> std::result::Result<(), String> + Send + Sync>;

static HANDLER: Mutex<Option<MaterialHandler>> = Mutex::new(None);
static STATE: Mutex<WindowState> = Mutex::new(WindowState {
    api: 0,
    available: false,
    active: false,
    requested: false,
    requested_radius: 0.0,
    radius: 0,
    power_save: false,
    thermal_status: 0,
    reason: 6,
});

#[derive(Clone, Copy, PartialEq)]
struct WindowState {
    api: i32,
    available: bool,
    active: bool,
    requested: bool,
    requested_radius: f32,
    radius: i32,
    power_save: bool,
    thermal_status: i32,
    reason: i32,
}

pub fn install_handler(handler: MaterialHandler) -> bool {
    let Ok(mut slot) = HANDLER.lock() else {
        return false;
    };
    *slot = Some(handler);
    drop(slot);
    let Ok(mut state) = STATE.lock() else {
        return false;
    };
    state.active = false;
    state.radius = 0;
    true
}

pub fn configure(effect: GlassEffect, blur_radius_px: f32) -> Result<bool> {
    ensure!(
        blur_radius_px.is_finite() && (0.0..=100.0).contains(&blur_radius_px),
        "Android compositor blur radius must be 0..100 pixels"
    );
    let handler = HANDLER
        .lock()
        .map_err(|_| anyhow::anyhow!("Android material bridge lock unavailable"))?
        .clone()
        .context("Android material bridge is unavailable")?;
    let enabled = effect != GlassEffect::Off;
    {
        let mut state = STATE
            .lock()
            .map_err(|_| anyhow::anyhow!("Android material state lock unavailable"))?;
        state.requested = enabled;
        state.requested_radius = blur_radius_px;
    }
    if let Err(error) = handler(enabled, blur_radius_px) {
        if let Ok(mut state) = STATE.lock() {
            state.active = false;
            state.radius = 0;
            state.reason = 7;
        }
        anyhow::bail!("Android material request failed: {error}");
    }
    Ok(active())
}

/// Called only after the Activity has applied the corresponding Window attributes.
pub fn update_state(
    api: i32,
    available: bool,
    active: bool,
    power_save: bool,
    thermal_status: i32,
    radius: i32,
    reason: i32,
) -> bool {
    let Ok(mut state) = STATE.lock() else {
        return false;
    };
    let previous = *state;
    state.api = api.max(0);
    state.available = api >= 31 && available;
    state.power_save = power_save;
    state.thermal_status = thermal_status.clamp(0, 6);
    state.reason = if (0..=8).contains(&reason) { reason } else { 7 };
    // Window translucency is independent of the compositor's blur capability.
    // Keep explicit Off, lifecycle and resource-limit fallbacks opaque.
    state.active = active
        && !power_save
        && state.thermal_status < 2
        && matches!(state.reason, 1 | 2 | 3 | 8);
    state.radius = if state.active && state.available && state.reason == 1 {
        radius.clamp(0, 100)
    } else {
        0
    };
    *state != previous
}

pub fn active() -> bool {
    STATE
        .lock()
        .is_ok_and(|state| state.requested && state.active)
}

pub fn diagnostics() -> String {
    let Ok(state) = STATE.lock().map(|state| *state) else {
        return "android_material_state=unavailable".into();
    };
    let bridge = HANDLER.lock().is_ok_and(|handler| handler.is_some());
    let reason = match state.reason {
        0 => "disabled",
        1 => "blur_requested",
        2 => "system_blur_disabled_or_unsupported",
        3 => "api_below_31",
        4 => "power_save",
        5 => "thermal_limit",
        6 => "activity_inactive",
        8 => "clear_without_blur",
        _ => "platform_error",
    };
    format!(
        "android_material_bridge={bridge} api={} cross_window_blur_enabled={} native_window_translucency={} clear_transmission_fallback={} requested={} requested_blur_radius_px={} applied_blur_radius_px={} power_save={} thermal_status={} state={reason} backend=Window_FLAG_BLUR_BEHIND visual_confirmation=unverified",
        state.api,
        state.available,
        state.requested && state.active,
        state.requested && state.active && !state.available,
        state.requested,
        state.requested_radius,
        state.radius,
        state.power_save,
        state.thermal_status,
    )
}

// FVPROV1 4542 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZWY4NzMzM2M3ZTEyYTAzODZlMDAyYzFhNzE5NDk1NmZjZTRlMzAyNjZhYjc1YjRlNzgzMWM0YzA4NGM1MjMyOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMTljNTFkMzUwMTQ3NzI0YzMxOTdkNmZiZDYwYzk0OGY2NmY2Y2Q3NjEzYzU5MzI3MjhkYzg5Njc1YjljNTBiYzgwNzhkMDQ1NTQ4MjE1YWQxZjNjY2VmMTE2MTc5OTRjYTE5NWQ4YTU1Yzc1YWM4NGFmMjcxMTFkOTkxODZlMGYifV19XQ== FVPROV1-END
