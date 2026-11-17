// SPDX-License-Identifier: GPL-3.0-only
//! Native Windows backdrop requests through documented DWM APIs.

use std::{mem::size_of, ptr};

use anyhow::{bail, Result};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{BOOL, HWND},
    Graphics::{
        Dwm::{
            DwmEnableBlurBehindWindow, DwmExtendFrameIntoClientArea, DwmGetWindowAttribute,
            DwmIsCompositionEnabled, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE,
            DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND,
        },
        Gdi::{CreateRectRgn, DeleteObject},
    },
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        Controls::MARGINS,
        WindowsAndMessaging::{
            GetWindowLongPtrW, SystemParametersInfoW, GWL_EXSTYLE, SPI_GETCLIENTAREAANIMATION,
            SPI_GETDISABLEOVERLAPPEDCONTENT, SPI_GETHIGHCONTRAST, WS_EX_NOREDIRECTIONBITMAP,
        },
    },
};
use winit::{
    platform::windows::{BackdropType, CornerPreference, WindowExtWindows},
    window::Window,
};

use crate::{AccessibilityPreferences, GlassEffect, GlassMaterial};

pub(crate) fn preferences() -> AccessibilityPreferences {
    let mut animation: BOOL = 1;
    let mut disable_overlapped_content: BOOL = 0;
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        dwFlags: 0,
        lpszDefaultScheme: ptr::null_mut(),
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut animation as *mut BOOL).cast(),
            0,
        );
        SystemParametersInfoW(
            SPI_GETDISABLEOVERLAPPEDCONTENT,
            0,
            (&mut disable_overlapped_content as *mut BOOL).cast(),
            0,
        );
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            (&mut contrast as *mut HIGHCONTRASTW).cast(),
            0,
        );
    }
    AccessibilityPreferences {
        reduce_motion: animation == 0,
        reduce_transparency: disable_overlapped_content != 0
            || contrast.dwFlags & HCF_HIGHCONTRASTON != 0,
    }
}

fn hwnd(window: &Window) -> Result<HWND> {
    let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
        bail!("Windows backdrop requires a Win32 window handle");
    };
    Ok(handle.hwnd.get() as HWND)
}

fn read_attribute(window: HWND, attribute: i32) -> std::result::Result<i32, i32> {
    let mut value = 0_i32;
    let result = unsafe {
        DwmGetWindowAttribute(
            window,
            attribute as u32,
            (&mut value as *mut i32).cast(),
            size_of::<i32>() as u32,
        )
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(value)
    }
}

fn composition_enabled() -> std::result::Result<bool, i32> {
    let mut enabled: BOOL = 0;
    let result = unsafe { DwmIsCompositionEnabled(&mut enabled) };
    if result < 0 {
        Err(result)
    } else {
        Ok(enabled != 0)
    }
}

fn backdrop(material: GlassMaterial) -> BackdropType {
    match material {
        GlassMaterial::UnderWindowBackground => BackdropType::MainWindow,
        GlassMaterial::LiquidClear
        | GlassMaterial::LiquidRegular
        | GlassMaterial::Hud
        | GlassMaterial::Sidebar => BackdropType::TransientWindow,
    }
}

fn corners(radius: f32) -> CornerPreference {
    if !radius.is_finite() || radius <= 0.0 {
        CornerPreference::DoNotRound
    } else if radius <= 8.0 {
        CornerPreference::RoundSmall
    } else {
        CornerPreference::Round
    }
}

fn extend_frame(window: HWND, enabled: bool) -> i32 {
    let inset = if enabled { -1 } else { 0 };
    let margins = MARGINS {
        cxLeftWidth: inset,
        cxRightWidth: inset,
        cyTopHeight: inset,
        cyBottomHeight: inset,
    };
    unsafe { DwmExtendFrameIntoClientArea(window, &margins) }
}

fn clear_backdrop(window: &Window, handle: HWND) {
    window.set_system_backdrop(BackdropType::None);
    transparent_client(handle, false);
    extend_frame(handle, false);
    window.set_transparent(false);
}

fn transparent_client(handle: HWND, enabled: bool) -> i32 {
    // Winit normally installs this empty blur region for transparent Win32
    // clients, but skips it for NOREDIRECTIONBITMAP composition windows.
    // Without it, removing Acrylic leaves an opaque black DWM client behind
    // the premultiplied DirectComposition surface.
    let region = if enabled {
        unsafe { CreateRectRgn(0, 0, -1, -1) }
    } else {
        ptr::null_mut()
    };
    if enabled && region.is_null() {
        return -1;
    }
    let blur = DWM_BLURBEHIND {
        dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
        fEnable: i32::from(enabled),
        hRgnBlur: region,
        fTransitionOnMaximized: 0,
    };
    let result = unsafe { DwmEnableBlurBehindWindow(handle, &blur) };
    if !region.is_null() {
        unsafe { DeleteObject(region) };
    }
    result
}

/// Returns whether DWM accepted the backdrop and full-client-area frame.
/// The compositor controls its visible recipe and runtime power/accessibility fallback.
pub(crate) fn configure(
    window: &Window,
    effect: GlassEffect,
    material: GlassMaterial,
    corner_radius: f32,
    blur_radius_px: f32,
) -> Result<bool> {
    let handle = hwnd(window)?;
    window.set_corner_preference(corners(corner_radius));
    window.set_border_color(None);
    let enabled = effect != GlassEffect::Off && !preferences().reduce_transparency;
    let available = composition_enabled().unwrap_or(false)
        && read_attribute(handle, DWMWA_SYSTEMBACKDROP_TYPE).is_ok();
    if !enabled || !available {
        clear_backdrop(window, handle);
        return Ok(false);
    }
    // DWM does not expose an Acrylic blur radius. Zero requests clear
    // DirectComposition transparency; positive values enable system Acrylic.
    let selected = if blur_radius_px > 0.0 {
        backdrop(material)
    } else {
        BackdropType::None
    };
    window.set_transparent(true);
    window.set_system_backdrop(selected);
    let client = if blur_radius_px > 0.0 {
        0
    } else {
        transparent_client(handle, true)
    };
    let applied = client >= 0
        && extend_frame(handle, true) >= 0
        && read_attribute(handle, DWMWA_SYSTEMBACKDROP_TYPE) == Ok(selected as i32);
    if !applied {
        clear_backdrop(window, handle);
    }
    Ok(applied)
}

pub(crate) fn diagnostics(window: &Window) -> Result<String> {
    let handle = hwnd(window)?;
    let composition = composition_enabled();
    let selected = read_attribute(handle, DWMWA_SYSTEMBACKDROP_TYPE);
    let corner = read_attribute(handle, DWMWA_WINDOW_CORNER_PREFERENCE);
    let material = match selected {
        Ok(2) => "DWM.Mica",
        Ok(3) => "DWM.DesktopAcrylic",
        Ok(4) => "DWM.MicaAlt",
        Ok(0) => "DWM.Auto",
        Ok(1) => "off",
        Ok(_) => "DWM.unknown",
        Err(_) => "unavailable",
    };
    let attribute = |value: std::result::Result<i32, i32>| match value {
        Ok(value) => value.to_string(),
        Err(error) => format!("HRESULT_0x{:08X}", error as u32),
    };
    let preferences = preferences();
    let ex_style = unsafe { GetWindowLongPtrW(handle, GWL_EXSTYLE) } as u32;
    Ok(format!(
        "native_backdrop={} composition={:?} backdrop_attribute={} corner_preference={} ex_style=0x{:08X} no_redirection_bitmap={} system_backdrop_minimum_build=22621 backdrop_confirmation=DWM_attribute_readback visual_confirmation=unverified reduce_motion={} reduce_transparency={} transparency_recipe=client_alpha blur_strength=system_managed_when_enabled",
        material,
        composition,
        attribute(selected),
        attribute(corner),
        ex_style,
        ex_style & WS_EX_NOREDIRECTIONBITMAP != 0,
        preferences.reduce_motion,
        preferences.reduce_transparency,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_uses_acrylic_and_under_window_uses_mica() {
        assert_eq!(
            backdrop(GlassMaterial::LiquidClear),
            BackdropType::TransientWindow
        );
        assert_eq!(
            backdrop(GlassMaterial::LiquidRegular),
            BackdropType::TransientWindow
        );
        assert_eq!(
            backdrop(GlassMaterial::UnderWindowBackground),
            BackdropType::MainWindow
        );
    }

    #[test]
    fn zero_radius_keeps_square_corners() {
        assert_eq!(corners(0.0), CornerPreference::DoNotRound);
        assert_eq!(corners(8.0), CornerPreference::RoundSmall);
        assert_eq!(corners(16.0), CornerPreference::Round);
        assert_eq!(corners(f32::NAN), CornerPreference::DoNotRound);
    }
}

// FVPROV1 8797 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDM1OGZjZWJlMmIzMDBmZGJjYjAwOWVkMTQyZjlmY2MzNGNkMTcxMGQ4MmFjNzMyZTkzMjQ5NmI1YjQ0OGU0OSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMmY4ZGEyNGVkYmM3ODBhMDEyOWI3YTMyYTAzOThiNDA5M2NlN2I3MjZiNDI0MzRiOWVhZThmMzUxNzFkMGE5MzgzYzZiYWQxYjlhYWI5NzE3MWY5YjU1OTBjNjI0MjMwMDUxNWUxZGE2ZDUxMDZjZTlkZjIxOGIxYWVlNDZhMGIifV19XQ== FVPROV1-END
