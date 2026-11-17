// SPDX-License-Identifier: GPL-3.0-only
//! A DirectComposition visual retained alongside the DX12 WGPU surface.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::{
    core::{IUnknown, Interface},
    Win32::{
        Foundation::{BOOL, HWND},
        Graphics::DirectComposition::{
            DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget,
            IDCompositionVisual,
        },
    },
};
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, STYLESTRUCT,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WM_NCDESTROY,
            WM_STYLECHANGING, WS_EX_NOREDIRECTIONBITMAP,
        },
    },
};
use winit::{
    platform::windows::{BackdropType, WindowExtWindows},
    window::Window,
};

fn raw_hwnd(window: &Window) -> Result<isize> {
    let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
        bail!("DirectComposition requires a Win32 window handle");
    };
    Ok(handle.hwnd.get())
}

pub(crate) fn has_no_redirection_bitmap(window: &Window) -> Result<bool> {
    let handle = raw_hwnd(window)? as windows_sys::Win32::Foundation::HWND;
    unsafe {
        SetLastError(0);
        let style = GetWindowLongPtrW(handle, GWL_EXSTYLE);
        if style == 0 && GetLastError() != 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading window redirection style");
        }
        Ok(style as u32 & WS_EX_NOREDIRECTIONBITMAP != 0)
    }
}

pub(crate) struct CompositionSurface {
    visual: IDCompositionVisual,
    target: IDCompositionTarget,
    device: IDCompositionDevice,
    // Keep the target HWND valid through COM teardown.
    _window: Arc<Window>,
}

impl CompositionSurface {
    pub(crate) fn new(window: Arc<Window>) -> Result<Self> {
        let handle = HWND(raw_hwnd(&window)? as *mut _);
        set_redirection_policy(&window, true)?;
        // The visual receives a DXGI swapchain from WGPU, so no D2D rendering
        // device or extra D3D11 device is needed by this composition factory.
        let device: IDCompositionDevice = unsafe {
            DCompositionCreateDevice2(None::<&IUnknown>)
                .map_err(|error| anyhow!("creating the DirectComposition device: {error}"))?
        };
        let target = unsafe {
            device
                .CreateTargetForHwnd(handle, BOOL(1))
                .map_err(|error| anyhow!("creating the DirectComposition window target: {error}"))?
        };
        let visual = unsafe {
            device
                .CreateVisual()
                .map_err(|error| anyhow!("creating the DirectComposition visual: {error}"))?
        };
        unsafe {
            target
                .SetRoot(&visual)
                .map_err(|error| anyhow!("attaching the DirectComposition visual: {error}"))?;
        }
        let composition = Self {
            visual,
            target,
            device,
            _window: window,
        };
        composition.commit()?;
        Ok(composition)
    }

    /// The renderer retains this owner until after the returned surface drops.
    pub(crate) fn create_surface(
        &self,
        instance: &wgpu::Instance,
    ) -> Result<wgpu::Surface<'static>> {
        // WGPU retains its own COM reference to the visual. The target and
        // device remain owned here for the visual tree's transactional updates.
        unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(
                self.visual.as_raw(),
            ))
        }
        .context("creating the DX12 DirectComposition surface")
    }

    /// Commit after surface configuration, which attaches or replaces its swapchain.
    /// Presentation of an existing swapchain needs no recurring COM transaction.
    pub(crate) fn commit(&self) -> Result<()> {
        unsafe { self.device.Commit() }
            .map_err(|error| anyhow!("committing the DirectComposition visual tree: {error}"))
    }
}

impl Drop for CompositionSurface {
    fn drop(&mut self) {
        unsafe {
            let _ = self.visual.SetContent(None::<&IUnknown>);
            let _ = self.target.SetRoot(None::<&IDCompositionVisual>);
            let _ = self.device.Commit();
        }
    }
}

const REDIRECTION_SUBCLASS_ID: usize = 0x47545346;

fn redirection_style(style: u32, composition: bool) -> u32 {
    if composition {
        style | WS_EX_NOREDIRECTIONBITMAP
    } else {
        style & !WS_EX_NOREDIRECTIONBITMAP
    }
}

unsafe extern "system" fn redirection_subclass(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass: usize,
    composition: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(window, Some(redirection_subclass), subclass) };
    }
    let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
    if message == WM_STYLECHANGING && wparam as isize == GWL_EXSTYLE as isize && lparam != 0 {
        // Winit retains its creation-time NO_BACK_BUFFER flag. Filter each
        // proposed update so later window operations retain the active mode.
        let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
        style.styleNew = redirection_style(style.styleNew, composition != 0);
    }
    result
}

fn set_redirection_policy(window: &Window, composition: bool) -> Result<()> {
    let handle = raw_hwnd(window)? as windows_sys::Win32::Foundation::HWND;
    unsafe {
        if SetWindowSubclass(
            handle,
            Some(redirection_subclass),
            REDIRECTION_SUBCLASS_ID,
            composition as usize,
        ) == 0
        {
            bail!("installing the window redirection policy failed");
        }
        SetLastError(0);
        let style = GetWindowLongPtrW(handle, GWL_EXSTYLE);
        if style == 0 && GetLastError() != 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading window redirection style");
        }
        let desired = redirection_style(style as u32, composition) as isize;
        if style != desired {
            SetLastError(0);
            let previous = SetWindowLongPtrW(handle, GWL_EXSTYLE, desired);
            if previous == 0 && GetLastError() != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("restoring window redirection style");
            }
            if SetWindowPos(
                handle,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
            ) == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("applying window redirection style");
            }
        }
    }
    Ok(())
}

/// Restore HWND presentation before falling back from DirectComposition.
pub(crate) fn restore_redirection(window: &Window) -> Result<()> {
    set_redirection_policy(window, false)?;
    window.set_system_backdrop(BackdropType::None);
    window.set_transparent(false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_removes_redirection_flag_from_future_style_updates() {
        let other_style = 0x00040000;
        let composition = redirection_style(other_style, true);
        assert_eq!(composition, other_style | WS_EX_NOREDIRECTIONBITMAP);
        assert_eq!(redirection_style(composition, false), other_style);
        assert_eq!(redirection_style(other_style, false), other_style);
    }
}

// FVPROV1 7974 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTE5NTUyNjg3YmQ3YTk4OTY2OWEyOGZhMGQzYWY4YTJmYzQxYzg4YWVmM2JkODcyYWYxNDA4MzNjZGU2M2JiMiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZWYwNTY4ZTdiOTdjYTNiNDI2YmMyMWI4ZDRkOWIyZGMyMWZiMjlhN2ZjZjYyMjAyZWFlYzg5OWNmNmZkNWI0NzI5ZGZhNDIwODAxOTEzMGY0OWZjOTQ4ZmExMjMzZDY0MzM1MmE3MDNjNTIxOTI4MjEzNjdlNjI4ZDg1N2UwMGMifV19XQ== FVPROV1-END
