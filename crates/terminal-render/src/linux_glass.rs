// SPDX-License-Identifier: GPL-3.0-only
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::protocol::{wl_callback, wl_compositor, wl_region, wl_registry, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1 as manager, ext_background_effect_surface_v1 as background,
};
use winit::window::{Window, WindowId};
use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as X11Wrapper;

use crate::GlassEffect;

/// Niri has no server titlebar, and its server-side mode keeps the focus ring
/// outside transparent client content. Other compositors retain the canvas mode.
pub fn prefer_server_decorations(display: raw_window_handle::DisplayHandle<'_>) -> bool {
    let RawDisplayHandle::Wayland(display) = display.as_raw() else {
        return false;
    };
    // Borrow the already-live display without taking connection ownership.
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let mut peer = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            backend.poll_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0
        && peer.pid > 0
        && std::fs::read_link(format!("/proc/{}/exe", peer.pid))
            .ok()
            .and_then(|path| path.file_name().map(|name| name == "niri"))
            .unwrap_or(false)
}

thread_local! {
    static WINDOWS: RefCell<HashMap<WindowId, LinuxGlass>> = RefCell::new(HashMap::new());
}

enum LinuxGlass {
    Wayland(Box<WaylandGlass>),
    X11(X11Glass),
}

#[derive(Default)]
struct ProtocolState {
    manager: Option<manager::ExtBackgroundEffectManagerV1>,
    compositor: Option<wl_compositor::WlCompositor>,
    manager_global: Option<u32>,
    blur_capable: bool,
    synchronized: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name, interface, ..
            } => {
                if interface == manager::ExtBackgroundEffectManagerV1::interface().name {
                    state.manager = Some(registry.bind(name, 1, queue, ()));
                    state.manager_global = Some(name);
                } else if interface == wl_compositor::WlCompositor::interface().name {
                    state.compositor = Some(registry.bind(name, 1, queue, ()));
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.manager_global == Some(name) => {
                if let Some(manager) = state.manager.take() {
                    manager.destroy();
                }
                state.manager_global = None;
                state.blur_capable = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<manager::ExtBackgroundEffectManagerV1, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &manager::ExtBackgroundEffectManagerV1,
        event: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let manager::Event::Capabilities { flags } = event {
            state.blur_capable =
                matches!(flags, WEnum::Value(flags) if flags.contains(manager::Capability::Blur));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synchronized = true;
    }
}

delegate_noop!(ProtocolState: ignore wl_compositor::WlCompositor);
delegate_noop!(ProtocolState: ignore wl_region::WlRegion);
delegate_noop!(ProtocolState: ignore background::ExtBackgroundEffectSurfaceV1);

struct WaylandGlass {
    connection: Connection,
    queue: EventQueue<ProtocolState>,
    protocol: ProtocolState,
    registry: wl_registry::WlRegistry,
    surface: wl_surface::WlSurface,
    effect: Option<background::ExtBackgroundEffectSurfaceV1>,
    shape: Option<(u32, u32, u32)>,
    enabled: bool,
    radius: f32,
    fallback_enabled: Option<bool>,
}

impl WaylandGlass {
    unsafe fn new(display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) -> Result<Self> {
        // Guest mode keeps winit's display and surface under winit's ownership.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        let surface_id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()) }?;
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id)?;
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let mut state = Self {
            connection,
            queue,
            protocol: ProtocolState::default(),
            registry,
            surface,
            effect: None,
            shape: None,
            enabled: false,
            radius: 0.0,
            fallback_enabled: None,
        };
        state.synchronize()?;
        // Binding globals above queues the initial capability event.
        state.synchronize()?;
        Ok(state)
    }

    fn dispatch(&mut self) -> Result<()> {
        self.connection.backend().dispatch_inner_queue()?;
        self.queue.dispatch_pending(&mut self.protocol)?;
        Ok(())
    }

    fn synchronize(&mut self) -> Result<()> {
        self.protocol.synchronized = false;
        let callback = self.connection.display().sync(&self.queue.handle(), ());
        self.connection.flush()?;
        let deadline = Instant::now() + Duration::from_millis(250);
        while !self.protocol.synchronized {
            self.dispatch()?;
            if self.protocol.synchronized {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.connection.backend().destroy_object(&callback.id());
                anyhow::bail!("Wayland background-effect discovery exceeded 250ms");
            }
            if let Some(reader) = self.connection.prepare_read() {
                let mut descriptor = libc::pollfd {
                    fd: reader.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let count =
                    unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().max(1) as i32) };
                if count > 0 {
                    reader.read()?;
                } else if count < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    fn update(&mut self, window: &Window) -> Result<bool> {
        self.dispatch()?;
        if self.protocol.manager.is_none() {
            if let Some(effect) = self.effect.take() {
                effect.destroy();
                self.shape = None;
            }
            if self.fallback_enabled != Some(self.enabled) {
                window.set_blur(self.enabled);
                self.fallback_enabled = Some(self.enabled);
            }
            return Ok(false);
        }
        if self.fallback_enabled == Some(true) {
            window.set_blur(false);
            self.fallback_enabled = Some(false);
        }
        if self.effect.is_none() {
            self.effect = Some(
                self.protocol
                    .manager
                    .as_ref()
                    .unwrap()
                    .get_background_effect(&self.surface, &self.queue.handle(), ()),
            );
            self.shape = None;
        }
        let logical = window.inner_size().to_logical::<u32>(window.scale_factor());
        let shape = (
            logical.width.max(1),
            logical.height.max(1),
            self.radius.to_bits(),
        );
        if self.shape != Some(shape) {
            if self.enabled {
                let compositor = self
                    .protocol
                    .compositor
                    .as_ref()
                    .context("Wayland compositor global unavailable")?;
                let region = compositor.create_region(&self.queue.handle(), ());
                for [x, y, width, height] in rounded_region(shape.0, shape.1, self.radius) {
                    region.add(x, y, width, height);
                }
                self.effect.as_ref().unwrap().set_blur_region(Some(&region));
                region.destroy();
            } else {
                self.effect.as_ref().unwrap().set_blur_region(None);
            }
            self.shape = Some(shape);
            // wgpu's next presentation commits this surface-local state.
            self.connection.flush()?;
        }
        Ok(self.enabled && self.protocol.blur_capable)
    }
}

impl Drop for WaylandGlass {
    fn drop(&mut self) {
        if let Some(effect) = self.effect.take() {
            effect.destroy();
        }
        if let Some(manager) = self.protocol.manager.take() {
            manager.destroy();
        }
        if let Some(compositor) = self.protocol.compositor.take() {
            let _ = self.connection.backend().destroy_object(&compositor.id());
        }
        let _ = self
            .connection
            .backend()
            .destroy_object(&self.registry.id());
        let _ = self.connection.flush();
    }
}

struct X11Glass {
    connection: RustConnection,
    window: u32,
    blur_atom: u32,
    supported: bool,
    requested: bool,
    shape: Option<(u32, u32, u32)>,
    radius: f32,
}

impl X11Glass {
    fn new(window: u32) -> Result<Self> {
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let blur_atom = connection
            .intern_atom(false, b"_KDE_NET_WM_BLUR_BEHIND_REGION")?
            .reply()?
            .atom;
        let supported = connection
            .list_properties(root)?
            .reply()?
            .atoms
            .contains(&blur_atom);
        Ok(Self {
            connection,
            window,
            blur_atom,
            supported,
            requested: false,
            shape: None,
            radius: 0.0,
        })
    }

    fn configure(&mut self, window: &Window, enabled: bool, radius: f32) -> Result<bool> {
        let size = window.inner_size();
        let physical_radius = radius * window.scale_factor() as f32;
        let shape = (
            size.width.max(1),
            size.height.max(1),
            physical_radius.to_bits(),
        );
        if self.requested == enabled && self.shape == Some(shape) {
            return Ok(enabled && self.supported);
        }
        if enabled {
            let rectangles: Vec<u32> = rounded_region(shape.0, shape.1, physical_radius)
                .into_iter()
                .flatten()
                .map(|value| value as u32)
                .collect();
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    self.window,
                    self.blur_atom,
                    AtomEnum::CARDINAL,
                    &rectangles,
                )?
                .check()?;
        } else {
            self.connection
                .delete_property(self.window, self.blur_atom)?
                .check()?;
        }
        self.connection.flush()?;
        self.requested = enabled;
        self.radius = radius;
        self.shape = Some(shape);
        Ok(enabled && self.supported)
    }
}

fn rounded_region(width: u32, height: u32, radius: f32) -> Vec<[i32; 4]> {
    let width = width.min(i32::MAX as u32) as i32;
    let height = height.min(i32::MAX as u32) as i32;
    let radius = radius.clamp(0.0, width.min(height) as f32 / 2.0);
    let rows = radius.ceil() as i32;
    if rows == 0 {
        return vec![[0, 0, width, height]];
    }
    let mut rectangles = Vec::with_capacity((rows * 2 + 1) as usize);
    if height > rows * 2 {
        rectangles.push([0, rows, width, height - rows * 2]);
    }
    for y in 0..rows {
        let distance = (radius - y as f32 - 0.5).max(0.0);
        let inset = (radius - (radius * radius - distance * distance).sqrt()).ceil() as i32;
        let span = width - inset * 2;
        if span > 0 {
            rectangles.push([inset, y, span, 1]);
            rectangles.push([inset, height - y - 1, span, 1]);
        }
    }
    rectangles
}

pub(crate) fn configure(
    window: &Window,
    effect: GlassEffect,
    radius: f32,
    blur_radius: f32,
) -> Result<bool> {
    let translucent = effect != GlassEffect::Off;
    let enabled = translucent && blur_radius > 0.0;
    window.set_transparent(translucent || radius > 0.0);
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        if !windows.contains_key(&window.id()) {
            let state = match (
                window.display_handle()?.as_raw(),
                window.window_handle()?.as_raw(),
            ) {
                (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) => {
                    LinuxGlass::Wayland(Box::new(unsafe {
                        WaylandGlass::new(display.display.as_ptr(), surface.surface.as_ptr())
                    }?))
                }
                (_, RawWindowHandle::Xlib(handle)) => {
                    LinuxGlass::X11(X11Glass::new(handle.window.try_into()?)?)
                }
                (_, RawWindowHandle::Xcb(handle)) => {
                    LinuxGlass::X11(X11Glass::new(handle.window.get())?)
                }
                _ => anyhow::bail!("Linux material requires a Wayland or X11 window"),
            };
            windows.insert(window.id(), state);
        }
        match windows.get_mut(&window.id()).unwrap() {
            LinuxGlass::Wayland(state) => {
                if state.enabled != enabled || state.radius != radius {
                    state.shape = None;
                }
                state.enabled = enabled;
                state.radius = radius;
                state.update(window)
            }
            LinuxGlass::X11(state) => state.configure(window, enabled, radius),
        }
    })
}

pub(crate) fn poll(window: &Window) {
    WINDOWS.with(|windows| match windows.borrow_mut().get_mut(&window.id()) {
        Some(LinuxGlass::Wayland(state)) => {
            let _ = state.update(window);
        }
        Some(LinuxGlass::X11(state)) => {
            let _ = state.configure(window, state.requested, state.radius);
        }
        None => {}
    });
}

pub(crate) fn diagnostics(window: &Window) -> Result<String> {
    WINDOWS.with(|windows| match windows.borrow().get(&window.id()) {
        Some(LinuxGlass::Wayland(state)) => Ok(format!(
            "backend=Wayland protocol={} blur_capability={} blur_requested={} blur_shape={:?} pixel_effect_verification=required",
            if state.protocol.manager.is_some() { "ext-background-effect-v1" } else { "KWin-request-if-supported" },
            state.protocol.blur_capable, state.enabled, state.shape,
        )),
        Some(LinuxGlass::X11(state)) => Ok(format!(
            "backend=X11 protocol=KDE-blur-property compositor_advertised={} blur_requested={} pixel_effect_verification=required",
            state.supported, state.requested,
        )),
        None => Ok("native_glass=unconfigured".into()),
    })
}

pub(crate) fn release(window: &Window) {
    WINDOWS.with(|windows| {
        windows.borrow_mut().remove(&window.id());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_shape_matches_rectangular_and_rounded_window_bounds() {
        assert_eq!(rounded_region(80, 50, 0.0), vec![[0, 0, 80, 50]]);
        let rounded = rounded_region(80, 50, 12.0);
        assert!(rounded.iter().all(|[x, y, w, h]| *x >= 0
            && *y >= 0
            && *w > 0
            && *h > 0
            && x + w <= 80
            && y + h <= 50));
        assert!(rounded.iter().any(|[x, y, _, _]| *y == 0 && *x > 0));
        assert!(rounded.len() <= 97);
    }
}

// FVPROV1 17388 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYmVhNjRkY2Q1NmZkYmIyYzU1OGM1OWZkZWZkMDgzMDQ1YzQ2Y2FmYjNkYmRhYTliMDA3NTQyN2VmMWY0ZTQ3YiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZjc4NDY1MDk4MzUzMzlmOTJhM2Q5MDYyZTZhZWY5Yjg0MTEwNGY0ZDgzNTNiOGRmZmNhNTMwZmViY2MwZDM1NzVkZTY3NDFlZTRhNDJlZmI4NTg1MDk4Y2FlMzg3MzExMThhYjcxMDZlN2JiZGY2NDRhYjgxZDYyNTFkYjVlMDMifV19XQ== FVPROV1-END
