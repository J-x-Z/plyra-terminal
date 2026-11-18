// SPDX-License-Identifier: GPL-3.0-only
//! AppKit menu commands and nonblocking termination confirmation.

use std::{
    cell::{Cell, RefCell},
    sync::OnceLock,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use block2::RcBlock;
use objc2::{
    class,
    declare::ClassBuilder,
    encode::{Encoding, RefEncode},
    msg_send, msg_send_id,
    rc::Retained,
    runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Sel},
    sel,
};
use objc2_foundation::NSString;
use winit::{
    event::KeyEvent,
    event_loop::EventLoopProxy,
    keyboard::ModifiersState,
    platform::scancode::PhysicalKeyExtScancode,
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::Window,
};

use crate::{
    config::{Chord, Settings, Shortcut},
    Wake,
};

const COMMAND: usize = 1 << 20;
const CONTROL: usize = 1 << 18;
const OPTION: usize = 1 << 19;
const SHIFT: usize = 1 << 17;
const KEY_DOWN: usize = 10;
const KEY_UP: usize = 11;
const FLAGS_CHANGED: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuitChord {
    key_code: u16,
    modifiers: u8,
}

impl QuitChord {
    pub(crate) fn from_key_event(event: &KeyEvent, modifiers: ModifiersState) -> Option<Self> {
        Some(Self {
            key_code: event.physical_key.to_scancode()?.try_into().ok()?,
            modifiers: modifier_bits(modifiers),
        })
    }
}

#[derive(Clone, Copy)]
struct FirstQuitPress {
    chord: QuitChord,
    deadline: Instant,
    released: bool,
}

#[derive(Default)]
pub(crate) struct QuitDoublePress {
    first: Option<FirstQuitPress>,
}

impl QuitDoublePress {
    pub(crate) fn press(
        &mut self,
        chord: QuitChord,
        now: Instant,
        interval: Duration,
    ) -> bool {
        if let Some(first) = self.first {
            if first.chord == chord && !first.released {
                return false;
            }
            if first.chord == chord && now < first.deadline {
                self.reset();
                return true;
            }
        }
        self.first = Some(FirstQuitPress {
            chord,
            deadline: now + interval,
            released: false,
        });
        false
    }

    pub(crate) fn release(&mut self) {
        if let Some(first) = &mut self.first {
            first.released = true;
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.first.map(|first| first.deadline)
    }

    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        if self.first.is_some_and(|first| now >= first.deadline) {
            self.reset();
            return true;
        }
        false
    }

    pub(crate) fn reset(&mut self) {
        self.first = None;
    }
}

fn modifier_bits(modifiers: ModifiersState) -> u8 {
    u8::from(modifiers.super_key())
        | (u8::from(modifiers.control_key()) << 1)
        | (u8::from(modifiers.alt_key()) << 2)
        | (u8::from(modifiers.shift_key()) << 3)
}

fn native_modifier_bits(flags: usize) -> u8 {
    u8::from(flags & COMMAND != 0)
        | (u8::from(flags & CONTROL != 0) << 1)
        | (u8::from(flags & OPTION != 0) << 2)
        | (u8::from(flags & SHIFT != 0) << 3)
}

#[derive(Clone, Copy, Debug)]
#[repr(isize)]
pub enum MenuCommand {
    NewTab,
    SplitRight,
    SplitBelow,
    ClosePane,
    CloseTab,
    CloseWindow,
    Settings,
    ReloadConfig,
    Copy,
    Paste,
    SelectAll,
    Find,
    Commands,
    Mcp,
    SystemPanel,
    IncreaseFont,
    DecreaseFont,
    ResetFont,
    Minimize,
    Zoom,
    Fullscreen,
    BringToFront,
    Quit,
}

const COMMANDS: &[MenuCommand] = &[
    MenuCommand::NewTab,
    MenuCommand::SplitRight,
    MenuCommand::SplitBelow,
    MenuCommand::ClosePane,
    MenuCommand::CloseTab,
    MenuCommand::CloseWindow,
    MenuCommand::Settings,
    MenuCommand::ReloadConfig,
    MenuCommand::Copy,
    MenuCommand::Paste,
    MenuCommand::SelectAll,
    MenuCommand::Find,
    MenuCommand::Commands,
    MenuCommand::Mcp,
    MenuCommand::SystemPanel,
    MenuCommand::IncreaseFont,
    MenuCommand::DecreaseFont,
    MenuCommand::ResetFont,
    MenuCommand::Minimize,
    MenuCommand::Zoom,
    MenuCommand::Fullscreen,
    MenuCommand::BringToFront,
    MenuCommand::Quit,
];

thread_local! {
    static PROXY: RefCell<Option<EventLoopProxy<Wake>>> = const { RefCell::new(None) };
    static QUIT_CHORD: RefCell<Option<QuitChord>> = const { RefCell::new(None) };
    static QUIT_SHORTCUT: RefCell<Option<Chord>> = const { RefCell::new(None) };
    static TRACKED_QUIT: RefCell<Option<(QuitChord, bool, Option<Instant>)>> = const { RefCell::new(None) };
    static QUIT_TRACKING_GENERATION: Cell<u64> = const { Cell::new(0) };
}

fn send(event: Wake) {
    PROXY.with(|proxy| {
        if let Some(proxy) = proxy.borrow().as_ref() {
            let _ = proxy.send_event(event);
        }
    });
}

fn cancel_menu_tracking() {
    unsafe {
        let application: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let menu: *mut AnyObject = msg_send![application, mainMenu];
        let Some(menu) = Retained::retain(menu) else {
            return;
        };
        let items: Retained<AnyObject> = msg_send_id![&*menu, itemArray];
        let count: usize = msg_send![&*items, count];
        for index in 0..count {
            let item: *mut AnyObject = msg_send![&*items, objectAtIndex: index];
            let submenu: *mut AnyObject = msg_send![item, submenu];
            if !submenu.is_null() {
                let _: () = msg_send![submenu, cancelTrackingWithoutAnimation];
            }
        }
        let _: () = msg_send![&*menu, cancelTrackingWithoutAnimation];
    }
}

extern "C" fn perform_action(_: &AnyObject, _: Sel, item: &AnyObject) {
    let tag: isize = unsafe { msg_send![item, tag] };
    let Some(command) = COMMANDS
        .iter()
        .copied()
        .find(|command| *command as isize == tag)
    else {
        return;
    };
    if matches!(command, MenuCommand::Quit) {
        let application: *mut AnyObject =
            unsafe { msg_send![class!(NSApplication), sharedApplication] };
        let event: *mut AnyObject = unsafe { msg_send![application, currentEvent] };
        if let Some(chord) = equivalent_quit_chord(event) {
            let repeat: Bool = unsafe { msg_send![event, isARepeat] };
            dispatch_tracked_quit(chord, repeat.as_bool(), key_event_time(event));
            return;
        }
        cancel_menu_tracking();
    }
    send(Wake::MacMenu(command));
}

fn dispatch_tracked_quit(chord: QuitChord, repeat: bool, pressed_at: Option<Instant>) {
    // The winit loop must resume before it can handle the queued command.
    cancel_menu_tracking();
    queue_quit_press(chord, repeat, pressed_at);
}

fn key_event_time(event: *mut AnyObject) -> Option<Instant> {
    if event.is_null() {
        return None;
    }
    // AppKit time stamps and systemUptime use elapsed system time. Convert
    // the event's age instead of timing a press or release when the UI drains it.
    unsafe {
        let timestamp: f64 = msg_send![event, timestamp];
        if timestamp <= 0.0 {
            return None;
        }
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        let uptime: f64 = msg_send![info, systemUptime];
        let age = Duration::try_from_secs_f64(uptime - timestamp).ok()?;
        Instant::now().checked_sub(age)
    }
}

pub(crate) fn current_key_down_time() -> Option<Instant> {
    unsafe {
        let application: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let event: *mut AnyObject = msg_send![application, currentEvent];
        let kind: usize = msg_send![event, type];
        if kind == KEY_DOWN {
            key_event_time(event)
        } else {
            None
        }
    }
}

pub(crate) fn queue_quit_press(chord: QuitChord, repeat: bool, pressed_at: Option<Instant>) {
    if track_quit_press(chord, repeat) {
        send(Wake::MacQuitKeyDown(
            chord,
            pressed_at.unwrap_or_else(Instant::now),
            quit_tracking_generation(),
        ));
    }
}

fn track_quit_press(chord: QuitChord, repeat: bool) -> bool {
    if repeat {
        return false;
    }
    QUIT_CHORD.with(|state| {
        let mut state = state.borrow_mut();
        if *state == Some(chord) {
            return false;
        }
        *state = Some(chord);
        true
    })
}

pub(crate) fn reset_quit_tracking() {
    QUIT_CHORD.with(|state| *state.borrow_mut() = None);
    TRACKED_QUIT.with(|state| *state.borrow_mut() = None);
    QUIT_TRACKING_GENERATION.with(|state| state.set(state.get().wrapping_add(1)));
}

pub(crate) fn quit_tracking_generation() -> u64 {
    QUIT_TRACKING_GENERATION.with(Cell::get)
}

extern "C" fn perform_tracked_quit(_: &AnyObject, _: Sel, _: *mut AnyObject) {
    let pending = TRACKED_QUIT.with(|state| state.borrow_mut().take());
    if let Some((chord, repeat, pressed_at)) = pending {
        dispatch_tracked_quit(chord, repeat, pressed_at);
    }
}

#[repr(transparent)]
struct SelectorOut(Option<Sel>);

// Option<Sel> has Objective-C's nullable SEL representation. Its out pointer
// must retain the SDK's SEL* encoding even though objc2 omits RefEncode for Sel.
unsafe impl RefEncode for SelectorOut {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Sel);
}

extern "C" fn menu_key_equivalent(
    owner: &AnyObject,
    _: Sel,
    _: &AnyObject,
    event: *mut AnyObject,
    target: *mut *mut AnyObject,
    action: *mut SelectorOut,
) -> Bool {
    let Some(chord) = equivalent_quit_chord(event) else {
        return Bool::NO;
    };
    if target.is_null() || action.is_null() {
        return Bool::NO;
    }
    unsafe {
        let repeat: Bool = msg_send![event, isARepeat];
        TRACKED_QUIT.with(|state| {
            *state.borrow_mut() = Some((chord, repeat.as_bool(), key_event_time(event)));
        });
        *target = owner as *const AnyObject as *mut AnyObject;
        *action = SelectorOut(Some(sel!(performTrackedQuit:)));
    }
    Bool::YES
}

extern "C" fn menu_perform_key_equivalent(menu: &AnyObject, _: Sel, event: *mut AnyObject) -> Bool {
    if let Some(chord) = equivalent_quit_chord(event) {
        let repeat: Bool = unsafe { msg_send![event, isARepeat] };
        dispatch_tracked_quit(chord, repeat.as_bool(), key_event_time(event));
        return Bool::YES;
    }
    unsafe { msg_send![super(menu, class!(NSMenu)), performKeyEquivalent: event] }
}

fn menu_class() -> &'static AnyClass {
    static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("PlyraTerminalMenu", class!(NSMenu))
            .expect("unique terminal menu class");
        unsafe {
            builder.add_method(
                sel!(performKeyEquivalent:),
                menu_perform_key_equivalent as extern "C" fn(_, _, _) -> _,
            );
        }
        builder.register()
    })
}

fn target_class() -> &'static AnyClass {
    static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("PlyraTerminalMenuTarget", class!(NSObject))
            .expect("unique menu target class");
        builder.add_protocol(
            AnyProtocol::get("NSMenuDelegate").expect("AppKit menu delegate protocol"),
        );
        unsafe {
            builder.add_method(
                sel!(menuHasKeyEquivalent:forEvent:target:action:),
                menu_key_equivalent as extern "C" fn(_, _, _, _, _, _) -> _,
            );
            builder.add_method(
                sel!(performTrackedQuit:),
                perform_tracked_quit as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(performTerminalMenuAction:),
                perform_action as extern "C" fn(_, _, _),
            );
        }
        builder.register()
    })
}

fn system_termination() -> bool {
    unsafe {
        let manager: *mut AnyObject =
            msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
        let event: *mut AnyObject = msg_send![manager, currentAppleEvent];
        if event.is_null() {
            return false;
        }
        // AERegistry.h documents kAEQuitReason and these system quit reasons.
        let reason: *mut AnyObject =
            msg_send![event, paramDescriptorForKeyword: u32::from_be_bytes(*b"why?")];
        if reason.is_null() {
            return false;
        }
        let code: u32 = msg_send![reason, enumCodeValue];
        [*b"shut", *b"rest", *b"rlgo", *b"quia"]
            .iter()
            .any(|reason| code == u32::from_be_bytes(*reason))
    }
}

extern "C" fn should_terminate(_: &AnyObject, _: Sel, _: &AnyObject) -> usize {
    if system_termination() {
        return 1; // NSTerminateNow lets winit deliver its normal cleanup callbacks.
    }
    send(Wake::MacMenu(MenuCommand::Quit));
    0 // NSTerminateCancel keeps the event loop alive for our confirmation sheet.
}

fn protected_delegate_class(superclass: &'static AnyClass) -> &'static AnyClass {
    static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder =
            ClassBuilder::new("PlyraTerminalProtectedApplicationDelegate", superclass)
                .expect("unique protected application delegate class");
        // This subclass adds no ivars and preserves winit's delegate methods.
        unsafe {
            builder.add_method(
                sel!(applicationShouldTerminate:),
                should_terminate as extern "C" fn(_, _, _) -> _,
            );
        }
        builder.register()
    })
}

static KEY_ROUTING_CLASS: OnceLock<&'static AnyClass> = OnceLock::new();

extern "C" fn route_application_key(application: &AnyObject, _: Sel, event: *mut AnyObject) {
    unsafe {
        let kind: usize = msg_send![event, type];
        if kind == KEY_DOWN {
            if let Some(chord) = equivalent_quit_chord(event) {
                let repeat: Bool = msg_send![event, isARepeat];
                dispatch_tracked_quit(chord, repeat.as_bool(), key_event_time(event));
                return;
            }
        } else if kind == KEY_UP || kind == FLAGS_CHANGED {
            let released = if kind == KEY_UP {
                let key: u16 = msg_send![event, keyCode];
                observe_quit_release(KEY_UP, key, 0)
            } else {
                let flags: usize = msg_send![event, modifierFlags];
                observe_quit_release(FLAGS_CHANGED, 0, native_modifier_bits(flags))
            };
            if let Some(chord) = released {
                send(Wake::MacQuitKeyReleased(
                    chord,
                    key_event_time(event),
                    quit_tracking_generation(),
                ));
            }
        }
        let superclass = KEY_ROUTING_CLASS
            .get()
            .and_then(|class| class.superclass())
            .expect("installed quit key routing class");
        let _: () = msg_send![super(application, superclass), sendEvent: event];
    }
}

fn key_routing_class(superclass: &'static AnyClass) -> &'static AnyClass {
    KEY_ROUTING_CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("PlyraTerminalKeyRoutingApplication", superclass)
            .expect("unique quit key routing class");
        // Observe releases before winit sends Cmd-held key-up straight to the
        // key window, bypassing NSEvent local monitors and sometimes the view.
        unsafe {
            builder.add_method(
                sel!(sendEvent:),
                route_application_key as extern "C" fn(_, _, _),
            );
        }
        builder.register()
    })
}

fn menu(title: &str) -> Retained<AnyObject> {
    unsafe {
        let value: Retained<AnyObject> = msg_send_id![menu_class(), new];
        let _: () = msg_send![&*value, setTitle: &*NSString::from_str(title)];
        let _: () = msg_send![&*value, setAutoenablesItems: objc2::runtime::Bool::NO];
        value
    }
}

fn item(title: &str, selector: Option<Sel>, key: &str, modifiers: usize) -> Retained<AnyObject> {
    unsafe {
        let value = msg_send_id![class!(NSMenuItem), alloc];
        let value: Retained<AnyObject> = msg_send_id![value,
            initWithTitle: &*NSString::from_str(title)
            action: selector
            keyEquivalent: &*NSString::from_str(key)
        ];
        let _: () = msg_send![&*value, setKeyEquivalentModifierMask: modifiers];
        value
    }
}

fn add(menu: &AnyObject, item: &AnyObject) {
    unsafe {
        let _: () = msg_send![menu, addItem: item];
    }
}

fn separator(menu: &AnyObject) {
    unsafe {
        let item: Retained<AnyObject> = msg_send_id![class!(NSMenuItem), separatorItem];
        add(menu, &item);
    }
}

fn submenu(menubar: &AnyObject, title: &str) -> Retained<AnyObject> {
    let value = menu(title);
    let parent = item(title, None, "", 0);
    unsafe {
        let _: () = msg_send![&*parent, setSubmenu: &*value];
    }
    add(menubar, &parent);
    value
}

fn command_item(
    menu: &AnyObject,
    target: &AnyObject,
    title: &str,
    command: MenuCommand,
    key: &str,
    modifiers: usize,
) {
    let value = item(
        title,
        Some(sel!(performTerminalMenuAction:)),
        key,
        modifiers,
    );
    unsafe {
        let _: () = msg_send![&*value, setTag: command as isize];
        let _: () = msg_send![&*value, setTarget: target];
    }
    add(menu, &value);
}

fn configured_shortcut(settings: &Settings, shortcut: Shortcut) -> Option<Chord> {
    settings
        .keybindings
        .iter()
        .find(|binding| binding.action == shortcut)
        .and_then(|binding| Chord::parse(&binding.keys).ok())
}

fn shortcut_item(
    menu: &AnyObject,
    target: &AnyObject,
    settings: &Settings,
    title: &str,
    command: MenuCommand,
    shortcut: Shortcut,
) {
    let chord = configured_shortcut(settings, shortcut);
    let (key, modifiers) = chord.map_or((String::new(), 0), |chord| {
        let modifiers = usize::from(chord.modifiers & 1 != 0) * COMMAND
            | usize::from(chord.modifiers & 2 != 0) * CONTROL
            | usize::from(chord.modifiers & 4 != 0) * OPTION
            | usize::from(chord.modifiers & 8 != 0) * SHIFT;
        (key_equivalent(&chord.key).to_owned(), modifiers)
    });
    command_item(menu, target, title, command, &key, modifiers);
}

fn key_equivalent(key: &str) -> &str {
    match key {
        "escape" => "\u{1b}",
        "enter" => "\r",
        "tab" => "\t",
        "space" => " ",
        "backspace" => "\u{7f}",
        "delete" => "\u{f728}",
        "pageup" => "\u{f72c}",
        "pagedown" => "\u{f72d}",
        "arrowup" => "\u{f700}",
        "arrowdown" => "\u{f701}",
        "arrowleft" => "\u{f702}",
        "arrowright" => "\u{f703}",
        _ => key,
    }
}

struct Confirmation {
    // Release the callback before its captured AppKit objects.
    completion: RcBlock<dyn Fn(isize)>,
    alert: Retained<AnyObject>,
    sheet: Retained<AnyObject>,
    parent: Retained<AnyObject>,
}

impl Confirmation {
    unsafe fn new(parent: *mut AnyObject, proxy: EventLoopProxy<Wake>) -> Result<Self> {
        let parent =
            unsafe { Retained::retain(parent) }.context("macOS sheet parent unavailable")?;
        let alert: Retained<AnyObject> = unsafe { msg_send_id![class!(NSAlert), new] };
        let sheet: *mut AnyObject = unsafe { msg_send![&*alert, window] };
        let sheet = unsafe { Retained::retain(sheet) }.context("macOS alert window unavailable")?;
        unsafe {
            // This is our alert's window. Winit retains ownership of its parent.
            let _: () = msg_send![&*sheet, setReleasedWhenClosed: objc2::runtime::Bool::NO];
            let _: () =
                msg_send![&*alert, setMessageText: &*NSString::from_str("Quit Plyra Terminal?")];
            let _: *mut AnyObject =
                msg_send![&*alert, addButtonWithTitle: &*NSString::from_str("Cancel")];
            let _: *mut AnyObject =
                msg_send![&*alert, addButtonWithTitle: &*NSString::from_str("Quit")];
        }
        let alert_guard = alert.clone();
        let sheet_guard = sheet.clone();
        let parent_guard = parent.clone();
        let completion = RcBlock::new(move |response: isize| {
            let _keep_alive = (&alert_guard, &sheet_guard, &parent_guard);
            let _ = proxy.send_event(Wake::MacQuitDecision(response == 1001));
        });
        Ok(Self {
            completion,
            alert,
            sheet,
            parent,
        })
    }
}

pub(crate) struct NativeMenu {
    confirmation: Option<Confirmation>,
    confirmation_active: bool,
    menu: Retained<AnyObject>,
    target: Retained<AnyObject>,
    application: Retained<AnyObject>,
    original_application_class: &'static AnyClass,
    delegate: Retained<AnyObject>,
    original_delegate_class: &'static AnyClass,
    original_menu: Option<Retained<AnyObject>>,
    original_windows_menu: Option<Retained<AnyObject>>,
    proxy: EventLoopProxy<Wake>,
}

impl NativeMenu {
    pub(crate) fn install(proxy: EventLoopProxy<Wake>, settings: &Settings) -> Result<Self> {
        unsafe {
            let application: Retained<AnyObject> =
                msg_send_id![class!(NSApplication), sharedApplication];
            let original_application_class = application.class();
            let key_routing = key_routing_class(original_application_class);
            ensure!(
                key_routing
                    .superclass()
                    .is_some_and(|class| std::ptr::eq(class, original_application_class))
                    && key_routing.instance_size() == original_application_class.instance_size(),
                "incompatible macOS application layout"
            );
            let delegate: *mut AnyObject = msg_send![&*application, delegate];
            let delegate =
                Retained::retain(delegate).context("macOS application delegate unavailable")?;
            let original_delegate_class = delegate.class();
            let protected = protected_delegate_class(original_delegate_class);
            ensure!(
                protected.instance_size() == original_delegate_class.instance_size(),
                "incompatible macOS application delegate layout"
            );
            let previous: *mut AnyObject = msg_send![&*application, mainMenu];
            let previous_windows: *mut AnyObject = msg_send![&*application, windowsMenu];
            let target: Retained<AnyObject> = msg_send_id![target_class(), new];
            let mut value = Self {
                menu: menu("Plyra Terminal"),
                target,
                application,
                original_application_class,
                delegate,
                original_delegate_class,
                original_menu: Retained::retain(previous),
                confirmation: None,
                confirmation_active: false,
                original_windows_menu: Retained::retain(previous_windows),
                proxy: proxy.clone(),
            };
            PROXY.with(|slot| *slot.borrow_mut() = Some(proxy));
            AnyObject::set_class(&value.delegate, protected);
            AnyObject::set_class(&value.application, key_routing);
            value.rebuild(settings);
            Ok(value)
        }
    }

    pub(crate) fn rebuild(&mut self, settings: &Settings) {
        reset_quit_tracking();
        QUIT_SHORTCUT
            .with(|state| *state.borrow_mut() = configured_shortcut(settings, Shortcut::Quit));
        let menubar = menu("Plyra Terminal");
        let app = submenu(&menubar, "Plyra Terminal");
        add(
            &app,
            &item(
                "About Plyra Terminal",
                Some(sel!(orderFrontStandardAboutPanel:)),
                "",
                0,
            ),
        );
        separator(&app);
        shortcut_item(
            &app,
            &self.target,
            settings,
            "Settings…",
            MenuCommand::Settings,
            Shortcut::Settings,
        );
        shortcut_item(
            &app,
            &self.target,
            settings,
            "Reload Configuration",
            MenuCommand::ReloadConfig,
            Shortcut::ReloadConfig,
        );
        separator(&app);
        add(
            &app,
            &item("Hide Plyra Terminal", Some(sel!(hide:)), "h", COMMAND),
        );
        add(
            &app,
            &item(
                "Hide Others",
                Some(sel!(hideOtherApplications:)),
                "h",
                COMMAND | OPTION,
            ),
        );
        add(
            &app,
            &item("Show All", Some(sel!(unhideAllApplications:)), "", 0),
        );
        separator(&app);
        shortcut_item(
            &app,
            &self.target,
            settings,
            "Quit Plyra Terminal…",
            MenuCommand::Quit,
            Shortcut::Quit,
        );

        let session = submenu(&menubar, "Shell");
        for (title, command, shortcut) in [
            ("New Tab", MenuCommand::NewTab, Shortcut::NewShell),
            ("Split Right", MenuCommand::SplitRight, Shortcut::SplitRight),
            ("Split Below", MenuCommand::SplitBelow, Shortcut::SplitBelow),
            ("Close Pane", MenuCommand::ClosePane, Shortcut::ClosePane),
            ("Close Tab", MenuCommand::CloseTab, Shortcut::CloseTab),
        ] {
            shortcut_item(&session, &self.target, settings, title, command, shortcut);
        }
        separator(&session);
        command_item(
            &session,
            &self.target,
            "Close Window…",
            MenuCommand::CloseWindow,
            "",
            0,
        );

        let edit = submenu(&menubar, "Edit");
        shortcut_item(
            &edit,
            &self.target,
            settings,
            "Copy",
            MenuCommand::Copy,
            Shortcut::Copy,
        );
        shortcut_item(
            &edit,
            &self.target,
            settings,
            "Paste",
            MenuCommand::Paste,
            Shortcut::Paste,
        );
        command_item(
            &edit,
            &self.target,
            "Select All",
            MenuCommand::SelectAll,
            "a",
            COMMAND,
        );
        separator(&edit);
        shortcut_item(
            &edit,
            &self.target,
            settings,
            "Find…",
            MenuCommand::Find,
            Shortcut::Search,
        );

        let view = submenu(&menubar, "View");
        command_item(
            &view,
            &self.target,
            "Increase Font Size",
            MenuCommand::IncreaseFont,
            "",
            0,
        );
        command_item(
            &view,
            &self.target,
            "Decrease Font Size",
            MenuCommand::DecreaseFont,
            "",
            0,
        );
        command_item(
            &view,
            &self.target,
            "Reset Font Size",
            MenuCommand::ResetFont,
            "",
            0,
        );
        separator(&view);
        command_item(
            &view,
            &self.target,
            "System Panel",
            MenuCommand::SystemPanel,
            "",
            0,
        );
        command_item(&view, &self.target, "MCP", MenuCommand::Mcp, "", 0);
        shortcut_item(
            &view,
            &self.target,
            settings,
            "Commands…",
            MenuCommand::Commands,
            Shortcut::Commands,
        );

        let window = submenu(&menubar, "Window");
        command_item(
            &window,
            &self.target,
            "Minimize",
            MenuCommand::Minimize,
            "m",
            COMMAND,
        );
        command_item(&window, &self.target, "Zoom", MenuCommand::Zoom, "", 0);
        command_item(
            &window,
            &self.target,
            "Toggle Full Screen",
            MenuCommand::Fullscreen,
            "f",
            COMMAND | CONTROL,
        );
        separator(&window);
        command_item(
            &window,
            &self.target,
            "Bring to Front",
            MenuCommand::BringToFront,
            "",
            0,
        );

        unsafe {
            // These menus are owned here. NSMenu retains its items but keeps a
            // weak delegate, while NativeMenu retains the shared action target.
            for menu in [&menubar, &app, &session, &edit, &view, &window] {
                let _: () = msg_send![&**menu, setDelegate: &*self.target];
            }
            let application: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![application, setMainMenu: &*menubar];
            let _: () = msg_send![application, setWindowsMenu: &*window];
        }
        self.menu = menubar;
    }

    pub(crate) fn confirm_quit(&mut self, window: &Window, session_count: usize) -> Result<()> {
        cancel_menu_tracking();
        if self.confirmation_active {
            return Ok(());
        }
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("macOS quit confirmation requires an AppKit window");
        };
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let parent: *mut AnyObject = msg_send![view, window];
            ensure!(
                !parent.is_null(),
                "macOS quit confirmation window unavailable"
            );
            if self
                .confirmation
                .as_ref()
                .is_some_and(|dialog| !std::ptr::eq(&*dialog.parent, parent))
            {
                self.confirmation = None;
            }
            if self.confirmation.is_none() {
                self.confirmation = Some(Confirmation::new(parent, self.proxy.clone())?);
            }
            let dialog = self.confirmation.as_ref().unwrap();
            let text = format!("All {session_count} terminal sessions will close.");
            let _: () = msg_send![&*dialog.alert, setInformativeText: &*NSString::from_str(&text)];
            self.confirmation_active = true;
            let _: () = msg_send![&*dialog.alert, beginSheetModalForWindow: parent completionHandler: &*dialog.completion];
        }
        Ok(())
    }

    pub(crate) fn finish_confirmation(&mut self) {
        // AppKit dismisses a sheet after its completion handler returns. Reuse
        // the owned alert instead of releasing its glass window from that path.
        self.confirmation_active = false;
    }
}

impl Drop for NativeMenu {
    fn drop(&mut self) {
        reset_quit_tracking();
        QUIT_SHORTCUT.with(|state| *state.borrow_mut() = None);
        PROXY.with(|slot| *slot.borrow_mut() = None);
        unsafe {
            let application = &*self.application;
            if std::ptr::eq(
                application.class(),
                key_routing_class(self.original_application_class),
            ) {
                AnyObject::set_class(application, self.original_application_class);
            }
            if let Some(dialog) = &self.confirmation {
                let parent: *mut AnyObject = msg_send![&*dialog.sheet, sheetParent];
                if !parent.is_null() {
                    let _: () = msg_send![parent, endSheet: &*dialog.sheet returnCode: 1000_isize];
                }
            }
            let previous_windows = self
                .original_windows_menu
                .as_deref()
                .map_or(std::ptr::null(), |menu| menu as *const AnyObject);
            let _: () = msg_send![application, setWindowsMenu: previous_windows];
            let previous = self
                .original_menu
                .as_deref()
                .map_or(std::ptr::null(), |menu| menu as *const AnyObject);
            let _: () = msg_send![application, setMainMenu: previous];
            if std::ptr::eq(
                self.delegate.class(),
                protected_delegate_class(self.original_delegate_class),
            ) {
                AnyObject::set_class(&self.delegate, self.original_delegate_class);
            }
        }
    }
}

fn equivalent_quit_chord(event: *mut AnyObject) -> Option<QuitChord> {
    if event.is_null() {
        return None;
    }
    let kind: usize = unsafe { msg_send![event, type] };
    if kind != KEY_DOWN {
        return None;
    }
    let flags: usize = unsafe { msg_send![event, modifierFlags] };
    let modifiers = native_modifier_bits(flags);
    QUIT_SHORTCUT.with(|state| {
        let state = state.borrow();
        let shortcut = state.as_ref()?;
        if shortcut.modifiers != modifiers {
            return None;
        }
        unsafe {
            let characters: *mut AnyObject = msg_send![event, charactersIgnoringModifiers];
            if characters.is_null() {
                return None;
            }
            let characters: *mut AnyObject = msg_send![characters, lowercaseString];
            let matches: objc2::runtime::Bool = msg_send![characters,
                isEqualToString: &*NSString::from_str(key_equivalent(&shortcut.key))];
            matches.as_bool().then(|| QuitChord {
                key_code: msg_send![event, keyCode],
                modifiers,
            })
        }
    })
}

pub(crate) fn quit_chord_is_pressed(chord: QuitChord) -> bool {
    QUIT_CHORD.with(|state| *state.borrow() == Some(chord))
}

fn observe_quit_release(kind: usize, key: u16, modifiers: u8) -> Option<QuitChord> {
    QUIT_CHORD.with(|state| {
        state.borrow_mut().take_if(|chord| {
            (kind == KEY_UP && chord.key_code == key)
                || (kind == FLAGS_CHANGED && chord.modifiers != modifiers)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_quit_accepts_a_second_press_while_command_stays_held() {
        let chord = QuitChord {
            key_code: 12,
            modifiers: 1,
        };
        let now = Instant::now();
        let interval = Duration::from_millis(800);
        let mut state = QuitDoublePress::default();
        assert!(track_quit_press(chord, false));
        assert!(!state.press(chord, now, interval));
        assert!(!track_quit_press(chord, true)); // Autorepeat.
        assert!(!track_quit_press(chord, false)); // Duplicate menu delivery.
        assert_eq!(observe_quit_release(KEY_UP, 13, 1), None); // Another key.
        assert_eq!(observe_quit_release(KEY_UP, 12, 1), Some(chord)); // Cmd remains held.
        state.release();
        assert!(!track_quit_press(chord, true));
        assert!(track_quit_press(chord, false));
        assert!(state.press(chord, now + Duration::from_millis(200), interval));
        assert!(state.deadline().is_none());
        reset_quit_tracking();
    }

    #[test]
    fn double_quit_timeout_restarts_the_first_press() {
        let chord = QuitChord {
            key_code: 12,
            modifiers: 1,
        };
        let now = Instant::now();
        let interval = Duration::from_millis(800);
        let mut state = QuitDoublePress::default();
        assert!(!state.press(chord, now, interval));
        state.release();
        assert!(!state.press(chord, now + interval, interval));
        assert_eq!(state.deadline(), Some(now + interval + interval));
        state.release();
        assert!(state.expire(now + interval + interval));
        assert!(state.deadline().is_none());
        assert!(!state.press(chord, now + interval + interval, interval));
    }

    #[test]
    fn double_quit_reset_and_changed_chord_require_a_new_pair() {
        let chord = QuitChord {
            key_code: 12,
            modifiers: 1,
        };
        let other = QuitChord {
            key_code: 12,
            modifiers: 1 | 8,
        };
        let now = Instant::now();
        let interval = Duration::from_millis(800);
        let mut state = QuitDoublePress::default();
        assert!(!state.press(chord, now, interval));
        state.release();
        state.reset(); // Window focus loss, suspend, or changed configuration.
        assert!(!state.press(chord, now + Duration::from_millis(100), interval));
        state.release();
        assert!(!state.press(other, now + Duration::from_millis(200), interval));
        state.release();
        assert!(state.press(other, now + Duration::from_millis(300), interval));
    }

    #[test]
    fn hold_quit_tracks_local_release_and_modifier_changes() {
        let chord = QuitChord {
            key_code: 12,
            modifiers: 1,
        };
        QUIT_CHORD.with(|state| *state.borrow_mut() = Some(chord));
        assert!(quit_chord_is_pressed(chord));
        assert_eq!(observe_quit_release(KEY_UP, 13, 1), None);
        assert_eq!(observe_quit_release(FLAGS_CHANGED, 0, 1), None);
        assert_eq!(observe_quit_release(KEY_UP, 12, 1), Some(chord));
        assert!(!quit_chord_is_pressed(chord));
        QUIT_CHORD.with(|state| *state.borrow_mut() = Some(chord));
        assert_eq!(observe_quit_release(FLAGS_CHANGED, 0, 0), Some(chord));
        assert!(!quit_chord_is_pressed(chord));
    }

    #[test]
    fn buffered_quit_release_requires_a_completed_hold_in_focus() {
        let chord = QuitChord {
            key_code: 12,
            modifiers: 1,
        };
        let other = QuitChord {
            key_code: 13,
            ..chord
        };
        let pressed_at = Instant::now();
        let duration = Duration::from_millis(1200);
        let hold = crate::QuitHold {
            deadline: pressed_at + duration * 2,
            release_deadline: pressed_at + duration,
            chord,
        };
        assert!(!hold.completed_by_release(chord, Some(pressed_at), true));
        assert!(!hold.completed_by_release(chord, None, true));
        assert!(!hold.completed_by_release(other, Some(hold.release_deadline), true));
        assert!(!hold.completed_by_release(chord, Some(hold.release_deadline), false));
        assert!(hold.completed_by_release(chord, Some(hold.release_deadline), true));

        let previous_generation = quit_tracking_generation();
        assert!(track_quit_press(chord, false));
        reset_quit_tracking();
        assert_ne!(quit_tracking_generation(), previous_generation);
        assert_eq!(observe_quit_release(KEY_UP, chord.key_code, 1), None);
    }

    #[test]
    fn hold_quit_requires_the_complete_configured_modifier_chord() {
        let command_shift = QuitChord {
            key_code: 12,
            modifiers: 1 | 8,
        };
        assert!(track_quit_press(command_shift, false));
        assert_eq!(
            observe_quit_release(FLAGS_CHANGED, 0, native_modifier_bits(COMMAND | SHIFT)),
            None,
        );
        assert_eq!(
            observe_quit_release(FLAGS_CHANGED, 0, native_modifier_bits(COMMAND)),
            Some(command_shift),
        );
        assert!(track_quit_press(command_shift, false));
        assert_eq!(
            observe_quit_release(
                FLAGS_CHANGED,
                0,
                native_modifier_bits(COMMAND | SHIFT | OPTION)
            ),
            Some(command_shift),
        );
        let control_option = QuitChord {
            key_code: 12,
            modifiers: 2 | 4,
        };
        assert_eq!(
            modifier_bits(ModifiersState::CONTROL | ModifiersState::ALT),
            control_option.modifiers
        );
        assert_eq!(
            native_modifier_bits(CONTROL | OPTION),
            control_option.modifiers
        );
        assert_eq!(
            native_modifier_bits(COMMAND | SHIFT),
            command_shift.modifiers
        );
    }
}

// FVPROV1 40023 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiN2NiMTkyYWJkMTY0YjM3MDBkNGMzYWNjN2UyYTRhM2YyZTE5ODg4ZWI5MzkwNGNiMmNkYWNjZDdmMTJkZjE3OCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4yIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjIiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMzYyMjYyY2I1MDllYzg3ODNjMGNmZDYwYTg3OGQ2YTNlZjczNzE1ODI3MGMwZGYwNDRlMTIyNDRmNGY0YTlmOTlhYzg2MTk4MTRhNDFiNGNmZTk1Yzg0ZDVlM2FiY2JiOTkwMGZhOTVjMzNiMzlkYmE4NGYxNGQxZGUzYjY5MGQifV19XQ== FVPROV1-END
