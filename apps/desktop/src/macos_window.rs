// SPDX-License-Identifier: GPL-3.0-only
//! Native window buttons and window dragging in a transparent corner overlay.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    sync::OnceLock,
};

use anyhow::{Context, Result};
use objc2::{
    class,
    declare::ClassBuilder,
    msg_send, msg_send_id,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, Sel},
    sel,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use winit::{
    dpi::PhysicalPosition,
    event_loop::EventLoopProxy,
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::Window,
};

use crate::{macos_menu::MenuCommand, Wake};

thread_local! {
    static OWNERS: RefCell<HashMap<usize, Weak<TrackingState>>> = RefCell::new(HashMap::new());
}

fn state(owner: &AnyObject) -> Option<Rc<TrackingState>> {
    OWNERS.with(|owners| {
        owners
            .borrow()
            .get(&(owner as *const AnyObject as usize))
            .and_then(Weak::upgrade)
    })
}

extern "C" fn entered(owner: &AnyObject, _: Sel, _: &AnyObject) {
    if let Some(state) = state(owner) {
        state.set_visible(true);
    }
}

extern "C" fn exited(owner: &AnyObject, _: Sel, _: &AnyObject) {
    if let Some(state) = state(owner) {
        state.set_visible(false);
    }
}

extern "C" fn changed(owner: &AnyObject, _: Sel, _: &AnyObject) {
    if let Some(state) = state(owner) {
        state.update(owner);
    }
}

extern "C" fn perform_control(owner: &AnyObject, _: Sel, button: &AnyObject) {
    let Some(state) = state(owner) else {
        return;
    };
    let kind: isize = unsafe { msg_send![button, tag] };
    let command = match kind {
        0 => MenuCommand::CloseWindow,
        1 => MenuCommand::Minimize,
        2 => MenuCommand::Fullscreen,
        _ => return,
    };
    let _ = state.proxy.send_event(Wake::MacMenu(command));
}

fn owner_class() -> &'static AnyClass {
    static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("PlyraTerminalTrafficLightsOwner", class!(NSObject))
            .expect("unique traffic lights owner class");
        unsafe {
            builder.add_method(sel!(mouseEntered:), entered as extern "C" fn(_, _, _));
            builder.add_method(sel!(mouseExited:), exited as extern "C" fn(_, _, _));
            builder.add_method(
                sel!(windowGeometryChanged:),
                changed as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(windowBecameInactive:),
                exited as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(performWindowControl:),
                perform_control as extern "C" fn(_, _, _),
            );
        }
        builder.register()
    })
}

fn contains(rect: NSRect, point: NSPoint) -> bool {
    point.x >= rect.origin.x
        && point.x < rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y < rect.origin.y + rect.size.height
}

extern "C" fn overlay_hit_test(view: &AnyObject, _: Sel, point: NSPoint) -> *mut AnyObject {
    unsafe {
        let parent: *mut AnyObject = msg_send![view, superview];
        if parent.is_null() {
            return std::ptr::null_mut();
        }
        let local: NSPoint = msg_send![view, convertPoint: point fromView: parent];
        let bounds: NSRect = msg_send![view, bounds];
        if !contains(bounds, local) || !state(view).is_some_and(|state| state.enabled.get()) {
            return std::ptr::null_mut();
        }
        let children: *mut AnyObject = msg_send![view, subviews];
        let count: usize = msg_send![children, count];
        for index in 0..count.min(3) {
            let button: *mut AnyObject = msg_send![children, objectAtIndex: index];
            let hidden: Bool = msg_send![button, isHidden];
            let frame: NSRect = msg_send![button, frame];
            if !hidden.as_bool() && contains(frame, local) {
                return msg_send![button, hitTest: local];
            }
        }
    }
    view as *const AnyObject as *mut AnyObject
}

extern "C" fn overlay_mouse_down(view: &AnyObject, _: Sel, event: &AnyObject) {
    let Some(state) = state(view).filter(|state| state.enabled.get()) else {
        return;
    };
    unsafe {
        state
            .mouse_downs
            .set(state.mouse_downs.get().saturating_add(1));
        state.drag.borrow_mut().take();
        let style: usize = msg_send![&*state.window, styleMask];
        if style & (1 << 14) != 0 {
            return;
        }
        let _: () = msg_send![&*state.window, setMovable: Bool::YES];
        let clicks: isize = msg_send![event, clickCount];
        if clicks >= 2 {
            let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
            let action: *mut AnyObject =
                msg_send![defaults, stringForKey: &*NSString::from_str("AppleActionOnDoubleClick")];
            let minimize: Bool = if action.is_null() {
                Bool::NO
            } else {
                msg_send![action, isEqualToString: &*NSString::from_str("Minimize")]
            };
            let disabled: Bool = if action.is_null() {
                Bool::NO
            } else {
                msg_send![action, isEqualToString: &*NSString::from_str("None")]
            };
            if minimize.as_bool() {
                let _: () =
                    msg_send![&*state.window, performMiniaturize: std::ptr::null::<AnyObject>()];
            } else if !disabled.as_bool() {
                let _: () = msg_send![&*state.window, performZoom: std::ptr::null::<AnyObject>()];
            }
        } else {
            let local: NSPoint = msg_send![event, locationInWindow];
            let pointer: NSPoint = msg_send![&*state.window, convertPointToScreen: local];
            let frame: NSRect = msg_send![&*state.window, frame];
            *state.drag.borrow_mut() = Some(WindowDrag {
                pointer,
                origin: frame.origin,
                manual: false,
            });
            let _: () = msg_send![&*state.window, performWindowDragWithEvent: event];
        }
    }
}

extern "C" fn overlay_mouse_dragged(view: &AnyObject, _: Sel, event: &AnyObject) {
    let Some(state) = state(view) else {
        return;
    };
    state
        .drag_events
        .set(state.drag_events.get().saturating_add(1));
    let Some(mut drag) = state.drag.borrow_mut().take() else {
        return;
    };
    unsafe {
        let frame: NSRect = msg_send![&*state.window, frame];
        if !drag.manual && frame.origin != drag.origin {
            return;
        }
        let local: NSPoint = msg_send![event, locationInWindow];
        let pointer: NSPoint = msg_send![&*state.window, convertPointToScreen: local];
        let origin = NSPoint::new(
            drag.origin.x + pointer.x - drag.pointer.x,
            drag.origin.y + pointer.y - drag.pointer.y,
        );
        if origin != frame.origin {
            drag.manual = true;
            let _: () = msg_send![&*state.window, setFrameOrigin: origin];
            state
                .fallback_moves
                .set(state.fallback_moves.get().saturating_add(1));
        }
    }
    *state.drag.borrow_mut() = Some(drag);
}

extern "C" fn overlay_mouse_up(view: &AnyObject, _: Sel, _: &AnyObject) {
    if let Some(state) = state(view) {
        state.drag.borrow_mut().take();
    }
}

extern "C" fn accepts_first_mouse(_: &AnyObject, _: Sel, _: &AnyObject) -> Bool {
    Bool::YES
}

extern "C" fn flipped(_: &AnyObject, _: Sel) -> Bool {
    Bool::YES
}
extern "C" fn nonopaque(_: &AnyObject, _: Sel) -> Bool {
    Bool::NO
}

fn overlay_class() -> &'static AnyClass {
    static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("PlyraTerminalTrafficLightsOverlay", class!(NSView))
            .expect("unique traffic lights overlay class");
        unsafe {
            builder.add_method(
                sel!(hitTest:),
                overlay_hit_test as extern "C" fn(_, _, _) -> _,
            );
            builder.add_method(
                sel!(mouseDown:),
                overlay_mouse_down as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(mouseDragged:),
                overlay_mouse_dragged as extern "C" fn(_, _, _),
            );
            builder.add_method(sel!(mouseUp:), overlay_mouse_up as extern "C" fn(_, _, _));
            builder.add_method(
                sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> _,
            );
            builder.add_method(sel!(isFlipped), flipped as extern "C" fn(_, _) -> _);
            builder.add_method(sel!(isOpaque), nonopaque as extern "C" fn(_, _) -> _);
        }
        builder.register()
    })
}

struct Attachment {
    area: Retained<AnyObject>,
    rect: NSRect,
}

struct WindowDrag {
    pointer: NSPoint,
    origin: NSPoint,
    manual: bool,
}

struct TrackingState {
    window: Retained<AnyObject>,
    content: Retained<AnyObject>,
    overlay: Retained<AnyObject>,
    buttons: Vec<Retained<AnyObject>>,
    proxy: EventLoopProxy<Wake>,
    enabled: Cell<bool>,
    visible: Cell<bool>,
    attachment: RefCell<Option<Attachment>>,
    drag: RefCell<Option<WindowDrag>>,
    mouse_downs: Cell<u64>,
    drag_events: Cell<u64>,
    fallback_moves: Cell<u64>,
}

impl TrackingState {
    fn set_visible(&self, visible: bool) {
        self.apply_visibility(visible && self.enabled.get(), false);
    }

    fn apply_visibility(&self, visible: bool, force: bool) {
        if !force && self.visible.replace(visible) == visible {
            return;
        }
        self.visible.set(visible);
        unsafe {
            for button in &self.buttons {
                let _: () = msg_send![&**button, setHidden: Bool::new(!visible)];
            }
        }
    }

    fn remove_tracking(&self) {
        let attachment = self.attachment.borrow_mut().take();
        if let Some(attachment) = attachment {
            unsafe {
                let _: () = msg_send![&*self.content, removeTrackingArea: &*attachment.area];
            }
        }
    }

    fn refresh_pointer(&self) {
        unsafe {
            let rect: NSRect = msg_send![&*self.overlay, frame];
            let point: NSPoint = msg_send![&*self.window, mouseLocationOutsideOfEventStream];
            let point: NSPoint = msg_send![&*self.content, convertPoint: point fromView: std::ptr::null::<AnyObject>()];
            self.set_visible(contains(rect, point));
        }
    }

    fn update(&self, owner: &AnyObject) {
        unsafe {
            let bounds: NSRect = msg_send![&*self.content, bounds];
            let flipped: Bool = msg_send![&*self.content, isFlipped];
            let height = 40.0_f64.min(bounds.size.height.max(0.0));
            let rect = NSRect::new(
                NSPoint::new(
                    bounds.origin.x,
                    if flipped.as_bool() {
                        bounds.origin.y
                    } else {
                        bounds.origin.y + bounds.size.height - height
                    },
                ),
                NSSize::new(88.0_f64.min(bounds.size.width.max(0.0)), height),
            );
            let _: () = msg_send![&*self.overlay, setFrame: rect];
            let _: () = msg_send![&*self.overlay, setAutoresizingMask: if flipped.as_bool() { 32_usize } else { 4_usize }];
            // A fullscreen transition may introduce system titlebar buttons.
            for kind in 0..3_usize {
                let system_button: *mut AnyObject =
                    msg_send![&*self.window, standardWindowButton: kind];
                if !system_button.is_null() {
                    let _: () = msg_send![system_button, setHidden: Bool::YES];
                }
            }
            if !self.enabled.get() {
                self.remove_tracking();
                self.apply_visibility(false, true);
                return;
            }
            let same = self
                .attachment
                .borrow()
                .as_ref()
                .is_some_and(|area| area.rect == rect);
            if !same {
                self.remove_tracking();
                let area = msg_send_id![class!(NSTrackingArea), alloc];
                let area: Retained<AnyObject> = msg_send_id![area, initWithRect: rect options: 1153_usize owner: owner userInfo: std::ptr::null::<AnyObject>()];
                let _: () = msg_send![&*self.content, addTrackingArea: &*area];
                *self.attachment.borrow_mut() = Some(Attachment { area, rect });
            }
            self.refresh_pointer();
        }
    }
}

pub(crate) struct HoverButtons {
    state: Rc<TrackingState>,
    owner: Retained<AnyObject>,
    notifications: Retained<AnyObject>,
}

impl HoverButtons {
    pub(crate) fn install(
        window: &Window,
        enabled: bool,
        proxy: EventLoopProxy<Wake>,
    ) -> Result<Self> {
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("native window buttons require an AppKit window");
        };
        unsafe {
            let content = handle.ns_view.as_ptr().cast::<AnyObject>();
            let window: *mut AnyObject = msg_send![content, window];
            let window = Retained::retain(window).context("native window unavailable")?;
            let content = Retained::retain(content).context("native terminal view unavailable")?;
            let owner: Retained<AnyObject> = msg_send_id![owner_class(), new];
            let overlay = msg_send_id![overlay_class(), alloc];
            let overlay: Retained<AnyObject> = msg_send_id![overlay, initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(88.0, 40.0))];
            let _: () = msg_send![&*overlay, setWantsLayer: Bool::YES];
            let mut buttons = Vec::with_capacity(3);
            let mut x = 12.0;
            for (kind, label) in ["Close Window", "Minimize", "Full Screen"]
                .iter()
                .enumerate()
            {
                let button: Option<Retained<AnyObject>> = msg_send_id![class!(NSWindow), standardWindowButton: kind forStyleMask: 15_usize];
                let button = button.context("AppKit standard window button unavailable")?;
                let frame: NSRect = msg_send![&*button, frame];
                let size = NSSize::new(
                    frame.size.width.clamp(14.0, 20.0),
                    frame.size.height.clamp(14.0, 20.0),
                );
                let _: () = msg_send![&*button, setFrame: NSRect::new(NSPoint::new(x, 12.0), size)];
                let _: () = msg_send![&*button, setTag: kind as isize];
                let _: () = msg_send![&*button, setTarget: &*owner];
                let _: () = msg_send![&*button, setAction: sel!(performWindowControl:)];
                let _: () = msg_send![&*button, setEnabled: Bool::YES];
                let _: () = msg_send![&*button, setHidden: Bool::YES];
                let _: () = msg_send![&*button, setAccessibilityLabel: &*NSString::from_str(label)];
                let _: () = msg_send![&*overlay, addSubview: &*button];
                x += size.width + 6.0;
                buttons.push(button);
            }
            let _: () = msg_send![&*content, addSubview: &*overlay positioned: 1_isize relativeTo: std::ptr::null::<AnyObject>()];
            let notifications: Retained<AnyObject> =
                msg_send_id![class!(NSNotificationCenter), defaultCenter];
            let state = Rc::new(TrackingState {
                window,
                content,
                overlay,
                buttons,
                proxy,
                enabled: Cell::new(enabled),
                visible: Cell::new(false),
                attachment: RefCell::new(None),
                drag: RefCell::new(None),
                mouse_downs: Cell::new(0),
                drag_events: Cell::new(0),
                fallback_moves: Cell::new(0),
            });
            OWNERS.with(|owners| {
                let mut owners = owners.borrow_mut();
                owners.insert(&*owner as *const AnyObject as usize, Rc::downgrade(&state));
                owners.insert(
                    &*state.overlay as *const AnyObject as usize,
                    Rc::downgrade(&state),
                );
            });
            for name in [
                "NSWindowDidResizeNotification",
                "NSWindowDidEnterFullScreenNotification",
                "NSWindowDidExitFullScreenNotification",
                "NSWindowDidBecomeKeyNotification",
            ] {
                let _: () = msg_send![&*notifications, addObserver: &*owner selector: sel!(windowGeometryChanged:) name: &*NSString::from_str(name) object: &*state.window];
            }
            for name in [
                "NSWindowDidResignKeyNotification",
                "NSWindowDidMiniaturizeNotification",
                "NSWindowWillCloseNotification",
            ] {
                let _: () = msg_send![&*notifications, addObserver: &*owner selector: sel!(windowBecameInactive:) name: &*NSString::from_str(name) object: &*state.window];
            }
            state.update(&owner);
            state.apply_visibility(false, true);
            Ok(Self {
                state,
                owner,
                notifications,
            })
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.state.drag.borrow_mut().take();
        self.state.enabled.set(enabled);
        self.state.update(&self.owner);
    }

    pub(crate) fn drag_region_contains(&self, position: PhysicalPosition<f64>, scale: f64) -> bool {
        if !self.state.enabled.get() || !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        let x = position.x / scale;
        let y = position.y / scale;
        x >= 0.0 && x < 88.0 && y >= 0.0 && y < 40.0
    }

    pub(crate) fn pointer_moved(&self, position: PhysicalPosition<f64>, scale: f64) {
        self.state
            .set_visible(self.drag_region_contains(position, scale));
    }

    pub(crate) fn diagnostics(&self) -> serde_json::Value {
        unsafe {
            let movable: Bool = msg_send![&*self.state.window, isMovable];
            let frame: NSRect = msg_send![&*self.state.overlay, frame];
            serde_json::json!({
                "enabled": self.state.enabled.get(),
                "visible": self.state.visible.get(),
                "movable": movable.as_bool(),
                "hotspot_rect_points": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
                "mouse_down_count": self.state.mouse_downs.get(),
                "mouse_dragged_count": self.state.drag_events.get(),
                "fallback_move_count": self.state.fallback_moves.get(),
            })
        }
    }

    pub(crate) fn pointer_left(&self) {
        // Entering an AppKit button can leave winit's content view while the
        // pointer remains inside the activation corner.
        self.state.refresh_pointer();
    }
}

impl Drop for HoverButtons {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.notifications, removeObserver: &*self.owner];
        }
        self.state.remove_tracking();
        self.state.apply_visibility(false, true);
        unsafe {
            let _: () = msg_send![&*self.state.overlay, removeFromSuperview];
        }
        OWNERS.with(|owners| {
            let mut owners = owners.borrow_mut();
            owners.remove(&(&*self.owner as *const AnyObject as usize));
            owners.remove(&(&*self.state.overlay as *const AnyObject as usize));
        });
    }
}

// FVPROV1 19969 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNGFmM2Y0ZmY5OGM3OTMxMmU3MWIxN2M1MmZiM2YxN2NlYTE1ODg3YzMyYTg1NTQ1ZjIyMzY5ZGU3YTUyN2MyNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOGVjN2ZlNWJhMzUyZmY2N2UwMTdhZThhYjdmMTI4MmY1MDFjYjcwYmM2ZWQ3MTZlNjBmOTFkYWI0NjRhYzUyOGZkMDNhMzhhMmU5YTQ2N2UyOGM3NmI3MmI4YzE0ZjViMWNjMDU3MGY3YzM1ZGI2YjA5NTNhZDI3YTI4OGRmMDAifV19XQ== FVPROV1-END
