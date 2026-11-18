// SPDX-License-Identifier: GPL-3.0-only
#[cfg(target_os = "android")]
mod android_arch;
#[cfg(target_os = "android")]
pub mod android_input;
mod config;
mod diagnostics;
mod extensions;
#[cfg(target_os = "macos")]
pub mod macos_menu;
#[cfg(target_os = "macos")]
mod macos_window;
mod mcp_panel;
mod pane;
mod probe;
mod system_panel;
mod ui;
mod workspace;

use anyhow::{Context, Result};
use config::{Chord, Settings, Shortcut};
use pane::{terminal_key, TerminalPane};
use probe::Probe;
pub use session_manager::Backend;
use session_manager::SessionEvent;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use system_panel::SystemPanel;
use terminal_render::{PaneRender, Renderer};
use ui::{Action, UiState};
#[cfg(target_os = "macos")]
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
#[cfg(target_os = "linux")]
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition},
    event::{ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState},
    window::{Window, WindowId},
};
use workspace::{Axis, Workspace};

#[derive(Clone, Debug)]
pub enum Wake {
    #[cfg(target_os = "macos")]
    MacMenu(macos_menu::MenuCommand),
    #[cfg(target_os = "macos")]
    MacQuitKeyDown(macos_menu::QuitChord, Instant, u64),
    #[cfg(target_os = "macos")]
    MacQuitKeyReleased(macos_menu::QuitChord, Option<Instant>, u64),
    #[cfg(target_os = "macos")]
    MacQuitDecision(bool),
    #[cfg(target_os = "android")]
    AndroidInput(android_input::AndroidInput),
    Session,
    Commit(String),
    Preedit(String),
    Key {
        name: String,
        modifiers: u8,
    },
    System,
    Repaint(Duration),
}

pub struct Options {
    pub backend: Backend,
    pub shell: Option<String>,
    pub shell_integration: Option<session_manager::ShellIntegration>,
    pub probe_dir: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            shell: None,
            shell_integration: None,
            probe_dir: None,
            config_path: None,
        }
    }
}

pub fn run(event_loop: EventLoop<Wake>, options: Options) -> Result<()> {
    if let Some(path) = &options.config_path {
        config::select_path(path)?;
    }
    #[cfg(target_os = "linux")]
    let mut event_loop = event_loop;
    #[cfg(unix)]
    anyhow::ensure!(
        system_api::current_uid() != 0,
        "Plyra Terminal UI requires an ordinary user account"
    );
    #[cfg(windows)]
    anyhow::ensure!(
        !system_api::windows::current_principal()?.elevated,
        "Plyra Terminal UI requires an ordinary user token"
    );
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(options, event_loop.create_proxy());
    // The Wayland clipboard borrows the event loop's display. Keep that display
    // alive until the application and its clipboard worker have been dropped.
    #[cfg(target_os = "linux")]
    event_loop.run_app_on_demand(&mut app)?;
    #[cfg(not(target_os = "linux"))]
    event_loop.run_app(&mut app)?;
    if let Some(error) = app.error {
        anyhow::bail!(error);
    }
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn session_redraw(visible_changed: bool, timeline_changed: bool, timeline_visible: bool) -> bool {
    visible_changed || (timeline_changed && timeline_visible)
}

fn startup_font_family<'a>(
    configured: &'a str,
    available: &'a [String],
    fallback: &'a str,
) -> &'a str {
    if configured == "JetBrainsMono Nerd Font Mono" {
        available
            .iter()
            .find(|family| family.as_str() == configured)
            .or_else(|| {
                available
                    .iter()
                    .find(|family| family.as_str() == "JetBrainsMono NFM")
            })
            .map_or(fallback, String::as_str)
    } else {
        configured
    }
}

#[cfg(any(target_os = "linux", windows, test))]
fn window_resize_direction(
    pointer: PhysicalPosition<f64>,
    size: winit::dpi::PhysicalSize<u32>,
    scale: f64,
) -> Option<winit::window::ResizeDirection> {
    use winit::window::ResizeDirection;
    if !pointer.x.is_finite()
        || !pointer.y.is_finite()
        || !scale.is_finite()
        || scale <= 0.0
        || pointer.x < 0.0
        || pointer.y < 0.0
        || pointer.x >= f64::from(size.width)
        || pointer.y >= f64::from(size.height)
    {
        return None;
    }
    let edge = 5.0 * scale;
    let horizontal = if pointer.x < edge {
        -1
    } else if pointer.x >= f64::from(size.width) - edge {
        1
    } else {
        0
    };
    let vertical = if pointer.y < edge {
        -1
    } else if pointer.y >= f64::from(size.height) - edge {
        1
    } else {
        0
    };
    match (horizontal, vertical) {
        (-1, -1) => Some(ResizeDirection::NorthWest),
        (0, -1) => Some(ResizeDirection::North),
        (1, -1) => Some(ResizeDirection::NorthEast),
        (-1, 0) => Some(ResizeDirection::West),
        (1, 0) => Some(ResizeDirection::East),
        (-1, 1) => Some(ResizeDirection::SouthWest),
        (0, 1) => Some(ResizeDirection::South),
        (1, 1) => Some(ResizeDirection::SouthEast),
        _ => None,
    }
}

#[cfg(any(target_os = "android", test))]
fn restore_egui_font_texture(context: &egui::Context) {
    if context.cumulative_pass_nr() == 0 {
        return;
    }
    let textures = context.tex_manager();
    let id = egui::TextureId::default();
    let options = {
        let manager = textures.read();
        manager.meta(id).map(|meta| meta.options)
    };
    if let Some(options) = options {
        let image = context.fonts(|fonts| fonts.image());
        textures
            .write()
            .set(id, egui::epaint::ImageDelta::full(image, options));
    }
}

#[cfg(test)]
mod android_resume_tests {
    #[test]
    fn android_renderer_resume_requeues_consumed_font_atlas_without_resetting_context() {
        let context = egui::Context::default();
        super::restore_egui_font_texture(&context);
        let frame = || {
            context.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Resume atlas");
                });
            })
        };
        let initial = frame();
        assert!(initial
            .textures_delta
            .set
            .iter()
            .any(|(id, image)| { *id == egui::TextureId::default() && image.pos.is_none() }));
        let steady = frame();
        assert!(steady.textures_delta.set.is_empty());
        let retained_focus = egui::Id::new("android-resume-retained-focus");
        context.memory_mut(|memory| memory.request_focus(retained_focus));
        let textures = context.tex_manager();
        let options = textures
            .read()
            .meta(egui::TextureId::default())
            .unwrap()
            .options;
        let image_size = context.fonts(|fonts| fonts.image().size);
        super::restore_egui_font_texture(&context);
        let delta = textures.write().take_delta();
        let (_, full) = delta
            .set
            .iter()
            .find(|(id, _)| *id == egui::TextureId::default())
            .unwrap();
        assert!(full.pos.is_none());
        assert_eq!(full.image.size(), image_size);
        assert_eq!(full.options, options);
        assert_eq!(
            context.memory(|memory| memory.focused()),
            Some(retained_focus)
        );
    }
}

fn shortcut_binding(
    settings: &Settings,
    key: &str,
    modifiers: u8,
    modifierless: Option<&Key>,
) -> Option<Shortcut> {
    if let Some(action) = settings.shortcut(&Chord {
        key: key.into(),
        modifiers,
    }) {
        return Some(action);
    }
    if modifiers & 1 == 0 {
        return None;
    }
    let modifierless = modifierless?;
    let alias = match key {
        "，" => Some(","),
        "［" | "【" | "「" => Some("["),
        "］" | "】" | "」" => Some("]"),
        _ => None,
    };
    if let Some(key) = alias {
        return settings.shortcut(&Chord {
            key: key.into(),
            modifiers,
        });
    }
    let Key::Character(base) = modifierless else {
        return None;
    };
    if !matches!(base.as_str(), "," | "[" | "]") {
        return None;
    }
    settings.shortcut(&Chord {
        key: base.to_string(),
        modifiers,
    })
}

#[derive(Default)]
struct LayoutPersistence {
    deferred_from: Option<(Vec<workspace::SplitTree>, usize)>,
}

impl LayoutPersistence {
    fn configuration_applied(
        &mut self,
        previous: &Settings,
        next: &Settings,
        workspace: &Workspace,
    ) {
        if previous.layout != next.layout || previous.active_tab != next.active_tab {
            let current = (workspace.snapshot(), workspace.active_tab);
            self.deferred_from =
                (next.layout != current.0 || next.active_tab != current.1).then_some(current);
        }
    }

    fn changed(&self, settings: &Settings, workspace: &Workspace) -> bool {
        let (saved, active) = self
            .deferred_from
            .as_ref()
            .map(|(layout, active)| (layout, *active))
            .unwrap_or((&settings.layout, settings.active_tab));
        workspace.active_tab != active || workspace.snapshot().as_slice() != saved.as_slice()
    }

    fn sync_for_save(&mut self, settings: &mut Settings, workspace: &Workspace) {
        if self.deferred_from.is_some() && !self.changed(settings, workspace) {
            return;
        }
        self.deferred_from = None;
        settings.layout = workspace.snapshot();
        settings.active_tab = workspace.active_tab;
    }
}

#[derive(Default)]
struct RedrawState {
    occluded: bool,
    zero_sized: bool,
    repaint_at: Option<Instant>,
    surface_retry_at: Option<Instant>,
    surface_retries: usize,
}

impl RedrawState {
    fn can_draw(&self) -> bool {
        !(self.occluded || self.zero_sized)
    }

    fn repaint_after(&mut self, delay: Duration) -> bool {
        if !self.can_draw() {
            self.clear_repaints();
            return false;
        }
        if delay.is_zero() {
            return true;
        }
        if let Some(deadline) = Instant::now().checked_add(delay) {
            self.repaint_at = Some(self.repaint_at.map_or(deadline, |old| old.min(deadline)));
        }
        false
    }

    fn clear_repaints(&mut self) {
        self.repaint_at = None;
        self.surface_presented();
    }

    fn surface_presented(&mut self) {
        self.surface_retry_at = None;
        self.surface_retries = 0;
    }

    fn retry_surface(&mut self, now: Instant) -> Option<Duration> {
        if !self.can_draw() {
            self.clear_repaints();
            return None;
        }
        let delay = Duration::from_millis(*[16, 64, 256].get(self.surface_retries)?);
        self.surface_retries += 1;
        let deadline = now.checked_add(delay)?;
        self.surface_retry_at = Some(
            self.surface_retry_at
                .map_or(deadline, |old| old.min(deadline)),
        );
        Some(delay)
    }

    fn next_repaint(&self) -> Option<Instant> {
        [self.repaint_at, self.surface_retry_at]
            .into_iter()
            .flatten()
            .min()
    }

    fn take_due_repaint(&mut self, now: Instant) -> bool {
        let mut due = false;
        for deadline in [&mut self.repaint_at, &mut self.surface_retry_at] {
            if deadline.is_some_and(|deadline| deadline <= now) {
                *deadline = None;
                due = true;
            }
        }
        due
    }
}

fn near_pane_edge(position: PhysicalPosition<f64>, rect: [f32; 4], tolerance: f64) -> bool {
    let [left, top, width, height] = rect.map(f64::from);
    let right = left + width;
    let bottom = top + height;
    position.x >= left - tolerance
        && position.x <= right + tolerance
        && position.y >= top - tolerance
        && position.y <= bottom + tolerance
        && ((position.x - left).abs() <= tolerance
            || (position.x - right).abs() <= tolerance
            || (position.y - top).abs() <= tolerance
            || (position.y - bottom).abs() <= tolerance)
}

#[cfg(test)]
mod redraw_tests {
    #[test]
    fn hidden_session_updates_sleep_until_visible_content_changes() {
        assert!(!super::session_redraw(false, false, false));
        assert!(!super::session_redraw(false, true, false));
        assert!(super::session_redraw(false, true, true));
        assert!(super::session_redraw(true, false, false));
    }

    #[test]
    fn occluded_windows_drop_deadlines_and_restore_latest_draw() {
        let mut state = super::RedrawState::default();
        assert!(state.can_draw());
        assert!(!state.repaint_after(std::time::Duration::from_millis(10)));
        assert!(state.repaint_at.is_some());
        state.occluded = true;
        assert!(!state.repaint_after(std::time::Duration::ZERO));
        assert!(state.repaint_at.is_none());
        assert!(!state.can_draw());
        state.occluded = false;
        assert!(state.can_draw());
        assert!(state.repaint_after(std::time::Duration::ZERO));
        state.zero_sized = true;
        assert!(!state.can_draw());
        assert!(!state.repaint_after(std::time::Duration::ZERO));
        state.zero_sized = false;
        assert!(state.can_draw());
    }

    #[test]
    fn surface_acquire_retries_are_bounded_and_stop_without_a_present() {
        let mut state = super::RedrawState::default();
        let mut now = std::time::Instant::now();
        for milliseconds in [16, 64, 256] {
            let delay = std::time::Duration::from_millis(milliseconds);
            assert_eq!(state.retry_surface(now), Some(delay));
            let deadline = now + delay;
            assert_eq!(state.next_repaint(), Some(deadline));
            assert!(!state.take_due_repaint(now));
            assert!(state.take_due_repaint(deadline));
            now = deadline;
        }
        for _ in 0..3 {
            assert_eq!(state.retry_surface(now), None);
            assert_eq!(state.next_repaint(), None);
            assert!(!state.take_due_repaint(now));
        }
    }

    #[test]
    fn successful_present_cancels_surface_retry_and_preserves_ui_deadline() {
        let mut state = super::RedrawState::default();
        state.repaint_after(std::time::Duration::from_secs(10));
        let ui_deadline = state.repaint_at;
        let now = std::time::Instant::now();
        assert!(state.retry_surface(now).is_some());
        state.surface_presented();
        assert_eq!(state.surface_retry_at, None);
        assert_eq!(state.surface_retries, 0);
        assert_eq!(state.next_repaint(), ui_deadline);
        assert_eq!(
            state.retry_surface(now),
            Some(std::time::Duration::from_millis(16))
        );
    }

    #[test]
    fn hidden_or_zero_sized_surfaces_drop_pending_retries() {
        let mut state = super::RedrawState::default();
        let now = std::time::Instant::now();
        assert!(state.retry_surface(now).is_some());
        state.occluded = true;
        assert_eq!(state.retry_surface(now), None);
        assert_eq!(state.next_repaint(), None);
        state.occluded = false;
        assert_eq!(
            state.retry_surface(now),
            Some(std::time::Duration::from_millis(16))
        );
        state.zero_sized = true;
        assert!(!state.repaint_after(std::time::Duration::ZERO));
        assert_eq!(state.next_repaint(), None);
        assert_eq!(state.surface_retries, 0);
    }

    #[test]
    fn cross_pane_release_keeps_the_press_owner_and_clears_capture() {
        let mut capture = super::MouseCapture::default();
        let pressed = super::CapturedButton {
            pane: 11,
            button: 1,
            native: true,
        };
        assert!(capture.press(pressed).is_none());
        // The pointer may now hit another pane or an overlay. Release uses only
        // the stored owner and consumes it before dispatching to that session.
        assert_eq!(capture.get(1), Some(pressed));
        assert_eq!(capture.release(1), Some(pressed));
        assert!(capture.get(1).is_none());
        assert!(capture.release(1).is_none());
    }

    #[test]
    fn mouse_capture_keeps_shift_routing_until_release() {
        let mut capture = super::MouseCapture::default();
        let native = super::CapturedButton {
            pane: 11,
            button: 1,
            native: true,
        };
        let local = super::CapturedButton {
            pane: 22,
            button: 1,
            native: false,
        };
        assert!(capture.press(native).is_none());
        // A subsequent Shift change does not reinterpret an existing press.
        assert!(capture.release(1).unwrap().native);
        assert!(capture.press(local).is_none());
        assert!(!capture.release(1).unwrap().native);
    }

    #[test]
    fn mouse_capture_cancel_releases_all_buttons() {
        let mut capture = super::MouseCapture::default();
        let left = super::CapturedButton {
            pane: 11,
            button: 1,
            native: false,
        };
        let right = super::CapturedButton {
            pane: 11,
            button: 3,
            native: true,
        };
        assert!(capture.press(left).is_none());
        assert!(capture.press(right).is_none());
        assert_eq!(capture.cancel(), [Some(left), None, Some(right)]);
        assert!(capture.buttons.iter().all(Option::is_none));
        assert!(capture.cancel().iter().all(Option::is_none));
    }

    #[test]
    fn pointer_edges_include_splitter_gaps_and_exclude_static_canvas() {
        let rect = [100.0, 100.0, 400.0, 300.0];
        let position = winit::dpi::PhysicalPosition::new;
        assert!(super::near_pane_edge(position(502.0, 200.0), rect, 8.0));
        assert!(super::near_pane_edge(position(495.0, 200.0), rect, 8.0));
        assert!(!super::near_pane_edge(position(300.0, 250.0), rect, 8.0));
        assert!(!super::near_pane_edge(position(800.0, 250.0), rect, 8.0));
    }
}

#[cfg(target_os = "macos")]
struct QuitHold {
    deadline: Instant,
    release_deadline: Instant,
    chord: macos_menu::QuitChord,
}

#[cfg(target_os = "macos")]
impl QuitHold {
    fn completed_by_release(
        &self,
        chord: macos_menu::QuitChord,
        released_at: Option<Instant>,
        focused: bool,
    ) -> bool {
        focused
            && self.chord == chord
            && released_at.is_some_and(|released_at| released_at >= self.release_deadline)
    }
}

#[cfg(target_os = "macos")]
struct QuitFocus {
    terminal_focused: bool,
    egui_focus: Option<egui::Id>,
}

#[cfg(not(target_os = "android"))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ImeCursorArea {
    window: WindowId,
    pane: u64,
    position: PhysicalPosition<i32>,
    size: winit::dpi::PhysicalSize<u32>,
    scale_factor: f64,
}

fn unchanged_terminal_preedit(
    event: &WindowEvent,
    terminal_input: bool,
    text: &str,
    cursor: Option<(usize, usize)>,
) -> bool {
    terminal_input
        && matches!(event, WindowEvent::Ime(Ime::Preedit(next_text, next_cursor))
            if next_text == text && *next_cursor == cursor)
}

#[cfg(test)]
mod ime_feedback_tests {
    use super::unchanged_terminal_preedit;
    use winit::event::{Ime, WindowEvent};

    #[test]
    fn identical_terminal_preedit_feedback_does_not_reach_egui() {
        let empty = WindowEvent::Ime(Ime::Preedit(String::new(), None));
        let forwarded = (0..512)
            .filter(|_| !unchanged_terminal_preedit(&empty, true, "", None))
            .count();
        assert_eq!(
            forwarded, 0,
            "idle compositor Done feedback must not request another frame"
        );
        assert!(!unchanged_terminal_preedit(
            &empty,
            true,
            "中文",
            Some((3, 6))
        ));
        assert!(!unchanged_terminal_preedit(&empty, false, "", None));
        let composition = WindowEvent::Ime(Ime::Preedit("中文".into(), Some((3, 6))));
        assert!(!unchanged_terminal_preedit(&composition, true, "", None));
        assert!(!unchanged_terminal_preedit(
            &composition,
            true,
            "中文",
            Some((0, 3))
        ));
        assert!(unchanged_terminal_preedit(
            &composition,
            true,
            "中文",
            Some((3, 6))
        ));
        assert!(!unchanged_terminal_preedit(
            &WindowEvent::Ime(Ime::Commit("中文".into())),
            true,
            "中文",
            Some((3, 6)),
        ));
    }
}

struct App {
    options: Options,
    proxy: EventLoopProxy<Wake>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    workspace: Workspace,
    panes: BTreeMap<u64, TerminalPane>,
    gui: egui::Context,
    gui_state: Option<egui_winit::State>,
    ui: UiState,
    terminal_focused: bool,
    window_focused: bool,
    discard_ime_commit: bool,
    #[cfg(not(target_os = "android"))]
    ime_cursor_area: Option<ImeCursorArea>,
    pointer: PhysicalPosition<f64>,
    modifiers: ModifiersState,
    error: Option<String>,
    probe: Option<Probe>,
    frames: u64,
    first_present_ms: Option<u128>,
    started: Instant,
    redraw_state: RedrawState,
    layout_persistence: LayoutPersistence,
    object: Option<OpenObject>,
    command_started: BTreeMap<u64, u64>,
    next_shell_operation: u64,
    metrics: diagnostics::Diagnostics,
    command_press: Option<CommandPress>,
    mouse_capture: MouseCapture,
    #[cfg(target_os = "android")]
    android_selection: Option<(u64, u64, terminal_render::Selection)>,
    #[cfg(target_os = "android")]
    android_viewport: Option<[u32; 4]>,
    ui_fonts_ready: bool,
    font_families: Arc<[String]>,
    #[cfg(target_os = "macos")]
    clipboard: Option<arboard::Clipboard>,
    #[cfg(target_os = "macos")]
    macos_menu: Option<macos_menu::NativeMenu>,
    #[cfg(target_os = "macos")]
    hover_buttons: Option<macos_window::HoverButtons>,
    #[cfg(target_os = "macos")]
    quit_hold: Option<QuitHold>,
    #[cfg(target_os = "macos")]
    quit_double_press: macos_menu::QuitDoublePress,
    #[cfg(target_os = "macos")]
    quit_focus: Option<QuitFocus>,
}

impl App {
    fn new(options: Options, proxy: EventLoopProxy<Wake>) -> Self {
        let probe = options
            .probe_dir
            .as_ref()
            .map(|dir| Probe::new(dir.clone()));
        let (settings, notice) = if probe.is_some() {
            (Settings::default(), None)
        } else {
            Settings::startup()
        };
        #[cfg(target_os = "android")]
        let notice = match system_api::android_root::set_root_options(settings.android_root.clone())
        {
            Ok(()) => notice,
            Err(error) => Some(error.to_string()),
        };
        let gui = egui::Context::default();
        ui::style(&gui);
        ui::apply_style(&gui, &settings);
        let wake = proxy.clone();
        gui.set_request_repaint_callback(move |request| {
            let _ = wake.send_event(Wake::Repaint(request.delay));
        });
        let mut ui = UiState::new(settings, SystemPanel::new(proxy.clone()), proxy.clone());
        ui.notice = notice;
        #[cfg(target_os = "macos")]
        if probe.is_none() {
            ui.system
                .configure_administrator(ui.settings.administrator_persistent, true);
        }
        Self {
            options,
            proxy,
            window: None,
            renderer: None,
            workspace: Workspace::new(),
            panes: BTreeMap::new(),
            gui,
            gui_state: None,
            ui,
            terminal_focused: true,
            window_focused: true,
            discard_ime_commit: false,
            #[cfg(not(target_os = "android"))]
            ime_cursor_area: None,
            pointer: PhysicalPosition::new(0.0, 0.0),
            modifiers: ModifiersState::empty(),
            error: None,
            probe,
            frames: 0,
            first_present_ms: None,
            started: Instant::now(),
            redraw_state: RedrawState::default(),
            layout_persistence: LayoutPersistence::default(),
            object: None,
            command_started: BTreeMap::new(),
            next_shell_operation: 1,
            metrics: diagnostics::Diagnostics::default(),
            command_press: None,
            mouse_capture: MouseCapture::default(),
            #[cfg(target_os = "android")]
            android_selection: None,
            #[cfg(target_os = "android")]
            android_viewport: None,
            ui_fonts_ready: false,
            font_families: Arc::from([]),
            #[cfg(target_os = "macos")]
            clipboard: None,
            #[cfg(target_os = "macos")]
            macos_menu: None,
            #[cfg(target_os = "macos")]
            hover_buttons: None,
            #[cfg(target_os = "macos")]
            quit_hold: None,
            #[cfg(target_os = "macos")]
            quit_double_press: macos_menu::QuitDoublePress::default(),
            #[cfg(target_os = "macos")]
            quit_focus: None,
        }
    }
    fn active(&self) -> &TerminalPane {
        &self.panes[&self.workspace.active_pane]
    }
    fn active_mut(&mut self) -> &mut TerminalPane {
        self.panes
            .get_mut(&self.workspace.active_pane)
            .expect("active terminal pane")
    }
    fn redraw(&self) {
        if !self.redraw_state.can_draw() {
            return;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn pointer_redraw(&self, position: PhysicalPosition<f64>) -> bool {
        if self.ui.overlay_open()
            || self.object.is_some()
            || self.ui.settings.effect == config::Effect::Full
            || self.gui.is_using_pointer()
            || self
                .panes
                .values()
                .any(|pane| pane.selection_anchor.is_some())
        {
            return true;
        }
        let tolerance = self
            .window
            .as_ref()
            .map_or(8.0, |window| 8.0 * window.scale_factor());
        self.workspace
            .active_ids()
            .iter()
            .filter_map(|id| self.panes.get(id))
            .any(|pane| {
                near_pane_edge(position, pane.rect, tolerance)
                    || near_pane_edge(self.pointer, pane.rect, tolerance)
            })
    }
    #[cfg(any(target_os = "linux", windows))]
    fn resize_direction(&self) -> Option<winit::window::ResizeDirection> {
        let window = self.window.as_ref()?;
        if self.probe.is_some()
            || self.ui.overlay_open()
            || self.object.is_some()
            || window.is_maximized()
            || window.fullscreen().is_some()
        {
            return None;
        }
        window_resize_direction(self.pointer, window.inner_size(), window.scale_factor())
    }
    fn terminal_input(&self) -> bool {
        ui::terminal_route(
            self.terminal_focused,
            self.window_focused,
            self.object.is_some() || self.ui.blocks_terminal(),
        )
    }

    #[cfg(windows)]
    fn windows_drag_strip_hit(&self) -> bool {
        let Some(window) = &self.window else {
            return false;
        };
        if self.probe.is_some()
            || self.ui.overlay_open()
            || self.object.is_some()
            || window.is_maximized()
            || window.fullscreen().is_some()
        {
            return false;
        }
        let scale = f64::from(egui_winit::pixels_per_point(&self.gui, window));
        let size = window.inner_size();
        ui::windows_drag_strip_hit(
            egui::pos2(
                (self.pointer.x / scale) as f32,
                (self.pointer.y / scale) as f32,
            ),
            egui::vec2(
                (f64::from(size.width) / scale) as f32,
                (f64::from(size.height) / scale) as f32,
            ),
        )
    }
    fn cancel_composition(&mut self) {
        self.invalidate_ime_anchor();
        let had_composition = self.panes.values().any(|pane| !pane.preedit.is_empty());
        for pane in self.panes.values_mut() {
            pane.clear_preedit();
        }
        if !had_composition {
            return;
        }
        self.discard_ime_commit = true;
        #[cfg(not(target_os = "android"))]
        if let Some(window) = &self.window {
            window.set_ime_allowed(false);
            if let Some(state) = &mut self.gui_state {
                state.set_allow_ime(false);
            }
            if self.window_focused {
                window.set_ime_allowed(true);
                if let Some(state) = &mut self.gui_state {
                    state.set_allow_ime(true);
                }
            }
        }
    }
    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        self.invalidate_ime_anchor();
        self.redraw_state = RedrawState::default();
        let probe = self.probe.is_some();
        if probe {
            self.ui.settings.effect = config::Effect::Off;
        }
        let attributes = Window::default_attributes()
            .with_title("Plyra Terminal")
            .with_decorations(false)
            .with_theme(Some(winit::window::Theme::Dark))
            .with_transparent(!probe || cfg!(target_os = "linux"))
            .with_inner_size(if probe {
                LogicalSize::new(1040.0, 680.0)
            } else {
                LogicalSize::new(1280.0, 820.0)
            });
        #[cfg(any(windows, target_os = "linux"))]
        let icon = winit::window::Icon::from_rgba(
            include_bytes!("../../../assets/app-icon/window-64.rgba").to_vec(),
            64,
            64,
        )
        .context("loading the terminal window icon")?;
        #[cfg(windows)]
        let attributes = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes
                .with_window_icon(Some(icon.clone()))
                .with_taskbar_icon(Some(icon))
                .with_no_redirection_bitmap(true)
        };
        #[cfg(target_os = "linux")]
        let attributes = {
            let server_decorations =
                winit::raw_window_handle::HasDisplayHandle::display_handle(event_loop)
                    .ok()
                    .is_some_and(terminal_render::prefer_server_decorations);
            winit::platform::wayland::WindowAttributesExtWayland::with_name(
                attributes
                    .with_window_icon(Some(icon))
                    .with_decorations(server_decorations),
                "org.plyra.glass-terminal",
                "glass-terminal",
            )
        };
        #[cfg(target_os = "macos")]
        let attributes = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            if probe {
                attributes
            } else {
                attributes
                    .with_decorations(false)
                    .with_title_hidden(true)
                    .with_titlebar_transparent(true)
                    .with_fullsize_content_view(true)
                    .with_titlebar_buttons_hidden(true)
            }
        };
        #[cfg(windows)]
        let fallback_attributes = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes
                .clone()
                .with_no_redirection_bitmap(false)
                .with_transparent(false)
        };
        let window = Arc::new(event_loop.create_window(attributes)?);
        #[cfg(not(target_os = "android"))]
        window.set_ime_allowed(true);
        let renderer = Renderer::new_with_adapter_preference(
            window.clone(),
            probe,
            self.ui.settings.gpu_power_preference.renderer_preference(),
        );
        #[cfg(windows)]
        let (window, mut renderer) = match renderer {
            Ok(renderer) => (window, renderer),
            Err(error) => {
                let first_error = format!("{error:#}");
                drop(error);
                let trace = std::env::var("GLASS_TRACE_STARTUP").as_deref() == Ok("1");
                if trace {
                    eprintln!(
                        "[glass-startup] app stage=opaque_window_retry.begin cause={first_error}"
                    );
                }
                // A NOREDIRECTIONBITMAP HWND may reject a runtime style change.
                // No panes exist yet: release it before one bounded retry with
                // an ordinary opaque HWND, without starting another Shell.
                window.set_visible(false);
                drop(window);
                let window = Arc::new(
                    event_loop
                        .create_window(fallback_attributes)
                        .with_context(|| format!("creating opaque HWND after: {first_error}"))?,
                );
                window.set_ime_allowed(true);
                let renderer = Renderer::new_with_adapter_preference(
                    window.clone(),
                    probe,
                    self.ui.settings.gpu_power_preference.renderer_preference(),
                )
                .with_context(|| format!("opaque HWND retry failed after: {first_error}"))?;
                if trace {
                    eprintln!("[glass-startup] app stage=opaque_window_retry.end");
                }
                (window, renderer)
            }
        };
        #[cfg(not(windows))]
        let mut renderer = renderer?;
        renderer.set_appearance(self.ui.settings.appearance());
        renderer.set_font_size(self.ui.settings.font_size);
        let families = renderer.font_families();
        self.ui.settings.font_family = startup_font_family(
            &self.ui.settings.font_family,
            &families,
            renderer.font_family(),
        )
        .to_owned();
        if let Err(error) = renderer.set_font_family(&self.ui.settings.font_family) {
            self.ui.notice = Some(error.to_string());
            self.ui.settings.font_family = renderer.font_family().to_owned();
        }
        self.font_families = families.into();
        let mut state = egui_winit::State::new(
            self.gui.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        #[cfg(not(target_os = "android"))]
        state.set_allow_ime(true);
        #[cfg(target_os = "android")]
        state.set_allow_ime(false);
        #[cfg(target_os = "android")]
        restore_egui_font_texture(&self.gui);
        self.gui_state = Some(state);
        self.renderer = Some(renderer);
        self.window = Some(window);
        if self.panes.is_empty() {
            if !probe && !self.ui.settings.layout.is_empty() {
                self.workspace = Workspace::restore(
                    self.ui.settings.layout.clone(),
                    self.ui.settings.active_tab,
                );
                for index in 0..self.workspace.tabs.len() {
                    for id in self.workspace.tab_ids(index) {
                        self.panes.insert(
                            id,
                            TerminalPane::spawn(
                                id,
                                None,
                                &self.options,
                                &self.ui.settings,
                                self.proxy.clone(),
                            )?,
                        );
                    }
                }
                self.ui
                    .system
                    .record("workspace", "Restored layout with new local Shell sessions");
            } else {
                self.panes.insert(
                    1,
                    TerminalPane::spawn(
                        1,
                        None,
                        &self.options,
                        &self.ui.settings,
                        self.proxy.clone(),
                    )?,
                );
                self.ui
                    .system
                    .record("workspace", "Local Shell session created");
            }
        }
        if probe {
            self.resize()?;
        } else if self.ui.settings.system_open {
            self.ui.system.refresh();
        }
        #[cfg(target_os = "macos")]
        if !probe && self.macos_menu.is_none() {
            self.macos_menu = Some(macos_menu::NativeMenu::install(
                self.proxy.clone(),
                &self.ui.settings,
            )?);
        }
        #[cfg(target_os = "macos")]
        if !probe {
            self.hover_buttons = Some(macos_window::HoverButtons::install(
                self.window.as_ref().unwrap(),
                self.ui.settings.appearance_ui.macos_hover_buttons,
                self.proxy.clone(),
            )?);
        }
        self.redraw();
        Ok(())
    }
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: impl std::fmt::Display) {
        eprintln!("Plyra Terminal: {error}");
        self.error = Some(error.to_string());
        self.close_session();
        event_loop.exit();
    }
    fn close_session(&mut self) {
        self.cancel_mouse_capture();
        for pane in self.panes.values_mut() {
            pane.close();
        }
        self.ui.mcp.shutdown();
    }
    fn focus(&mut self, id: u64) {
        if !self.panes.contains_key(&id) {
            return;
        }
        self.invalidate_ime_anchor();
        if id != self.workspace.active_pane {
            self.cancel_mouse_capture();
            self.cancel_composition();
        }
        if let Some(old) = self.panes.get_mut(&self.workspace.active_pane) {
            if let Some(session) = &mut old.session {
                let _ = session.focus(false);
            }
        }
        self.workspace.active_pane = id;
        self.terminal_focused = true;
        self.gui.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
        if let Some(pane) = self.panes.get_mut(&id) {
            if let Some(session) = &mut pane.session {
                let _ = session.focus(true);
            }
        }
        self.ime_anchor();
        self.redraw();
    }
    fn input_result(&mut self, result: Result<()>) {
        let status = result.err().map(|error| {
            eprintln!("Plyra Terminal input: {error:#}");
            error
                .to_string()
                .chars()
                .filter(|c| !c.is_control())
                .take(240)
                .collect::<String>()
        });
        if let Some(message) = &status {
            self.ui.notice = Some(message.clone());
        }
        if let Some(pane) = self.panes.get_mut(&self.workspace.active_pane) {
            pane.status = status;
        }
        self.redraw();
    }
    fn commit(&mut self, text: &str) -> Result<()> {
        let id = self.workspace.active_pane;
        let revision = self
            .active()
            .session
            .as_ref()
            .map(|session| session.output_revision());
        let result = self.active_mut().commit(text);
        if result.is_ok() && !text.is_empty() {
            if let Some(revision) = revision {
                self.metrics.input(id, revision);
            }
        }
        self.input_result(result);
        Ok(())
    }
    fn key(&mut self, name: &str, modifiers: u8) -> Result<()> {
        let id = self.workspace.active_pane;
        let revision = self
            .active()
            .session
            .as_ref()
            .map(|session| session.output_revision());
        let result = self.active_mut().key(name, modifiers);
        if result.is_ok() {
            if let Some(revision) = revision {
                self.metrics.input(id, revision);
            }
        }
        self.input_result(result);
        Ok(())
    }
    fn receive(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let visible = self.workspace.active_ids();
        let can_draw = self.redraw_state.can_draw();
        let mut changed = false;
        let mut timeline_changed = false;
        let mut closed = Vec::new();
        for (id, pane) in &mut self.panes {
            let mut session_identity = pane.shell_session_id.clone();
            let (events, screen_changed) = pane.receive(can_draw && visible.contains(id));
            changed |= screen_changed && visible.contains(id);
            for event in events {
                timeline_changed = true;
                match event {
                    SessionEvent::Shell { at_ms, event } => {
                        session_identity = event.data["session_id"].as_str().map(str::to_owned);
                        if event.event_type == "session.exiting" {
                            if let Some(identity) = &session_identity {
                                self.ui.system.cancel_shell_operations(*id, identity);
                            }
                        }
                        if event.event_type == "host.operation.request" {
                            self.ui.system.invoke_shell_operation(
                                *id,
                                event.data["session_id"].as_str().unwrap().to_owned(),
                                event.data["request_id"].as_u64().unwrap(),
                                event.data["operation"].as_str().unwrap().to_owned(),
                                event.data["args"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .map(|arg| arg.as_str().unwrap().to_owned())
                                    .collect(),
                            );
                            continue;
                        }
                        if matches!(
                            event.event_type.as_str(),
                            "completion.snapshot" | "values.record"
                        ) {
                            changed |= visible.contains(id);
                            continue;
                        }
                        self.ui.system.record_at(
                            at_ms,
                            "Aether Shell",
                            format!("Pane {id} · {}", event.summary()),
                        );
                    }
                    SessionEvent::ShellWarning(warning) => {
                        if let Some(identity) = &session_identity {
                            self.ui.system.cancel_shell_operations(*id, identity);
                        }
                        self.ui
                            .system
                            .record("Aether Shell protocol", format!("Pane {id} · {warning}"));
                    }
                    SessionEvent::Ready {
                        helper_pid,
                        shell_pid,
                        shell,
                    } => {
                        changed |= visible.contains(id);
                        eprintln!("Session ready pane={id} helper={helper_pid:?} shell={shell_pid:?} executable={shell}");
                        self.ui.system.record(
                            "PTY lifecycle",
                            format!(
                                "Pane {id} started · PID {}",
                                shell_pid.map_or("unknown".into(), |pid| pid.to_string())
                            ),
                        );
                        if let Some(probe) = &mut self.probe {
                            probe.ready = true;
                            probe.helper_pid = helper_pid;
                            probe.shell_pid = shell_pid;
                        }
                    }
                    SessionEvent::Exit(code) => {
                        if let Some(identity) = &session_identity {
                            self.ui.system.cancel_shell_operations(*id, identity);
                        }
                        changed |= visible.contains(id);
                        self.ui
                            .system
                            .record("PTY lifecycle", format!("Pane {id} exited · code {code}"));
                        if let Some(probe) = &mut self.probe {
                            probe.exit_code = Some(code);
                        } else {
                            closed.push(*id);
                        }
                    }
                    SessionEvent::Error(error) => {
                        if let Some(identity) = &session_identity {
                            self.ui.system.cancel_shell_operations(*id, identity);
                        }
                        changed |= visible.contains(id);
                        if visible.contains(id) {
                            self.ui.notice = Some(error.clone());
                        }
                        pane.status = Some(error.clone());
                        self.ui.system.record("PTY error", error);
                        pane.close();
                    }
                    SessionEvent::Command {
                        at_ms,
                        phase,
                        detail,
                    } => {
                        if phase == "start" {
                            self.command_started.insert(*id, at_ms);
                        }
                        let duration = if phase == "finish" {
                            self.command_started
                                .remove(id)
                                .map(|started| {
                                    format!(" · observed {} ms", at_ms.saturating_sub(started))
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        self.ui.system.record_at(
                            at_ms,
                            "PTY OSC 133",
                            format!("Pane {id} · {phase} · {detail}{duration}"),
                        );
                    }
                }
            }
        }
        if self.mouse_capture.buttons.iter().flatten().any(|capture| {
            self.panes
                .get(&capture.pane)
                .is_none_or(|pane| pane.session.is_none())
        }) {
            self.cancel_mouse_capture();
        }
        for (id, pane) in &self.panes {
            self.metrics.output(*id, pane.output_revision);
        }
        for id in closed {
            self.close_pane(id, event_loop);
        }
        if session_redraw(
            changed,
            timeline_changed,
            self.ui.settings.system_open && self.ui.system.page == system_panel::Page::Timeline,
        ) {
            self.ime_anchor();
            self.redraw();
        }
        Ok(())
    }
    fn close_pane(&mut self, id: u64, event_loop: &ActiveEventLoop) {
        if let Some(identity) = self
            .panes
            .get(&id)
            .and_then(|pane| pane.shell_session_id.as_deref())
        {
            self.ui.system.cancel_shell_operations(id, identity);
        }
        if self
            .mouse_capture
            .buttons
            .iter()
            .flatten()
            .any(|capture| capture.pane == id)
        {
            self.cancel_mouse_capture();
        }
        self.metrics.close(id);
        let focused = id == self.workspace.active_pane;
        let visible = self.workspace.active_ids().contains(&id);
        if focused {
            self.cancel_composition();
        }
        self.panes.remove(&id);
        self.workspace.close_pane(id);
        self.ui
            .system
            .record("workspace", format!("Pane {id} closed"));
        if self.workspace.tabs.is_empty() {
            event_loop.exit();
        } else if focused {
            self.focus(self.workspace.active_pane);
        } else if visible {
            self.redraw();
        }
    }
    fn new_shell(&mut self, split: Option<Axis>, command: Option<Vec<String>>) -> Result<()> {
        self.cancel_composition();
        let id = self.workspace.allocate();
        let pane = TerminalPane::spawn(
            id,
            command,
            &self.options,
            &self.ui.settings,
            self.proxy.clone(),
        )?;
        self.panes.insert(id, pane);
        if let Some(axis) = split {
            self.workspace.split(id, axis);
        } else {
            self.workspace.new_tab(id);
        }
        self.ui.system.record(
            "workspace",
            if split.is_some() {
                "Split Shell created"
            } else {
                "Shell tab created"
            },
        );
        self.focus(id);
        Ok(())
    }
    fn action(&mut self, action: Action, event_loop: &ActiveEventLoop) -> Result<()> {
        #[cfg(target_os = "macos")]
        if self.probe.is_none()
            && match &action {
                Action::ClosePane(id) => self.panes.len() == 1 && self.panes.contains_key(id),
                Action::CloseTab(index) => self.workspace.tab_ids(*index).len() == self.panes.len(),
                _ => false,
            }
        {
            return self.request_quit(None, event_loop);
        }
        self.cancel_mouse_capture();
        let persist = matches!(
            &action,
            Action::NewShell
                | Action::NewSsh { .. }
                | Action::Split(_)
                | Action::ActivateTab(_)
                | Action::CycleTab(_)
                | Action::ClosePane(_)
                | Action::CloseTab(_)
                | Action::ApplySettings
        );
        match action {
            Action::NewShell => self.new_shell(None, None)?,
            #[cfg(target_os = "android")]
            Action::ArchLinux => {
                if let Some(command) = android_arch::command()? {
                    self.ui.settings_open = false;
                    self.new_shell(None, Some(command))?;
                } else if android_arch::default_enabled() {
                    android_arch::prepare()?;
                    self.ui.notice = Some("Preparing Arch Linux…".into());
                } else {
                    android_input::pick_arch_archive()?;
                }
            }
            Action::Split(axis) => self.new_shell(Some(axis), None)?,
            Action::NewSsh { target, user, port } => {
                anyhow::ensure!(
                    !target.is_empty()
                        && !target.starts_with('-')
                        && !target.chars().any(char::is_whitespace)
                        && !target.chars().any(char::is_control),
                    "Invalid SSH target"
                );
                anyhow::ensure!(
                    user.len() <= 128 && !user.chars().any(|c| c.is_whitespace() || c.is_control()),
                    "Invalid SSH user"
                );
                let mut command = vec![
                    self.ui.settings.ssh_program.clone(),
                    "-tt".into(),
                    "-p".into(),
                    port.to_string(),
                ];
                if !user.is_empty() {
                    command.extend(["-l".into(), user]);
                }
                command.extend(["--".into(), target.clone()]);
                self.new_shell(None, Some(command))?;
            }
            Action::ActivateTab(index) => {
                self.cancel_composition();
                self.workspace.activate_tab(index);
                self.focus(self.workspace.active_pane);
            }
            Action::FocusCurrent => self.focus(self.workspace.active_pane),
            Action::CycleTab(direction) => {
                self.cancel_composition();
                let index = (self.workspace.active_tab as i32 + direction)
                    .rem_euclid(self.workspace.tabs.len() as i32)
                    as usize;
                self.workspace.activate_tab(index);
                self.focus(self.workspace.active_pane);
            }
            Action::CyclePane(direction) => {
                let ids = self.workspace.active_ids();
                let current = ids
                    .iter()
                    .position(|id| *id == self.workspace.active_pane)
                    .unwrap_or(0);
                let index = (current as i32 + direction).rem_euclid(ids.len() as i32) as usize;
                self.focus(ids[index]);
            }
            Action::ClosePane(id) => self.close_pane(id, event_loop),
            Action::CloseTab(index) => {
                for id in self.workspace.tab_ids(index) {
                    self.close_pane(id, event_loop);
                }
            }
            Action::ApplySettings => {
                self.ui.settings.validate()?;
                #[cfg(target_os = "macos")]
                self.cancel_quit_hold();
                ui::apply_style(&self.gui, &self.ui.settings);
                #[cfg(target_os = "macos")]
                self.ui
                    .system
                    .configure_administrator(self.ui.settings.administrator_persistent, false);
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_appearance(self.ui.settings.appearance());
                    if renderer.font_size() != self.ui.settings.font_size {
                        renderer.set_font_size(self.ui.settings.font_size);
                    }
                    if renderer.font_family() != self.ui.settings.font_family {
                        renderer.set_font_family(&self.ui.settings.font_family)?;
                    }
                }
                self.update_session_palettes(self.ui.settings.palette());
                #[cfg(target_os = "macos")]
                if let Some(menu) = &mut self.macos_menu {
                    menu.rebuild(&self.ui.settings);
                }
                #[cfg(target_os = "macos")]
                if let Some(buttons) = &self.hover_buttons {
                    buttons.set_enabled(self.ui.settings.appearance_ui.macos_hover_buttons);
                }
            }
            Action::ReloadConfig => {
                let settings = Settings::load()?;
                self.apply_config(settings)?;
                self.ui.notice = Some(
                    "Configuration reloaded. Layout changes apply at next launch, Shell changes to new sessions.".into(),
                );
            }
            Action::SaveConfig(text) => {
                let mut candidate = Settings::parse(text.as_bytes())?;
                self.validate_runtime_config(&mut candidate)?;
                let mut settings = self.ui.settings.clone();
                settings.replace_json(&serde_json::to_string(&candidate)?)?;
                self.apply_config(settings)?;
                self.ui.notice = Some(
                    "Configuration saved and applied. Layout changes apply at next launch, Shell changes to new sessions."
                        .into(),
                );
            }
            Action::Search(query, forward) => self.find(&query, forward),
            #[cfg(target_os = "android")]
            Action::Copy => {
                self.ui.close_overlays();
                self.focus(self.workspace.active_pane);
                self.copy();
            }
            #[cfg(target_os = "android")]
            Action::Paste => {
                self.ui.close_overlays();
                self.focus(self.workspace.active_pane);
                self.paste();
            }
            Action::PrepareShellObject {
                pane_id,
                session_id,
                name,
                object,
            } => {
                let result = self
                    .panes
                    .get(&pane_id)
                    .context("Shell pane has closed")
                    .and_then(|pane| {
                        anyhow::ensure!(
                            pane.shell_session_id.as_deref() == Some(session_id.as_str()),
                            "Shell session has changed"
                        );
                        self.ui.system.prepare_catalog_action(&name, &object)
                    });
                if let Err(error) = result {
                    self.ui.notice = Some(error.to_string());
                }
            }
            Action::ShellOperation {
                pane_id,
                session_id,
                operation,
                args,
            } => {
                let request_id = self.next_shell_operation;
                self.next_shell_operation = request_id
                    .checked_add(1)
                    .context("Shell operation request IDs exhausted")?;
                let result = self
                    .panes
                    .get(&pane_id)
                    .context("Shell pane has closed")
                    .and_then(|pane| {
                        anyhow::ensure!(
                            pane.shell_session_id.as_deref() == Some(session_id.as_str()),
                            "Shell session has changed"
                        );
                        pane.session
                            .as_ref()
                            .context("No active Shell session")?
                            .shell_operation(request_id, &operation, &args)
                    });
                match result {
                    Ok(()) => self.ui.system.record(
                        "Aether Shell operation",
                        format!("Pane {pane_id} · requested {operation} · request {request_id}"),
                    ),
                    Err(error) => self.ui.notice = Some(error.to_string()),
                }
            }
            Action::SaveDiagnostics => {
                let window = self.window.as_ref().context("No window")?;
                let renderer = self.renderer.as_ref().context("No renderer")?;
                let report = serde_json::json!({"schema":"glass-terminal.runtime-diagnostics.v1","captured_at_ms":now_ms(),
                    "os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"app_initialization_to_first_surface_present_ms":self.first_present_ms,
                    "presented_frames":self.frames,"window_physical_size":[window.inner_size().width,window.inner_size().height],"scale_factor":window.scale_factor(),
                    "window_physical_position":window.outer_position().ok().map(|position|[position.x,position.y]),
                    "gpu":format!("{:?}",renderer.adapter_info()),"native_glass":renderer.native_glass_enabled(),"native_surface":renderer.diagnostics(),"timings":self.metrics.report()});
                #[cfg(target_os = "macos")]
                let report = {
                    let mut report = report;
                    if let Some(buttons) = &self.hover_buttons {
                        report["native_window_controls"] = buttons.diagnostics();
                    }
                    report
                };
                let path = Settings::path().with_file_name("diagnostics.json");
                config::write_json(&path, &report)?;
                self.ui.notice = Some(format!("Diagnostics saved · {}", path.display()));
            }
        }
        if persist && self.probe.is_none() {
            self.persist_layout()?;
        }
        self.redraw();
        Ok(())
    }
    fn validate_runtime_config(&self, settings: &mut Settings) -> Result<()> {
        settings.validate()?;
        if self.renderer.is_some() {
            let fonts = &self.font_families;
            if settings.font_family == "JetBrainsMono Nerd Font Mono"
                && !fonts.contains(&settings.font_family)
                && fonts.iter().any(|font| font == "JetBrainsMono NFM")
            {
                settings.font_family = "JetBrainsMono NFM".into();
            }
            anyhow::ensure!(
                fonts.contains(&settings.font_family),
                "Monospace font is unavailable"
            );
        }
        Ok(())
    }
    fn update_session_palettes(&mut self, palette: terminal_core::TerminalPalette) {
        for pane in self.panes.values_mut() {
            if let Some(error) = pane.request_palette(palette.clone()) {
                eprintln!("Plyra Terminal: {error}");
                self.ui.notice = Some(error);
            }
        }
        if self
            .panes
            .values()
            .any(|pane| pane.pending_palette.is_some())
        {
            self.ui.notice =
                Some("Configuration applied. Waiting for busy session palette queues.".into());
            self.gui.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn apply_config(&mut self, mut settings: Settings) -> Result<()> {
        self.validate_runtime_config(&mut settings)?;
        if let Some(renderer) = &mut self.renderer {
            renderer.set_font_family(&settings.font_family)?;
            renderer.set_font_size(settings.font_size);
            renderer.set_appearance(settings.appearance());
        }
        #[cfg(target_os = "android")]
        system_api::android_root::set_root_options(settings.android_root.clone())?;
        self.update_session_palettes(settings.palette());
        if self.ui.settings.mcp_servers != settings.mcp_servers {
            self.ui.mcp.reconfigure();
        }
        ui::apply_style(&self.gui, &settings);
        self.layout_persistence.configuration_applied(
            &self.ui.settings,
            &settings,
            &self.workspace,
        );
        self.ui.settings = settings;
        #[cfg(target_os = "macos")]
        {
            self.cancel_quit_hold();
            if let Some(menu) = &mut self.macos_menu {
                menu.rebuild(&self.ui.settings);
            }
            if let Some(buttons) = &self.hover_buttons {
                buttons.set_enabled(self.ui.settings.appearance_ui.macos_hover_buttons);
            }
        }
        #[cfg(target_os = "macos")]
        self.ui
            .system
            .configure_administrator(self.ui.settings.administrator_persistent, false);
        self.redraw();
        Ok(())
    }
    fn persist_layout(&mut self) -> Result<()> {
        self.layout_persistence
            .sync_for_save(&mut self.ui.settings, &self.workspace);
        self.ui.settings.save()
    }
    fn invalidate_ime_anchor(&mut self) {
        #[cfg(not(target_os = "android"))]
        {
            self.ime_cursor_area = None;
        }
    }
    fn ime_anchor(&mut self) {
        #[cfg(not(target_os = "android"))]
        {
            if !self.terminal_input() {
                self.invalidate_ime_anchor();
                return;
            }
            if let (Some(window), Some(renderer), Some(pane)) = (
                &self.window,
                &self.renderer,
                self.panes.get(&self.workspace.active_pane),
            ) {
                let (position, size) = if self.probe.is_some() {
                    renderer.cursor_area(&pane.live_screen)
                } else {
                    renderer.cursor_area_in(&pane.live_screen, pane.rect)
                };
                let area = ImeCursorArea {
                    window: window.id(),
                    pane: pane.id,
                    position,
                    size,
                    scale_factor: window.scale_factor(),
                };
                if self.ime_cursor_area == Some(area) {
                    return;
                }
                window.set_ime_allowed(true);
                window.set_ime_cursor_area(position, size);
                self.ime_cursor_area = Some(area);
            }
        }
    }
    fn draw(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if !self.redraw_state.can_draw() {
            self.redraw_state.clear_repaints();
            return Ok(());
        }
        let render_started = Instant::now();
        for id in self.workspace.active_ids() {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.sync_screen();
                self.metrics.output(id, pane.output_revision);
            }
        }
        for pane in self.panes.values_mut() {
            pane.sync_palette();
        }
        if self
            .panes
            .values()
            .any(|pane| pane.pending_palette.is_some())
        {
            self.gui.request_repaint_after(Duration::from_millis(100));
        }
        let Some(window) = self.window.clone() else {
            return Ok(());
        };
        let frames_before = self.renderer.as_ref().map_or(0, Renderer::frame_count);
        if self.probe.is_some() {
            let screen = self.active().screen.clone();
            let selection = self.active().selection;
            let renderer = self.renderer.as_mut().context("renderer")?;
            renderer.set_selection(selection);
            let size = window.inner_size();
            let pane = PaneRender {
                id: self.workspace.active_pane,
                rect: [0.0, 0.0, size.width as f32, size.height as f32],
                screen: &screen,
                selection,
                focused: true,
                preedit: None,
            };
            renderer.render_workspace(
                &[pane],
                &[],
                &egui::TexturesDelta::default(),
                window.scale_factor() as f32,
                self.ui.settings.effect.renderer(),
            )?;
            self.metrics
                .render(render_started.elapsed().as_secs_f64() * 1000.0);
            self.presented(frames_before);
            return Ok(());
        }
        let input = self
            .gui_state
            .as_mut()
            .context("egui state")?
            .take_egui_input(&window);
        #[cfg(target_os = "android")]
        let input = {
            let mut input = input;
            if let Some([x, y, width, height]) = self.android_viewport {
                let surface = window.inner_size();
                let scale = egui_winit::pixels_per_point(&self.gui, &window);
                let right = x.saturating_add(width).min(surface.width);
                let bottom = y.saturating_add(height).min(surface.height);
                if right > x && bottom > y {
                    input.screen_rect = Some(egui::Rect::from_min_max(
                        egui::pos2(x as f32 / scale, y as f32 / scale),
                        egui::pos2(right as f32 / scale, bottom as f32 / scale),
                    ));
                }
            }
            input
        };
        let released_primary = input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::PointerButton {
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    ..
                }
            )
        });
        if !self.ui_fonts_ready && (self.ui.overlay_open() || self.object.is_some()) {
            if let Some((family, bytes, index)) =
                self.renderer.as_ref().and_then(Renderer::ui_font_fallback)
            {
                ui::install_system_fallback(&self.gui, family, bytes, index);
            }
            self.ui_fonts_ready = true;
        }
        let fonts = Arc::clone(&self.font_families);
        let mut layouts = Vec::new();
        let mut actions = Vec::new();
        let gui = self.gui.clone();
        let prepare_queries = self.ui.query_focus_requested();
        let shell_caret = self.renderer.as_ref().and_then(|renderer| {
            let pane = self.panes.get(&self.workspace.active_pane)?;
            let (position, size) = renderer.cursor_area_in(&pane.live_screen, pane.rect);
            Some([
                position.x as f32,
                position.y as f32,
                size.width as f32,
                size.height as f32,
            ])
        });
        let windows_drag_enabled = cfg!(windows)
            && self.probe.is_none()
            && self.object.is_none()
            && !window.is_maximized()
            && window.fullscreen().is_none();
        let mut output = ui::run_ui(&gui, input, prepare_queries, |ctx| {
            let result = self.ui.draw(
                ctx,
                &mut self.workspace,
                &self.panes,
                &fonts,
                shell_caret,
                windows_drag_enabled,
            );
            layouts = result.0;
            actions.extend(result.1);
            self.object_dialog(ctx);
        });
        let terminal_ime = self.terminal_input();
        if terminal_ime {
            gui.memory_mut(|memory| {
                if let Some(id) = memory.focused() {
                    memory.surrender_focus(id);
                }
            });
            #[cfg(target_os = "android")]
            if let (Some(renderer), Some(pane)) = (
                self.renderer.as_ref(),
                self.panes.get(&self.workspace.active_pane),
            ) {
                let (position, size) = renderer.cursor_area_in(&pane.live_screen, pane.rect);
                let scale = output.pixels_per_point;
                let cursor = egui::Rect::from_min_size(
                    egui::pos2(position.x as f32 / scale, position.y as f32 / scale),
                    egui::vec2(size.width as f32 / scale, size.height as f32 / scale),
                );
                output.platform_output.ime = Some(egui::output::IMEOutput {
                    rect: cursor,
                    cursor_rect: cursor,
                });
            }
        }
        if !self.window_focused {
            output.platform_output.ime = None;
        }
        #[cfg(target_os = "android")]
        {
            android_input::platform_output(&output.platform_output);
            // The served editor is Java's InputView. winit targets DecorView.
            output.platform_output.ime = None;
            self.gui_state.as_mut().unwrap().set_allow_ime(false);
        }
        #[cfg(any(target_os = "linux", windows))]
        if let Some(direction) = self.resize_direction() {
            use winit::window::ResizeDirection;
            output.platform_output.cursor_icon = match direction {
                ResizeDirection::NorthWest => egui::CursorIcon::ResizeNorthWest,
                ResizeDirection::North => egui::CursorIcon::ResizeNorth,
                ResizeDirection::NorthEast => egui::CursorIcon::ResizeNorthEast,
                ResizeDirection::West => egui::CursorIcon::ResizeWest,
                ResizeDirection::East => egui::CursorIcon::ResizeEast,
                ResizeDirection::SouthWest => egui::CursorIcon::ResizeSouthWest,
                ResizeDirection::South => egui::CursorIcon::ResizeSouth,
                ResizeDirection::SouthEast => egui::CursorIcon::ResizeSouthEast,
            };
        }
        #[cfg(not(target_os = "android"))]
        if terminal_ime {
            // ime_anchor owns the terminal candidate position, including native-call deduplication.
            output.platform_output.ime = None;
            self.gui_state.as_mut().unwrap().set_allow_ime(false);
        }
        self.gui_state
            .as_mut()
            .unwrap()
            .handle_platform_output(&window, output.platform_output);
        #[cfg(not(target_os = "android"))]
        if terminal_ime {
            // Keep egui's debounce state synchronized with the enabled terminal IME.
            self.gui_state.as_mut().unwrap().set_allow_ime(true);
        }
        let ppp = output.pixels_per_point;
        let primitives = gui.tessellate(output.shapes, ppp);
        let renderer = self.renderer.as_mut().context("renderer")?;
        for layout in layouts {
            if let Some(pane) = self.panes.get_mut(&layout.id) {
                pane.rect = [
                    layout.rect.left() * ppp,
                    layout.rect.top() * ppp,
                    layout.rect.width() * ppp,
                    layout.rect.height() * ppp,
                ];
                pane.resize(renderer.dimensions_in(pane.rect))?;
            }
        }
        let ids = self.workspace.active_ids();
        let panes: Vec<_> = ids
            .iter()
            .filter_map(|id| self.panes.get(id))
            .map(|pane| PaneRender {
                id: pane.id,
                rect: pane.rect,
                screen: &pane.screen,
                selection: pane.selection,
                focused: pane.id == self.workspace.active_pane,
                preedit: (!pane.preedit.is_empty()).then_some(pane.preedit.as_str()),
            })
            .collect();
        renderer.set_interaction(self.pointer, self.ui.settings.reduce_motion);
        renderer.render_workspace(
            &panes,
            &primitives,
            &output.textures_delta,
            ppp,
            self.ui.settings.effect.renderer(),
        )?;
        self.metrics
            .render(render_started.elapsed().as_secs_f64() * 1000.0);
        self.presented(frames_before);
        self.ime_anchor();
        for action in actions {
            if let Err(error) = self.action(action, event_loop) {
                self.ui.notice = Some(error.to_string());
                self.ui.system.record("workspace error", error.to_string());
            }
        }
        if released_primary
            && !event_loop.exiting()
            && self
                .layout_persistence
                .changed(&self.ui.settings, &self.workspace)
        {
            if let Err(error) = self.persist_layout() {
                self.ui.notice = Some(error.to_string());
            }
            self.focus(self.workspace.active_pane);
        }
        Ok(())
    }
    fn presented(&mut self, frames_before: u64) {
        if let Some(renderer) = &self.renderer {
            let frames = renderer.frame_count();
            if frames > frames_before {
                self.metrics.presented(&self.workspace.active_ids());
                self.frames = frames;
                self.redraw_state.surface_presented();
            } else if let Some(error) = renderer.last_surface_error() {
                if let Some(delay) = self.redraw_state.retry_surface(Instant::now()) {
                    eprintln!(
                        "Surface acquire deferred ({error:?}), retry in {} ms",
                        delay.as_millis()
                    );
                }
            }
        }
        if self.frames > 0 && self.first_present_ms.is_none() {
            self.first_present_ms = Some(self.started.elapsed().as_millis());
        }
    }
    fn resize(&mut self) -> Result<()> {
        self.invalidate_ime_anchor();
        if let (Some(window), Some(renderer)) = (&self.window, &mut self.renderer) {
            let size = window.inner_size();
            self.redraw_state.zero_sized = size.width == 0 || size.height == 0;
            if !self.redraw_state.can_draw() {
                self.redraw_state.clear_repaints();
            }
            renderer.resize(size, window.scale_factor())?;
            if self.probe.is_some() {
                let grid = renderer.dimensions();
                let rect = [
                    0.0,
                    0.0,
                    window.inner_size().width as f32,
                    window.inner_size().height as f32,
                ];
                let pane = self.active_mut();
                pane.rect = rect;
                pane.resize(grid)?;
            }
        }
        self.redraw();
        Ok(())
    }
    fn shortcut(&mut self, event: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        let key = match &event.logical_key {
            Key::Character(text) => match text.as_str() {
                "{" if self.modifiers.shift_key() => "[".into(),
                "}" if self.modifiers.shift_key() => "]".into(),
                value => value.to_lowercase(),
            },
            Key::Named(key) => format!("{key:?}").to_lowercase(),
            _ => return false,
        };
        let modifiers = u8::from(self.modifiers.super_key())
            | (u8::from(self.modifiers.control_key()) << 1)
            | (u8::from(self.modifiers.alt_key()) << 2)
            | (u8::from(self.modifiers.shift_key()) << 3);
        #[cfg(target_os = "macos")]
        let modifierless = self
            .modifiers
            .super_key()
            .then(|| event.key_without_modifiers());
        #[cfg(not(target_os = "macos"))]
        let modifierless = None;
        let Some(shortcut) =
            shortcut_binding(&self.ui.settings, &key, modifiers, modifierless.as_ref())
        else {
            return self.modifiers.super_key() && self.terminal_focused;
        };
        #[cfg(target_os = "macos")]
        if shortcut == Shortcut::Quit && self.probe.is_none() {
            if let Some(chord) = macos_menu::QuitChord::from_key_event(event, self.modifiers) {
                macos_menu::queue_quit_press(
                    chord,
                    event.repeat,
                    macos_menu::current_key_down_time(),
                );
            } else if !event.repeat {
                let result = self.request_quit(None, event_loop);
                self.input_result(result);
            }
            return true;
        }
        self.dispatch_shortcut(shortcut, event_loop)
    }
    fn dispatch_shortcut(&mut self, shortcut: Shortcut, event_loop: &ActiveEventLoop) -> bool {
        self.cancel_mouse_capture();
        let action = match shortcut {
            Shortcut::Dismiss => {
                if !self.ui.overlay_open() && self.object.is_none() {
                    return false;
                }
                self.ui.close_overlays();
                self.object = None;
                self.focus(self.workspace.active_pane);
                return true;
            }
            Shortcut::Quit => {
                #[cfg(target_os = "macos")]
                if self.probe.is_none() {
                    let result = self.request_quit(None, event_loop);
                    self.input_result(result);
                    return true;
                }
                self.close_session();
                event_loop.exit();
                return true;
            }
            Shortcut::NewShell => Action::NewShell,
            Shortcut::SplitRight => Action::Split(Axis::Horizontal),
            Shortcut::SplitBelow => Action::Split(Axis::Vertical),
            Shortcut::ClosePane => Action::ClosePane(self.workspace.active_pane),
            Shortcut::CloseTab => Action::CloseTab(self.workspace.active_tab),
            Shortcut::NextTab => Action::CycleTab(1),
            Shortcut::PreviousTab => Action::CycleTab(-1),
            Shortcut::NextPane => Action::CyclePane(1),
            Shortcut::PreviousPane => Action::CyclePane(-1),
            Shortcut::Tab1
            | Shortcut::Tab2
            | Shortcut::Tab3
            | Shortcut::Tab4
            | Shortcut::Tab5
            | Shortcut::Tab6
            | Shortcut::Tab7
            | Shortcut::Tab8
            | Shortcut::Tab9 => {
                let index = match shortcut {
                    Shortcut::Tab1 => 0,
                    Shortcut::Tab2 => 1,
                    Shortcut::Tab3 => 2,
                    Shortcut::Tab4 => 3,
                    Shortcut::Tab5 => 4,
                    Shortcut::Tab6 => 5,
                    Shortcut::Tab7 => 6,
                    Shortcut::Tab8 => 7,
                    _ => 8,
                };
                Action::ActivateTab(index)
            }
            Shortcut::Commands => {
                self.cancel_composition();
                self.ui.open_commands();
                self.terminal_focused = false;
                self.redraw();
                return true;
            }
            Shortcut::Search => {
                self.cancel_composition();
                self.ui.open_search();
                self.terminal_focused = false;
                self.redraw();
                return true;
            }
            Shortcut::Settings => {
                self.cancel_composition();
                self.ui.settings_open = true;
                self.terminal_focused = false;
                self.redraw();
                return true;
            }
            Shortcut::Copy | Shortcut::Paste => {
                if !self.terminal_focused {
                    return false;
                }
                if shortcut == Shortcut::Copy {
                    self.copy();
                } else {
                    self.paste();
                }
                return true;
            }
            Shortcut::HistoryPageUp | Shortcut::HistoryPageDown => {
                if !self.terminal_focused {
                    return false;
                }
                let rows = i64::from(self.active().grid.1.saturating_sub(1));
                self.active_mut()
                    .scroll(if shortcut == Shortcut::HistoryPageUp {
                        rows
                    } else {
                        -rows
                    });
                self.redraw();
                return true;
            }
            Shortcut::ReloadConfig => Action::ReloadConfig,
            Shortcut::Mcp => {
                self.ui.mcp.open = true;
                self.terminal_focused = false;
                self.redraw();
                return true;
            }
        };
        if let Err(error) = self.action(action, event_loop) {
            self.ui.notice = Some(error.to_string());
        }
        true
    }
    fn keyboard(&mut self, event: &winit::event::KeyEvent) -> Result<()> {
        if event.state != ElementState::Pressed
            || !self.terminal_input()
            || !self.active().preedit.is_empty()
        {
            return Ok(());
        }
        let modifiers = self.modifier_bits();
        if let Key::Named(key) = event.logical_key {
            if let Some(name) = terminal_key(key) {
                return self.key(name, modifiers);
            }
            return Ok(());
        }
        if self.modifiers.control_key() {
            if let Key::Character(text) = &event.logical_key {
                return self.key(text, modifiers);
            }
        } else if let Some(text) = &event.text {
            if self.modifiers.alt_key() && !cfg!(target_os = "macos") {
                return self.commit(&format!("\x1b{text}"));
            }
            return self.commit(text);
        }
        Ok(())
    }
    fn modifier_bits(&self) -> u8 {
        u8::from(self.modifiers.shift_key())
            | (u8::from(self.modifiers.alt_key()) << 1)
            | (u8::from(self.modifiers.control_key()) << 2)
    }
    fn pane_at(&self, position: PhysicalPosition<f64>) -> Option<u64> {
        self.workspace.active_ids().into_iter().find(|id| {
            let r = self.panes[id].rect;
            position.x >= f64::from(r[0])
                && position.y >= f64::from(r[1])
                && position.x < f64::from(r[0] + r[2])
                && position.y < f64::from(r[1] + r[3])
        })
    }
    fn cell(&self, id: u64) -> Option<(u16, u16)> {
        let renderer = self.renderer.as_ref()?;
        let pane = self.panes.get(&id)?;
        Some(if self.probe.is_some() {
            renderer.cell_at(self.pointer)
        } else {
            renderer.cell_at_in(self.pointer, pane.rect)
        })
    }
    fn terminal_pointer(&self) -> Option<u64> {
        let id = self.pane_at(self.pointer)?;
        let scale = f64::from(self.gui.pixels_per_point());
        let order = self
            .gui
            .layer_id_at(egui::pos2(
                (self.pointer.x / scale) as f32,
                (self.pointer.y / scale) as f32,
            ))
            .map(|layer| layer.order);
        ui::terminal_pointer_route(
            true,
            order,
            self.object.is_some() || self.ui.blocks_terminal(),
        )
        .then_some(id)
    }
    fn release_mouse_capture(&mut self, capture: CapturedButton) {
        let modifiers = self.modifier_bits();
        let cell = self.cell(capture.pane);
        let Some(pane) = self.panes.get_mut(&capture.pane) else {
            return;
        };
        if capture.native {
            if let (Some(cell), Some(session)) = (cell, &mut pane.session) {
                let result = session.mouse(cell.0, cell.1, capture.button, false, modifiers);
                self.input_result(result);
            }
        } else {
            pane.selection_anchor = None;
            self.redraw();
        }
    }
    fn cancel_mouse_capture(&mut self) {
        for capture in self.mouse_capture.cancel().into_iter().flatten() {
            self.release_mouse_capture(capture);
        }
        for pane in self.panes.values_mut() {
            pane.selection_anchor = None;
        }
    }
    fn finish_command_press(&mut self) {
        if let Some(press) = self.command_press.take() {
            if !press.moved {
                self.object = press.object;
                self.redraw();
            }
        }
    }
    fn drag_window(&mut self) {
        let Some(mut press) = self.command_press.take() else {
            return;
        };
        #[cfg(target_os = "linux")]
        if let Some(window) = &self.window {
            let dx = self.pointer.x - press.pointer.x;
            let dy = self.pointer.y - press.pointer.y;
            if dx.hypot(dy) > 4.0 * window.scale_factor() {
                press.moved = true;
            }
        }
        #[cfg(not(target_os = "linux"))]
        if let Some(window) = &self.window {
            if let (Some(origin), Ok(current)) = (press.origin, window.outer_position()) {
                let x = f64::from(current.x) + self.pointer.x;
                let y = f64::from(current.y) + self.pointer.y;
                let dx = x - (f64::from(origin.x) + press.pointer.x);
                let dy = y - (f64::from(origin.y) + press.pointer.y);
                if dx.hypot(dy) > 4.0 * window.scale_factor() {
                    press.moved = true;
                }
                if press.blank && press.moved {
                    let position = winit::dpi::PhysicalPosition::new(
                        origin.x.saturating_add(dx.round() as i32),
                        origin.y.saturating_add(dy.round() as i32),
                    );
                    if position != current {
                        window.set_outer_position(position);
                    }
                }
            }
        }
        self.command_press = Some(press);
    }
    fn mouse(&mut self, state: ElementState, button: MouseButton) -> Result<()> {
        let Some(code) = mouse_button_code(button) else {
            return Ok(());
        };
        if state == ElementState::Released {
            if let Some(capture) = self.mouse_capture.release(code) {
                self.release_mouse_capture(capture);
            }
            return Ok(());
        }
        let Some(id) = self.pane_at(self.pointer) else {
            self.terminal_focused = false;
            return Ok(());
        };
        self.focus(id);
        let Some(cell) = self.cell(id) else {
            return Ok(());
        };
        #[cfg(windows)]
        let command_modifier = if self.panes[&id].blank_at(cell) {
            self.modifiers.alt_key() && !self.modifiers.super_key() && !self.modifiers.control_key()
        } else {
            self.modifiers.super_key()
        };
        #[cfg(not(windows))]
        let command_modifier = self.modifiers.super_key();
        if command_modifier && button == MouseButton::Left {
            #[cfg(target_os = "linux")]
            if self.panes[&id].blank_at(cell) {
                if let Some(window) = &self.window {
                    if let Err(error) = window.drag_window() {
                        self.ui.notice = Some(format!(
                            "The window manager could not move this window: {error}"
                        ));
                        self.redraw();
                    }
                }
                return Ok(());
            }
            self.command_press = Some(CommandPress {
                pointer: self.pointer,
                #[cfg(not(target_os = "linux"))]
                origin: self
                    .window
                    .as_ref()
                    .and_then(|window| window.outer_position().ok()),
                #[cfg(not(target_os = "linux"))]
                blank: self.panes[&id].blank_at(cell),
                object: self.panes[&id].object_at(cell),
                moved: false,
            });
            return Ok(());
        }
        let modifiers = self.modifier_bits();
        let native = self.panes[&id].live_screen.mouse_mode > 0 && !self.modifiers.shift_key();
        if let Some(previous) = self.mouse_capture.release(code) {
            self.release_mouse_capture(previous);
        }
        let capture = CapturedButton {
            pane: id,
            button: code,
            native,
        };
        if native {
            let result = self
                .panes
                .get_mut(&id)
                .and_then(|pane| pane.session.as_mut())
                .context("No active Shell")?
                .mouse(cell.0, cell.1, code, true, modifiers);
            if result.is_ok() {
                let _ = self.mouse_capture.press(capture);
            }
            self.input_result(result);
        } else if button == MouseButton::Left {
            let _ = self.mouse_capture.press(capture);
            let pane = self.panes.get_mut(&id).unwrap();
            pane.clear_selection();
            pane.selection_anchor = Some(cell);
            self.redraw();
        }
        Ok(())
    }
    fn wheel(&mut self, delta: MouseScrollDelta, phase: TouchPhase) -> Result<()> {
        let Some(id) = self.pane_at(self.pointer) else {
            return Ok(());
        };
        let Some(cell) = self.cell(id) else {
            return Ok(());
        };
        let line_height = self
            .renderer
            .as_ref()
            .map_or(20.0, |renderer| renderer.cell_metrics()[1]);
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y) * 3.0,
            MouseScrollDelta::PixelDelta(position) => position.y / f64::from(line_height.max(1.0)),
        };
        if !lines.is_finite() {
            return Ok(());
        }
        let modifiers = self.modifier_bits();
        let pane = self.panes.get_mut(&id).unwrap();
        if phase == TouchPhase::Started {
            pane.wheel_remainder = 0.0;
        }
        pane.wheel_remainder += lines;
        let amount = pane.wheel_remainder.trunc();
        pane.wheel_remainder -= amount;
        if pane.live_screen.mouse_mode > 0 && !self.modifiers.shift_key() {
            if let Some(session) = &mut pane.session {
                for _ in 0..(amount.abs() as usize).min(128) {
                    session.mouse(
                        cell.0,
                        cell.1,
                        if amount > 0.0 { 4 } else { 5 },
                        true,
                        modifiers,
                    )?;
                }
            }
        } else {
            pane.scroll(amount as i64);
        }
        self.redraw();
        Ok(())
    }
    #[cfg(target_os = "macos")]
    fn clipboard(&mut self) -> Result<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new()?);
        }
        Ok(self.clipboard.as_mut().unwrap())
    }
    fn copy(&mut self) {
        #[cfg(target_os = "android")]
        {
            let result = self.active().selected_text().and_then(|text| {
                if let Some(text) = text {
                    android_input::copy_text(text)?;
                }
                Ok(())
            });
            self.input_result(result);
        }
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        {
            let result = self.active().selected_text().and_then(|text| {
                if let Some(text) = text {
                    #[cfg(target_os = "macos")]
                    self.clipboard()?.set_text(text)?;
                    #[cfg(any(target_os = "linux", windows))]
                    self.gui_state
                        .as_mut()
                        .context("The native clipboard is unavailable")?
                        .set_clipboard_text(text);
                }
                Ok(())
            });
            self.input_result(result);
        }
    }
    fn paste(&mut self) {
        #[cfg(target_os = "android")]
        {
            let result = android_input::request_paste();
            self.input_result(result);
        }
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        {
            let id = self.workspace.active_pane;
            let revision = self
                .active()
                .session
                .as_ref()
                .map(|session| session.output_revision());
            #[cfg(target_os = "macos")]
            let text = self
                .clipboard()
                .and_then(|clipboard| Ok(clipboard.get_text()?));
            #[cfg(any(target_os = "linux", windows))]
            let text = self
                .gui_state
                .as_mut()
                .context("The native clipboard is unavailable")
                .and_then(|state| {
                    state
                        .clipboard_text()
                        .context("The system clipboard has no text")
                });
            let result = text.and_then(|text| self.active_mut().paste(&text));
            if result.is_ok() {
                if let Some(revision) = revision {
                    self.metrics.input(id, revision);
                }
            }
            self.input_result(result);
        }
    }
    fn find(&mut self, query: &str, forward: bool) {
        let result = self.active_mut().find(query, forward);
        if result.is_err() {
            self.input_result(result);
        } else {
            self.redraw();
        }
    }
    fn object_dialog(&mut self, ctx: &egui::Context) {
        let Some(object) = self.object.clone() else {
            return;
        };
        if let OpenObject::Process(pid) = object {
            self.object = None;
            self.ui.settings.system_open = true;
            self.ui.system.inspect_pid(pid);
            self.terminal_focused = false;
            self.redraw();
            return;
        }
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Open terminal object")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(object.label());
                ui.monospace(object.target());
                ui.horizontal(|ui| {
                    confirm = ui
                        .button(if matches!(object, OpenObject::File(_)) {
                            if cfg!(target_os = "macos") {
                                "Reveal in Finder"
                            } else {
                                "Open file"
                            }
                        } else {
                            "Open in browser"
                        })
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if confirm {
            let result = object.open();
            self.object = None;
            self.input_result(result);
            self.ui.system.record(
                "user-confirmed object",
                "Requested an external object through the OS",
            );
        }
        if cancel {
            self.object = None;
        }
        if confirm || cancel {
            self.focus(self.workspace.active_pane);
        }
    }
    #[cfg(target_os = "macos")]
    fn cancel_quit_hold(&mut self) {
        self.quit_hold = None;
        self.quit_double_press.reset();
        if self.ui.notice.as_deref().is_some_and(|notice| {
            notice.starts_with("Hold the quit shortcut")
                || notice.starts_with("Press the quit shortcut again")
        }) {
            self.ui.notice = None;
            self.redraw();
        }
    }

    #[cfg(target_os = "macos")]
    fn release_quit_shortcut(&mut self) {
        self.quit_hold = None;
        self.quit_double_press.release();
        if self.ui.settings.quit_protection.mode == config::QuitMode::HoldOrDoublePress
            && self.quit_double_press.deadline().is_some()
        {
            self.ui.notice = Some("Press the quit shortcut again to quit.".into());
            self.redraw();
        } else {
            self.cancel_quit_hold();
        }
    }

    #[cfg(target_os = "macos")]
    fn request_quit(
        &mut self,
        press: Option<(macos_menu::QuitChord, Instant)>,
        event_loop: &ActiveEventLoop,
    ) -> Result<()> {
        if self.probe.is_some() {
            self.close_session();
            event_loop.exit();
            return Ok(());
        }
        if self.quit_focus.is_some() {
            return Ok(());
        }
        let protection = &self.ui.settings.quit_protection;
        if protection.mode != config::QuitMode::Confirm {
            if let Some((chord, pressed_at)) = press {
                let combined = protection.mode == config::QuitMode::HoldOrDoublePress;
                if combined
                    && self.quit_double_press.press(
                        chord,
                        pressed_at,
                        Duration::from_millis(u64::from(protection.double_press_interval_ms)),
                    )
                {
                    self.cancel_quit_hold();
                    self.close_session();
                    event_loop.exit();
                    return Ok(());
                }
                if self.quit_hold.is_none() {
                    let duration = Duration::from_millis(u64::from(protection.hold_duration_ms));
                    self.quit_hold = Some(QuitHold {
                        // Let queued releases settle before a delayed press can
                        // trigger the timer. A release still uses AppKit time.
                        deadline: Instant::now() + duration,
                        release_deadline: pressed_at + duration,
                        chord,
                    });
                    self.ui.notice = Some(if combined {
                        format!("Hold the quit shortcut for {:.1} seconds or press it again within {:.1} seconds to quit.",
                            f32::from(protection.hold_duration_ms) / 1000.0,
                            f32::from(protection.double_press_interval_ms) / 1000.0)
                    } else {
                        format!(
                            "Hold the quit shortcut for {:.1} seconds to quit.",
                            f32::from(protection.hold_duration_ms) / 1000.0
                        )
                    });
                    self.redraw();
                }
                return Ok(());
            }
        }
        self.cancel_quit_hold();
        let previous_focus = QuitFocus {
            terminal_focused: self.terminal_focused,
            egui_focus: self.gui.memory(|memory| memory.focused()),
        };
        self.cancel_mouse_capture();
        self.cancel_composition();
        let window = self
            .window
            .as_ref()
            .context("Quit confirmation requires a window")?;
        window.set_minimized(false);
        window.focus_window();
        self.quit_focus = Some(previous_focus);
        let session_count = self.panes.len();
        let result = self
            .macos_menu
            .as_mut()
            .context("macOS menu is unavailable")
            .and_then(|menu| menu.confirm_quit(window, session_count));
        if result.is_err() {
            self.quit_focus = None;
        }
        result
    }

    #[cfg(target_os = "macos")]
    fn macos_menu_action(
        &mut self,
        command: macos_menu::MenuCommand,
        event_loop: &ActiveEventLoop,
    ) -> Result<()> {
        use macos_menu::MenuCommand;
        let shortcut = match command {
            MenuCommand::NewTab => Some(Shortcut::NewShell),
            MenuCommand::SplitRight => Some(Shortcut::SplitRight),
            MenuCommand::SplitBelow => Some(Shortcut::SplitBelow),
            MenuCommand::ClosePane => Some(Shortcut::ClosePane),
            MenuCommand::CloseTab => Some(Shortcut::CloseTab),
            MenuCommand::Settings => Some(Shortcut::Settings),
            MenuCommand::ReloadConfig => Some(Shortcut::ReloadConfig),
            MenuCommand::Find => Some(Shortcut::Search),
            MenuCommand::Commands => Some(Shortcut::Commands),
            MenuCommand::Mcp => Some(Shortcut::Mcp),
            _ => None,
        };
        if let Some(shortcut) = shortcut {
            self.dispatch_shortcut(shortcut, event_loop);
            return Ok(());
        }
        match command {
            MenuCommand::Quit | MenuCommand::CloseWindow => {
                self.request_quit(None, event_loop)?
            }
            MenuCommand::Copy if self.terminal_input() => self.copy(),
            MenuCommand::Paste if self.terminal_input() => self.paste(),
            MenuCommand::Copy => {
                if let Some(state) = &mut self.gui_state {
                    state.egui_input_mut().events.push(egui::Event::Copy);
                }
            }
            MenuCommand::Paste => {
                let text = self.clipboard()?.get_text()?;
                if let Some(state) = &mut self.gui_state {
                    state.egui_input_mut().events.push(egui::Event::Paste(text));
                }
            }
            MenuCommand::SelectAll if self.terminal_input() => {
                let pane = self.active_mut();
                pane.selection = Some(terminal_render::Selection {
                    start: (0, 0),
                    end: (pane.screen.rows.saturating_sub(1), pane.screen.cols),
                });
            }
            MenuCommand::SelectAll => {
                if let Some(state) = &mut self.gui_state {
                    for pressed in [true, false] {
                        state.egui_input_mut().events.push(egui::Event::Key {
                            key: egui::Key::A,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers: egui::Modifiers {
                                mac_cmd: true,
                                command: true,
                                ..Default::default()
                            },
                        });
                    }
                }
            }
            MenuCommand::SystemPanel => {
                self.cancel_composition();
                self.ui.settings.system_open = !self.ui.settings.system_open;
                self.terminal_focused = !self.ui.settings.system_open;
                if self.ui.settings.system_open {
                    self.ui.system.refresh();
                }
                self.persist_layout()?;
            }
            MenuCommand::IncreaseFont | MenuCommand::DecreaseFont | MenuCommand::ResetFont => {
                let size = match command {
                    MenuCommand::IncreaseFont => self.ui.settings.font_size + 1.0,
                    MenuCommand::DecreaseFont => self.ui.settings.font_size - 1.0,
                    _ => Settings::default().font_size,
                };
                self.ui.settings.font_size = size.clamp(10.0, 28.0);
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_font_size(self.ui.settings.font_size);
                }
                self.resize()?;
                self.persist_layout()?;
            }
            MenuCommand::Minimize => {
                if let Some(window) = &self.window {
                    window.set_minimized(true);
                }
            }
            MenuCommand::Zoom => {
                if let Some(window) = &self.window {
                    window.set_maximized(!window.is_maximized());
                }
            }
            MenuCommand::Fullscreen => {
                if let Some(window) = &self.window {
                    window.set_fullscreen(if window.fullscreen().is_some() {
                        None
                    } else {
                        Some(winit::window::Fullscreen::Borderless(None))
                    });
                }
            }
            MenuCommand::BringToFront => {
                if let Some(window) = &self.window {
                    window.set_minimized(false);
                    window.focus_window();
                }
            }
            _ => {}
        }
        self.redraw();
        Ok(())
    }

    fn probe_tick(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let Some(mut probe) = self.probe.take() else {
            return Ok(());
        };
        let result = probe.tick(self, event_loop);
        if result.is_err() {
            probe.write_result(self, false)?;
        }
        self.probe = Some(probe);
        result
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            if let Err(error) = self.initialize(event_loop) {
                self.fail(event_loop, error);
            }
        }
    }
    fn suspended(&mut self, _: &ActiveEventLoop) {
        #[cfg(target_os = "macos")]
        {
            macos_menu::reset_quit_tracking();
            self.cancel_quit_hold();
        }
        #[cfg(target_os = "macos")]
        drop(self.hover_buttons.take());
        self.cancel_mouse_capture();
        self.invalidate_ime_anchor();
        self.command_press = None;
        self.redraw_state.occluded = true;
        self.redraw_state.clear_repaints();
        self.renderer = None;
        self.window = None;
        self.gui_state = None;
        for pane in self.panes.values_mut() {
            pane.clear_preedit();
            #[cfg(not(target_os = "android"))]
            pane.clear_selection();
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        if event_loop.exiting() {
            return;
        }
        let result = match event {
            #[cfg(target_os = "macos")]
            Wake::MacMenu(command) => {
                let result = self.macos_menu_action(command, event_loop);
                self.input_result(result);
                Ok(())
            }
            #[cfg(target_os = "macos")]
            Wake::MacQuitKeyDown(chord, pressed_at, generation) => {
                if generation == macos_menu::quit_tracking_generation() {
                    let result = self.request_quit(Some((chord, pressed_at)), event_loop);
                    self.input_result(result);
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            Wake::MacQuitKeyReleased(chord, released_at, generation) => {
                if generation == macos_menu::quit_tracking_generation() {
                    if self.quit_hold.as_ref().is_some_and(|hold| {
                        hold.completed_by_release(chord, released_at, self.window_focused)
                    }) {
                        self.cancel_quit_hold();
                        self.close_session();
                        event_loop.exit();
                    } else {
                        self.release_quit_shortcut();
                    }
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            Wake::MacQuitDecision(quit) => {
                if let Some(menu) = &mut self.macos_menu {
                    menu.finish_confirmation();
                }
                self.cancel_quit_hold();
                let previous_focus = self.quit_focus.take();
                if quit {
                    self.close_session();
                    event_loop.exit();
                } else if let Some(previous) = previous_focus {
                    self.terminal_focused = previous.terminal_focused;
                    self.gui.memory_mut(|memory| match previous.egui_focus {
                        Some(id) => memory.request_focus(id),
                        None => {
                            if let Some(id) = memory.focused() {
                                memory.surrender_focus(id);
                            }
                        }
                    });
                    self.ime_anchor();
                    self.redraw();
                }
                Ok(())
            }
            #[cfg(target_os = "android")]
            Wake::AndroidInput(input) => self.android_input(event_loop, input),
            Wake::Session => self.receive(event_loop),
            Wake::Commit(text) if self.terminal_input() && !self.discard_ime_commit => {
                self.commit(&text)
            }
            Wake::Preedit(text) if self.terminal_input() => {
                if self
                    .panes
                    .get_mut(&self.workspace.active_pane)
                    .is_some_and(|pane| pane.preedit(text))
                {
                    self.redraw();
                }
                Ok(())
            }
            Wake::Key { name, modifiers } if self.terminal_input() => self.key(&name, modifiers),
            Wake::Commit(_) | Wake::Preedit(_) | Wake::Key { .. } => Ok(()),
            Wake::System => {
                self.ui.system.receive();
                for reply in self.ui.system.take_shell_replies() {
                    if let Some(pane) = self.panes.get(&reply.pane_id).filter(|pane| {
                        pane.shell_session_id.as_deref() == Some(reply.session_id.as_str())
                    }) {
                        if let Some(session) = &pane.session {
                            if let Err(error) = session.host_operation_result(
                                reply.request_id,
                                reply.status,
                                reply.result,
                                reply.error,
                            ) {
                                self.ui
                                    .system
                                    .record("Shell host channel", error.to_string());
                            }
                        }
                    }
                }
                self.ui.mcp.poll();
                self.redraw();
                Ok(())
            }
            Wake::Repaint(delay) => {
                if self.redraw_state.repaint_after(delay) {
                    self.redraw();
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            self.fail(event_loop, error);
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if event_loop.exiting() {
            return;
        }
        if self.window.as_ref().map(|window| window.id()) != Some(id) {
            return;
        }
        match &event {
            WindowEvent::Occluded(occluded) => self.redraw_state.occluded = *occluded,
            WindowEvent::Resized(size) => {
                self.redraw_state.zero_sized = size.width == 0 || size.height == 0;
            }
            _ => {}
        }
        if !self.redraw_state.can_draw() {
            self.redraw_state.clear_repaints();
        }
        if self
            .panes
            .get(&self.workspace.active_pane)
            .is_some_and(|pane| {
                unchanged_terminal_preedit(
                    &event,
                    self.terminal_input(),
                    &pane.preedit,
                    pane.preedit_cursor,
                )
            })
        {
            return;
        }
        let pointer_repaint = match &event {
            WindowEvent::CursorMoved { position, .. } => self.pointer_redraw(*position),
            _ => true,
        };
        let discard_ime = self.discard_ime_commit
            && matches!(
                &event,
                WindowEvent::Ime(Ime::Commit(_) | Ime::Preedit(_, _))
            );
        let mut egui_repaint = false;
        if let (Some(window), Some(state)) = (&self.window, &mut self.gui_state) {
            if !discard_ime {
                if !pointer_repaint && matches!(event, WindowEvent::CursorMoved { .. }) {
                    state
                        .egui_input_mut()
                        .events
                        .retain(|event| !matches!(event, egui::Event::PointerMoved(_)));
                }
                let response = state.on_window_event(window, &event);
                egui_repaint = response.repaint
                    && pointer_repaint
                    && !matches!(event, WindowEvent::RedrawRequested);
            }
        }
        if egui_repaint {
            self.redraw();
        }
        let result = match event {
            WindowEvent::CloseRequested => {
                #[cfg(target_os = "macos")]
                if self.probe.is_none() {
                    let result = self.request_quit(None, event_loop);
                    self.input_result(result);
                    return;
                }
                self.close_session();
                event_loop.exit();
                Ok(())
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.resize(),
            WindowEvent::Occluded(_) => {
                self.redraw();
                Ok(())
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                Ok(())
            }
            WindowEvent::Focused(focused) => {
                self.invalidate_ime_anchor();
                self.window_focused = focused;
                if !focused {
                    #[cfg(target_os = "macos")]
                    {
                        macos_menu::reset_quit_tracking();
                        self.cancel_quit_hold();
                    }
                    self.cancel_mouse_capture();
                    self.command_press = None;
                    self.cancel_composition();
                    self.modifiers = ModifiersState::empty();
                    if let Some(pane) = self.panes.get_mut(&self.workspace.active_pane) {
                        pane.clear_preedit();
                        pane.selection_anchor = None;
                    }
                }
                if let Some(session) = self
                    .panes
                    .get_mut(&self.workspace.active_pane)
                    .and_then(|pane| pane.session.as_mut())
                {
                    let _ = session.focus(focused);
                }
                Ok(())
            }
            WindowEvent::Ime(Ime::Enabled) => {
                self.invalidate_ime_anchor();
                self.discard_ime_commit = false;
                Ok(())
            }
            WindowEvent::Ime(Ime::Preedit(text, cursor))
                if self.terminal_input() && !discard_ime =>
            {
                if self.active_mut().update_preedit(text, cursor) {
                    self.redraw();
                }
                Ok(())
            }
            WindowEvent::Ime(Ime::Commit(text)) if self.terminal_input() && !discard_ime => {
                self.commit(&text)
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.invalidate_ime_anchor();
                if let Some(pane) = self.panes.get_mut(&self.workspace.active_pane) {
                    pane.clear_preedit();
                }
                Ok(())
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                if self.shortcut(&event, event_loop) {
                    Ok(())
                } else {
                    self.keyboard(&event)
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                #[cfg(target_os = "macos")]
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && self.window.as_ref().is_some_and(|window| {
                        window.fullscreen().is_none()
                            && self.hover_buttons.as_ref().is_some_and(|buttons| {
                                buttons.drag_region_contains(self.pointer, window.scale_factor())
                            })
                    })
                {
                    // The AppKit overlay normally owns the corner. Keep the
                    // existing window drag path if input arrives through winit.
                    self.cancel_mouse_capture();
                    self.command_press = Some(CommandPress {
                        pointer: self.pointer,
                        origin: self
                            .window
                            .as_ref()
                            .and_then(|window| window.outer_position().ok()),
                        blank: true,
                        object: self.object.clone(),
                        moved: false,
                    });
                    return;
                }
                #[cfg(any(target_os = "linux", windows))]
                if state == ElementState::Pressed && button == MouseButton::Left {
                    if let Some(direction) = self.resize_direction() {
                        self.cancel_composition();
                        self.cancel_mouse_capture();
                        self.command_press = None;
                        if let Some(window) = &self.window {
                            if let Err(error) = window.drag_resize_window(direction) {
                                self.ui.notice = Some(format!(
                                    "The window manager could not resize this window: {error}"
                                ));
                                self.redraw();
                            }
                        }
                        return;
                    }
                }
                #[cfg(windows)]
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && self.windows_drag_strip_hit()
                {
                    self.cancel_composition();
                    self.cancel_mouse_capture();
                    self.command_press = None;
                    if let Some(window) = &self.window {
                        if let Err(error) = window.drag_window() {
                            self.ui.notice = Some(format!(
                                "The window manager could not move this window: {error}"
                            ));
                        }
                    }
                    return;
                }
                let result = if state == ElementState::Released
                    && button == MouseButton::Left
                    && self.command_press.is_some()
                {
                    self.finish_command_press();
                    Ok(())
                } else if state == ElementState::Released
                    && mouse_button_code(button)
                        .is_some_and(|code| self.mouse_capture.get(code).is_some())
                {
                    self.mouse(state, button)
                } else if self.terminal_pointer().is_none() {
                    if state == ElementState::Pressed {
                        self.cancel_composition();
                        self.terminal_focused = false;
                        self.cancel_mouse_capture();
                    }
                    for pane in self.panes.values_mut() {
                        pane.selection_anchor = None;
                    }
                    Ok(())
                } else {
                    self.mouse(state, button)
                };
                if state == ElementState::Released
                    && self.probe.is_none()
                    && self
                        .layout_persistence
                        .changed(&self.ui.settings, &self.workspace)
                {
                    if let Err(error) = self.persist_layout() {
                        self.ui.notice = Some(error.to_string());
                    }
                    if button == MouseButton::Left {
                        self.focus(self.workspace.active_pane);
                    }
                }
                result
            }
            WindowEvent::CursorMoved { position, .. } => {
                #[cfg(target_os = "macos")]
                if let (Some(buttons), Some(window)) = (&self.hover_buttons, &self.window) {
                    buttons.pointer_moved(position, window.scale_factor());
                }
                self.pointer = position;
                if self.command_press.is_some() {
                    self.drag_window();
                    return;
                }
                let captures = self.mouse_capture.buttons;
                let mut captured = false;
                for (index, capture) in captures.iter().enumerate() {
                    let Some(capture) = capture else { continue };
                    captured = true;
                    if captures[..index].iter().flatten().any(|previous| {
                        previous.pane == capture.pane && previous.native == capture.native
                    }) {
                        continue;
                    }
                    let Some(cell) = self.cell(capture.pane) else {
                        continue;
                    };
                    let modifiers = self.modifier_bits();
                    if let Some(pane) = self.panes.get_mut(&capture.pane) {
                        if !capture.native {
                            pane.drag(cell);
                            self.redraw();
                        } else if pane.live_screen.mouse_mode >= 2 {
                            if let Some(session) = &mut pane.session {
                                let result =
                                    session.mouse_motion(cell.0, cell.1, capture.button, modifiers);
                                self.input_result(result);
                            }
                        }
                    }
                }
                if !captured {
                    if let Some(id) = self.terminal_pointer() {
                        if let Some(cell) = self.cell(id) {
                            let modifiers = self.modifier_bits();
                            let pane = self.panes.get_mut(&id).unwrap();
                            if pane.selection_anchor.is_some() {
                                pane.drag(cell);
                                self.redraw();
                            } else if pane.live_screen.mouse_mode >= 2
                                && !self.modifiers.shift_key()
                            {
                                if let Some(session) = &mut pane.session {
                                    let _ = session.mouse_move(cell.0, cell.1, modifiers);
                                }
                            }
                        }
                    }
                }
                Ok(())
            }
            WindowEvent::CursorLeft { .. } => {
                #[cfg(target_os = "macos")]
                if let Some(buttons) = &self.hover_buttons {
                    buttons.pointer_left();
                }
                self.cancel_mouse_capture();
                Ok(())
            }
            WindowEvent::MouseWheel { delta, phase, .. } if self.terminal_pointer().is_some() => {
                self.wheel(delta, phase)
            }
            WindowEvent::RedrawRequested => self.draw(event_loop),
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(event_loop, error);
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if event_loop.exiting() {
            return;
        }
        let mut deadline: Option<Instant> = None;
        #[cfg(target_os = "macos")]
        if let Some(hold) = &self.quit_hold {
            if Instant::now() >= hold.deadline {
                let held = self.window_focused && macos_menu::quit_chord_is_pressed(hold.chord);
                if held {
                    self.cancel_quit_hold();
                    self.close_session();
                    event_loop.exit();
                    return;
                }
                self.release_quit_shortcut();
            } else {
                deadline = Some(hold.deadline);
            }
        }
        #[cfg(target_os = "macos")]
        {
            if self.quit_double_press.expire(Instant::now()) && self.quit_hold.is_none() {
                self.cancel_quit_hold();
            }
            if let Some(double_deadline) = self.quit_double_press.deadline() {
                deadline = Some(deadline.map_or(double_deadline, |old| old.min(double_deadline)));
            }
        }
        if self.probe.is_some() {
            if let Err(error) = self.probe_tick(event_loop) {
                self.fail(event_loop, error);
            }
            let probe_deadline = Instant::now() + Duration::from_millis(50);
            deadline = Some(deadline.map_or(probe_deadline, |old| old.min(probe_deadline)));
        }
        if !self.redraw_state.can_draw() {
            self.redraw_state.clear_repaints();
        } else if self.redraw_state.take_due_repaint(Instant::now()) {
            self.redraw();
        }
        if let Some(repaint) = self.redraw_state.next_repaint() {
            deadline = Some(deadline.map_or(repaint, |old| old.min(repaint)));
        }
        event_loop.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        if self.probe.is_none() {
            if let Err(error) = self.persist_layout() {
                eprintln!("Settings: {error:#}");
            }
        }
        self.close_session();
        #[cfg(target_os = "linux")]
        drop(self.gui_state.take());
        #[cfg(target_os = "macos")]
        drop(self.clipboard.take());
        #[cfg(target_os = "macos")]
        drop(self.hover_buttons.take());
    }
}

#[derive(Clone)]
enum OpenObject {
    Url(String),
    File(PathBuf),
    Process(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CapturedButton {
    pane: u64,
    button: u8,
    native: bool,
}
#[derive(Default)]
struct MouseCapture {
    buttons: [Option<CapturedButton>; 3],
}
impl MouseCapture {
    fn press(&mut self, capture: CapturedButton) -> Option<CapturedButton> {
        self.buttons[usize::from(capture.button - 1)].replace(capture)
    }
    fn get(&self, button: u8) -> Option<CapturedButton> {
        self.buttons[usize::from(button - 1)]
    }
    fn release(&mut self, button: u8) -> Option<CapturedButton> {
        self.buttons[usize::from(button - 1)].take()
    }
    fn cancel(&mut self) -> [Option<CapturedButton>; 3] {
        std::mem::take(&mut self.buttons)
    }
}
fn mouse_button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        _ => None,
    }
}
struct CommandPress {
    pointer: PhysicalPosition<f64>,
    #[cfg(not(target_os = "linux"))]
    origin: Option<PhysicalPosition<i32>>,
    #[cfg(not(target_os = "linux"))]
    blank: bool,
    object: Option<OpenObject>,
    moved: bool,
}
impl OpenObject {
    fn target(&self) -> String {
        match self {
            Self::Url(url) => url.clone(),
            Self::File(path) => path.display().to_string(),
            Self::Process(pid) => format!("PID {pid}"),
        }
    }
    fn label(&self) -> &str {
        match self {
            Self::Url(_) => "Open this URL using the system browser?",
            Self::File(_) if cfg!(target_os = "macos") => "Reveal this local file using Finder?",
            Self::File(_) => "Open this local file using the system application?",
            Self::Process(_) => "Inspect this process through the system API?",
        }
    }
    fn open(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let mut command = std::process::Command::new("/usr/bin/open");
            match self {
                Self::Url(url) => {
                    command.arg("--").arg(url);
                }
                Self::File(path) => {
                    command.arg("-R").arg("--").arg(path);
                }
                Self::Process(_) => anyhow::bail!("Process inspection belongs to the System panel"),
            }
            anyhow::ensure!(
                command.status()?.success(),
                "The OS could not open this object"
            );
            Ok(())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::UI::{
                Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL,
            };
            let target = match self {
                Self::Url(url) => std::ffi::OsStr::new(url),
                Self::File(path) => path.as_os_str(),
                Self::Process(_) => anyhow::bail!("Process inspection belongs to the System panel"),
            };
            let mut target: Vec<_> = target.encode_wide().collect();
            anyhow::ensure!(!target.contains(&0), "The object contains a null character");
            target.push(0);
            let operation: Vec<_> = "open\0".encode_utf16().collect();
            let result = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    operation.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    SW_SHOWNORMAL,
                )
            };
            anyhow::ensure!(
                (result as isize) > 32,
                "Windows could not open this object ({})",
                result as isize
            );
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            let target = match self {
                Self::Url(url) => url.clone(),
                Self::File(path) => path.to_string_lossy().into_owned(),
                Self::Process(_) => anyhow::bail!("Process inspection belongs to the System panel"),
            };
            let output = system_api::bounded_command(
                "/usr/bin/xdg-open",
                &[target],
                Duration::from_secs(3),
                65_536,
            )?;
            anyhow::ensure!(
                output.success,
                "The desktop could not open this object ({})",
                output.stderr
            );
            Ok(())
        }
        #[cfg(target_os = "android")]
        {
            match self {
                Self::Url(url) => android_input::open_url(url),
                Self::File(path) => android_input::open_file(path),
                Self::Process(_) => anyhow::bail!("Process inspection belongs to the System panel"),
            }
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "android",
            windows
        )))]
        anyhow::bail!("Object opening is unavailable on this platform")
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;

    #[test]
    fn default_font_uses_available_nerd_alias_or_renderer_fallback() {
        let canonical = vec![
            "JetBrainsMono Nerd Font Mono".to_owned(),
            "JetBrainsMono NFM".to_owned(),
        ];
        assert_eq!(
            startup_font_family("JetBrainsMono Nerd Font Mono", &canonical, "Embedded"),
            "JetBrainsMono Nerd Font Mono"
        );
        let alias = vec!["JetBrainsMono NFM".to_owned()];
        assert_eq!(
            startup_font_family("JetBrainsMono Nerd Font Mono", &alias, "Embedded"),
            "JetBrainsMono NFM"
        );
        assert_eq!(
            startup_font_family("JetBrainsMono Nerd Font Mono", &[], "Embedded"),
            "Embedded"
        );
        assert_eq!(
            startup_font_family("Explicit missing custom font", &[], "Embedded"),
            "Explicit missing custom font"
        );
    }

    #[test]
    fn borderless_resize_hit_test_uses_scaled_edges_and_rejects_outside_points() {
        use winit::{dpi::PhysicalSize, window::ResizeDirection};
        let size = PhysicalSize::new(100, 80);
        for (position, expected) in [
            ((2.0, 2.0), ResizeDirection::NorthWest),
            ((50.0, 2.0), ResizeDirection::North),
            ((98.0, 2.0), ResizeDirection::NorthEast),
            ((2.0, 40.0), ResizeDirection::West),
            ((98.0, 40.0), ResizeDirection::East),
            ((2.0, 78.0), ResizeDirection::SouthWest),
            ((50.0, 78.0), ResizeDirection::South),
            ((98.0, 78.0), ResizeDirection::SouthEast),
        ] {
            assert_eq!(
                window_resize_direction(PhysicalPosition::new(position.0, position.1), size, 1.0),
                Some(expected)
            );
        }
        assert_eq!(
            window_resize_direction(PhysicalPosition::new(8.0, 40.0), size, 2.0),
            Some(ResizeDirection::West)
        );
        for pointer in [
            PhysicalPosition::new(8.0, 40.0),
            PhysicalPosition::new(-1.0, 40.0),
            PhysicalPosition::new(100.0, 40.0),
            PhysicalPosition::new(50.0, 80.0),
            PhysicalPosition::new(f64::NAN, 40.0),
        ] {
            assert_eq!(window_resize_direction(pointer, size, 1.0), None);
        }
        assert_eq!(
            window_resize_direction(PhysicalPosition::new(2.0, 40.0), size, f64::INFINITY),
            None
        );
    }

    #[test]
    fn cmd_punctuation_falls_back_to_current_layout_without_losing_modifiers() {
        let settings = Settings::default();
        for (logical, base, modifiers, expected) in [
            ("，", ",", 1, Shortcut::Settings),
            ("，", "，", 1, Shortcut::Settings),
            ("［", "［", 1, Shortcut::PreviousPane),
            ("］", "］", 1, Shortcut::NextPane),
            ("［", "［", 9, Shortcut::PreviousTab),
            ("］", "］", 9, Shortcut::NextTab),
            ("【", "[", 1, Shortcut::PreviousPane),
            ("】", "]", 1, Shortcut::NextPane),
            ("【", "【", 1, Shortcut::PreviousPane),
            ("】", "】", 1, Shortcut::NextPane),
            ("【", "[", 9, Shortcut::PreviousTab),
            ("】", "]", 9, Shortcut::NextTab),
            ("「", "「", 9, Shortcut::PreviousTab),
            ("」", "」", 9, Shortcut::NextTab),
        ] {
            let modifierless = Key::Character(base.into());
            assert_eq!(
                shortcut_binding(&settings, logical, modifiers, Some(&modifierless)),
                Some(expected),
            );
        }
        let unidentified = Key::Unidentified(winit::keyboard::NativeKey::Unidentified);
        for (logical, modifiers, expected) in [
            ("，", 1, Shortcut::Settings),
            ("［", 1, Shortcut::PreviousPane),
            ("］", 1, Shortcut::NextPane),
            ("［", 9, Shortcut::PreviousTab),
            ("］", 9, Shortcut::NextTab),
            ("【", 1, Shortcut::PreviousPane),
            ("】", 1, Shortcut::NextPane),
            ("「", 9, Shortcut::PreviousTab),
            ("」", 9, Shortcut::NextTab),
        ] {
            assert_eq!(
                shortcut_binding(&settings, logical, modifiers, Some(&unidentified)),
                Some(expected),
            );
        }
    }

    #[test]
    fn logical_user_binding_takes_priority_over_punctuation_fallback() {
        let mut settings = Settings::default();
        settings.keybindings.push(config::KeyBinding {
            keys: "Cmd+，".into(),
            action: Shortcut::NewShell,
        });
        for modifierless in [
            Key::Character(",".into()),
            Key::Character("，".into()),
            Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
        ] {
            assert_eq!(
                shortcut_binding(&settings, "，", 1, Some(&modifierless)),
                Some(Shortcut::NewShell),
            );
        }
        for logical in ["「", "」"] {
            settings.keybindings.push(config::KeyBinding {
                keys: format!("Cmd+Shift+{logical}"),
                action: Shortcut::NewShell,
            });
            for modifierless in [
                Key::Character(logical.into()),
                Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
            ] {
                assert_eq!(
                    shortcut_binding(&settings, logical, 9, Some(&modifierless)),
                    Some(Shortcut::NewShell),
                );
            }
        }
    }

    #[test]
    fn punctuation_fallback_uses_the_current_user_configuration() {
        let mut settings = Settings::default();
        settings
            .keybindings
            .retain(|binding| binding.keys != "Cmd+,");
        let modifierless = Key::Character(",".into());
        assert_eq!(
            shortcut_binding(&settings, "，", 1, Some(&modifierless)),
            None
        );
        settings.keybindings.push(config::KeyBinding {
            keys: "Cmd+,".into(),
            action: Shortcut::Search,
        });
        assert_eq!(
            shortcut_binding(&settings, "，", 1, Some(&modifierless)),
            Some(Shortcut::Search),
        );
        for (logical, removed, other) in [("「", "Cmd+Shift+[", "]"), ("」", "Cmd+Shift+]", "[")]
        {
            let mut settings = Settings::default();
            settings
                .keybindings
                .retain(|binding| binding.keys != removed);
            let other_bracket = Key::Character(other.into());
            assert_eq!(
                shortcut_binding(&settings, logical, 9, Some(&other_bracket)),
                None,
            );
        }
    }

    #[test]
    fn modifierless_fallback_is_limited_to_cmd_punctuation() {
        let settings = Settings::default();
        let comma = Key::Character(",".into());
        assert_eq!(shortcut_binding(&settings, "，", 0, Some(&comma)), None);
        assert_eq!(shortcut_binding(&settings, "，", 2, Some(&comma)), None);
        assert_eq!(shortcut_binding(&settings, "，", 5, Some(&comma)), None);
        let letter = Key::Character("d".into());
        assert_eq!(shortcut_binding(&settings, "ｄ", 1, Some(&letter)), None);
        assert_eq!(shortcut_binding(&settings, "，", 1, None), None);
        assert_eq!(shortcut_binding(&settings, "［", 1, None), None);
        assert_eq!(shortcut_binding(&settings, "］", 1, None), None);
        assert_eq!(shortcut_binding(&settings, "「", 9, None), None);
        assert_eq!(shortcut_binding(&settings, "」", 9, None), None);
        assert_eq!(
            shortcut_binding(&settings, ",", 1, None),
            Some(Shortcut::Settings)
        );
        assert_eq!(
            shortcut_binding(&settings, "[", 9, None),
            Some(Shortcut::PreviousTab)
        );
    }
}

#[cfg(test)]
mod layout_persistence_tests {
    use super::*;
    use workspace::SplitTree;

    fn saved_workspace() -> (Workspace, Settings) {
        let mut workspace = Workspace::new();
        let next = workspace.allocate();
        workspace.new_tab(next);
        workspace.activate_tab(0);
        let mut settings = Settings::default();
        settings.layout = workspace.snapshot();
        settings.active_tab = workspace.active_tab;
        (workspace, settings)
    }

    #[test]
    fn saved_startup_tab_survives_unchanged_live_workspace_and_exit_save() {
        let (workspace, previous) = saved_workspace();
        let mut next = previous.clone();
        next.active_tab = 1;
        let mut persistence = LayoutPersistence::default();
        persistence.configuration_applied(&previous, &next, &workspace);
        assert!(!persistence.changed(&next, &workspace));
        persistence.sync_for_save(&mut next, &workspace);
        persistence.sync_for_save(&mut next, &workspace);
        assert_eq!(next.active_tab, 1);
        assert_eq!(
            Workspace::restore(next.layout, next.active_tab).active_tab,
            1
        );
    }

    #[test]
    fn appearance_only_reload_keeps_saving_actual_ui_layout() {
        let (mut workspace, previous) = saved_workspace();
        workspace.activate_tab(1);
        let mut next = previous.clone();
        next.opacity = 0.3;
        let mut persistence = LayoutPersistence::default();
        persistence.configuration_applied(&previous, &next, &workspace);
        assert!(persistence.deferred_from.is_none());
        persistence.sync_for_save(&mut next, &workspace);
        assert_eq!(next.active_tab, 1);
        let pane = workspace.allocate();
        workspace.split(pane, Axis::Horizontal);
        persistence.sync_for_save(&mut next, &workspace);
        assert_eq!(next.layout, workspace.snapshot());
    }

    #[test]
    fn actual_ui_layout_changes_replace_deferred_startup_layout() {
        let (mut workspace, previous) = saved_workspace();
        let mut next = previous.clone();
        next.layout = vec![SplitTree::Pane(42)];
        next.active_tab = 0;
        let mut persistence = LayoutPersistence::default();
        persistence.configuration_applied(&previous, &next, &workspace);
        assert!(!persistence.changed(&next, &workspace));
        let pane = workspace.allocate();
        workspace.split(pane, Axis::Vertical);
        assert!(persistence.changed(&next, &workspace));
        persistence.sync_for_save(&mut next, &workspace);
        assert!(persistence.deferred_from.is_none());
        assert_eq!(next.layout, workspace.snapshot());
        assert_eq!(next.active_tab, workspace.active_tab);
    }

    #[test]
    fn appearance_reload_preserves_existing_deferred_startup_choice() {
        let (workspace, previous) = saved_workspace();
        let mut pending = previous.clone();
        pending.active_tab = 1;
        let mut persistence = LayoutPersistence::default();
        persistence.configuration_applied(&previous, &pending, &workspace);
        let mut next = pending.clone();
        next.font_size = 18.0;
        persistence.configuration_applied(&pending, &next, &workspace);
        persistence.sync_for_save(&mut next, &workspace);
        assert_eq!(next.active_tab, 1);
        assert!(persistence.deferred_from.is_some());
    }
}

// FVPROV1 144536 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDllZGJiYTFmZWMxMWViOGNmYzdkZjMwYmRmNjQ0OGYzYTNkZTczNDY0Zjc0MGM2MzE0NTUwMDhlYWY0YTNmZCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4yIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjIiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNmE4ZTBjYzBhMWZiMWVkYmM2NjJiYzU4NzYwNThlM2NjZmUxNWM2ZmJhM2IyMzI1YzRkMmIwN2E4MGEzYmNjNmM3YWJmYTQwY2QxYjk1OGI5MzY2MDIyMGQ3ZWIyNmFjMjM5ZWYwYmVkZmRlMjMzNTVlNTZlOGM3NjJlNDZjMGYifV19XQ== FVPROV1-END
