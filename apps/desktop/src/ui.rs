use crate::{
    config::{AppearanceUi, Dock, PanelMaterial, Settings},
    mcp_panel::McpPanel,
    pane::TerminalPane,
    system_panel::{Page, SystemPanel},
    workspace::{Axis, PaneLayout, Workspace},
};
use egui::{Color32, Frame, Margin, RichText, Stroke};
use std::collections::BTreeMap;

#[cfg(target_os = "macos")]
use crate::config::{Effect, GpuPowerPreference, Material};
#[cfg(not(target_os = "macos"))]
mod settings_pages;
#[cfg(not(target_os = "macos"))]
mod settings_panel;
#[cfg(not(target_os = "macos"))]
use settings_panel::SettingsPage;

const WINDOWS_DRAG_STRIP_HEIGHT: f32 = 14.0;

fn windows_drag_strip_geometry(size: egui::Vec2) -> Option<(egui::Rect, egui::Rect)> {
    if !size.x.is_finite() || !size.y.is_finite() || size.x < 230.0 || size.y < 20.0 {
        return None;
    }
    let area = egui::Rect::from_min_max(egui::pos2(6.0, 6.0), egui::pos2(size.x - 160.0, 14.0));
    let x = (size.x * 0.5).clamp(area.left() + 28.0, area.right() - 28.0);
    let grip = egui::Rect::from_center_size(egui::pos2(x, 10.0), egui::vec2(56.0, 2.0));
    Some((area, grip))
}

#[cfg(any(windows, test))]
pub(crate) fn windows_drag_strip_hit(point: egui::Pos2, size: egui::Vec2) -> bool {
    windows_drag_strip_geometry(size).is_some_and(|(area, _)| {
        point.x >= area.left()
            && point.x < area.right()
            && point.y >= area.top()
            && point.y < area.bottom()
    })
}

/// Each queued motion batch owns a unique token until its wake is consumed.
#[cfg(any(target_os = "android", test))]
pub(crate) struct AndroidMotionQueue<T> {
    pending: Option<(std::sync::Arc<()>, T)>,
}

#[cfg(any(target_os = "android", test))]
impl<T> Default for AndroidMotionQueue<T> {
    fn default() -> Self {
        Self { pending: None }
    }
}

#[cfg(any(target_os = "android", test))]
impl<T> AndroidMotionQueue<T> {
    pub(crate) fn push(
        &mut self,
        value: T,
        merge: impl FnOnce(T, T) -> T,
    ) -> Option<std::sync::Arc<()>> {
        match self.pending.take() {
            Some((token, previous)) => {
                self.pending = Some((token, merge(previous, value)));
                None
            }
            None => {
                let token = std::sync::Arc::new(());
                self.pending = Some((token.clone(), value));
                Some(token)
            }
        }
    }

    pub(crate) fn take(&mut self, token: &std::sync::Arc<()>) -> Option<T> {
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending, _)| std::sync::Arc::ptr_eq(pending, token))
        {
            self.pending.take().map(|(_, value)| value)
        } else {
            None
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pending = None;
    }
}

/// Cancel Android's primary pointer while preserving completed clicks and keyboard input.
#[cfg(any(target_os = "android", test))]
pub(crate) fn cancel_android_pointer(context: &egui::Context, events: &mut Vec<egui::Event>) {
    let completed_prefix = events
        .iter()
        .rposition(|event| {
            matches!(
                event,
                egui::Event::PointerButton {
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    ..
                }
            )
        })
        .map_or(0, |index| index + 1);
    let previous_press_has_release =
        completed_prefix > 0 && context.input(|input| input.pointer.primary_down());
    let mut index = 0;
    events.retain(|event| {
        let retain = index < completed_prefix
            || !matches!(
                event,
                egui::Event::PointerMoved(_)
                    | egui::Event::MouseMoved(_)
                    | egui::Event::PointerButton {
                        button: egui::PointerButton::Primary,
                        ..
                    }
                    | egui::Event::PointerGone
            );
        index += 1;
        retain
    });
    if !previous_press_has_release {
        context.input_mut(|input| input.pointer = Default::default());
        // An up event from an already cleared pointer cannot be a click. It also
        // clears egui's private potential_click_id before a subsequent gesture.
        let pass = context.cumulative_pass_nr();
        let marker = egui::Id::new("glass.android.pointer.cancel.release");
        let release_queued = context.data_mut(|data| {
            let queued = data.get_temp::<u64>(marker) == Some(pass);
            data.insert_temp(marker, pass);
            queued
        });
        if !release_queued {
            events.insert(
                0,
                egui::Event::PointerButton {
                    pos: egui::Pos2::ZERO,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            );
        }
    }
    // A queued release can complete a press from the previous pass. Keep that
    // pointer state until this real release is consumed, then it is naturally up.
    context.stop_dragging();
    events.push(egui::Event::PointerGone);
}

pub enum Action {
    NewShell,
    NewSsh {
        target: String,
        user: String,
        port: u16,
    },
    Split(Axis),
    ActivateTab(usize),
    FocusCurrent,
    ClosePane(u64),
    CloseTab(usize),
    ApplySettings,
    ReloadConfig,
    SaveConfig(String),
    Search(String, bool),
    SaveDiagnostics,
    ShellOperation {
        pane_id: u64,
        session_id: String,
        operation: String,
        args: Vec<String>,
    },
    PrepareShellObject {
        pane_id: u64,
        session_id: String,
        name: String,
        object: serde_json::Value,
    },
    CycleTab(i32),
    CyclePane(i32),
    #[cfg(target_os = "android")]
    Copy,
    #[cfg(target_os = "android")]
    Paste,
    #[cfg(target_os = "android")]
    ArchLinux,
}

pub struct UiState {
    pub settings: Settings,
    pub system: SystemPanel,
    pub mcp: McpPanel,
    pub settings_open: bool,
    pub commands_open: bool,
    pub search_open: bool,
    pub ssh_open: bool,
    pub search: String,
    pub notice: Option<String>,
    command: String,
    ssh_target: String,
    ssh_user: String,
    ssh_port: u16,
    focus_command: bool,
    focus_search: bool,
    settings_dirty: bool,
    #[cfg(not(target_os = "macos"))]
    settings_page: SettingsPage,
    notice_deadline: Option<std::time::Instant>,
    config_editor: Option<String>,
    shell_program_editor: String,
}

impl UiState {
    pub fn overlay_open(&self) -> bool {
        self.settings_open
            || self.commands_open
            || self.search_open
            || self.ssh_open
            || self.settings.system_open
            || self.mcp.open
            || self.system.blocks_terminal()
    }
    pub fn close_overlays(&mut self) {
        self.settings_open = false;
        self.commands_open = false;
        self.search_open = false;
        self.ssh_open = false;
        self.settings.system_open = false;
        self.system.dismiss_dialogs();
        self.mcp.dismiss();
        self.settings_dirty = true;
    }
    pub fn blocks_terminal(&self) -> bool {
        (!cfg!(target_os = "macos") && self.settings_open)
            || self.commands_open
            || self.ssh_open
            || self.system.blocks_terminal()
    }
    pub fn new(
        mut settings: Settings,
        system: SystemPanel,
        proxy: winit::event_loop::EventLoopProxy<crate::Wake>,
    ) -> Self {
        settings.system_open = false;
        let shell_program_editor = settings
            .shell
            .program
            .clone()
            .filter(|program| program != "@aether")
            .unwrap_or_default();
        Self {
            settings,
            system,
            mcp: McpPanel::new(proxy),
            settings_open: false,
            commands_open: false,
            search_open: false,
            ssh_open: false,
            search: String::new(),
            notice: None,
            command: String::new(),
            ssh_target: String::new(),
            ssh_user: String::new(),
            ssh_port: 22,
            focus_command: false,
            focus_search: false,
            settings_dirty: false,
            #[cfg(not(target_os = "macos"))]
            settings_page: SettingsPage::Terminal,
            notice_deadline: None,
            config_editor: None,
            shell_program_editor,
        }
    }
    pub fn open_commands(&mut self) {
        self.commands_open = true;
        self.focus_command = true;
        self.command.clear();
    }
    pub fn open_search(&mut self) {
        self.search_open = true;
        self.focus_search = true;
    }
    pub fn query_focus_requested(&self) -> bool {
        (self.commands_open && self.focus_command) || (self.search_open && self.focus_search)
    }
    pub fn system_page(&mut self, page: Page) {
        self.settings.system_open = true;
        self.system.page = page;
        self.settings_dirty = true;
        if page != Page::Timeline {
            self.system.refresh();
        }
    }
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        workspace: &mut Workspace,
        panes: &BTreeMap<u64, TerminalPane>,
        fonts: &[String],
        shell_caret: Option<[f32; 4]>,
        windows_drag_enabled: bool,
    ) -> (Vec<PaneLayout>, Vec<Action>) {
        apply_style(ctx, &self.settings);
        let had_dialog = self.overlay_open();
        let mut actions = Vec::new();
        if window_visible(ctx, "Find", self.search_open) {
            let mut open = self.search_open;
            egui::Window::new("Find")
                .open(&mut open)
                .anchor(egui::Align2::RIGHT_TOP, [-12.0, 12.0])
                .collapsible(false)
                .resizable(false)
                .max_width((ctx.screen_rect().width() - 32.0).max(80.0))
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let submitted = query_text_edit(
                            ui,
                            "search_query",
                            &mut self.focus_search,
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("Terminal and history")
                                .desired_width(
                                    (ctx.screen_rect().width() - 64.0).clamp(80.0, 240.0),
                                )
                                .char_limit(1024),
                        );
                        if ui.button("Previous").clicked() {
                            actions.push(Action::Search(self.search.clone(), false));
                        }
                        if ui.button("Next").clicked() || submitted {
                            actions.push(Action::Search(self.search.clone(), true));
                        }
                    });
                });
            if !open {
                self.search_open = false;
            }
        }
        if self.settings.system_open {
            let frame = Frame::new()
                .fill(ctx.style().visuals.panel_fill)
                .stroke(Stroke::NONE)
                .corner_radius(ctx.style().visuals.window_corner_radius)
                .outer_margin(Margin::same(8))
                .inner_margin(Margin::same(14));
            let compact_touch = cfg!(target_os = "android") && ctx.screen_rect().width() < 600.0;
            if compact_touch {
                egui::Window::new("System")
                    .id(egui::Id::new("system_compact"))
                    .title_bar(false)
                    .resizable(false)
                    .fixed_rect(ctx.screen_rect().shrink(8.0))
                    .vscroll(true)
                    .frame(frame)
                    .show(ctx, |ui| {
                        self.system_contents(
                            ui,
                            false,
                            panes.get(&workspace.active_pane),
                            &mut actions,
                        )
                    });
            } else {
                match self.settings.dock {
                    Dock::Left => {
                        egui::SidePanel::left("system_left")
                            .resizable(true)
                            .show_separator_line(false)
                            .default_width(420.0)
                            .width_range(280.0..=760.0)
                            .frame(frame)
                            .show(ctx, |ui| {
                                self.system_contents(
                                    ui,
                                    true,
                                    panes.get(&workspace.active_pane),
                                    &mut actions,
                                )
                            });
                    }
                    Dock::Right => {
                        egui::SidePanel::right("system_right")
                            .resizable(true)
                            .show_separator_line(false)
                            .default_width(420.0)
                            .width_range(280.0..=760.0)
                            .frame(frame)
                            .show(ctx, |ui| {
                                self.system_contents(
                                    ui,
                                    true,
                                    panes.get(&workspace.active_pane),
                                    &mut actions,
                                )
                            });
                    }
                    Dock::Bottom => {
                        egui::TopBottomPanel::bottom("system_bottom")
                            .resizable(true)
                            .show_separator_line(false)
                            .default_height(300.0)
                            .height_range(150.0..=650.0)
                            .frame(frame)
                            .show(ctx, |ui| {
                                self.system_contents(
                                    ui,
                                    true,
                                    panes.get(&workspace.active_pane),
                                    &mut actions,
                                )
                            });
                    }
                }
            }
        }
        let mut layouts = Vec::new();
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(Margin::ZERO),
            )
            .show(ctx, |ui| {
                let mut content = ui.available_rect_before_wrap();
                if windows_drag_enabled {
                    // Keep the terminal's first row below this small transparent
                    // gutter, including while an overlay is open.
                    content.min.y = (content.min.y + WINDOWS_DRAG_STRIP_HEIGHT).min(content.max.y);
                    if !self.overlay_open() {
                        if let Some((drag_area, grip)) =
                            windows_drag_strip_geometry(ctx.screen_rect().size())
                        {
                            ui.painter().rect_filled(
                                grip,
                                1.0,
                                ui.visuals().text_color().gamma_multiply(0.28),
                            );
                            if let Some(pointer) = ctx.pointer_hover_pos().filter(|point| {
                                point.x >= drag_area.left()
                                    && point.x < drag_area.right()
                                    && point.y >= drag_area.top()
                                    && point.y < drag_area.bottom()
                                    && !ctx.input(|input| input.pointer.any_down())
                            }) {
                                // Paint only: no Area, Response or Sense can
                                // capture the native top-strip drag gesture.
                                let color = ui.visuals().text_color();
                                let galley = ui.painter().layout(
                                    "Drag · Ctrl+, Settings · Ctrl+Shift+P Commands".into(),
                                    egui::FontId::proportional(12.0),
                                    color,
                                    (ctx.screen_rect().width() - 28.0).max(60.0),
                                );
                                let size = galley.size() + egui::vec2(16.0, 10.0);
                                let bounds = ctx.screen_rect().shrink(6.0);
                                let position = egui::pos2(
                                    (pointer.x + 12.0).clamp(
                                        bounds.left(),
                                        (bounds.right() - size.x).max(bounds.left()),
                                    ),
                                    (pointer.y + 14.0).clamp(
                                        bounds.top(),
                                        (bounds.bottom() - size.y).max(bounds.top()),
                                    ),
                                );
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_size(position, size),
                                    4.0,
                                    ui.visuals().window_fill,
                                );
                                ui.painter()
                                    .galley(position + egui::vec2(8.0, 5.0), galley, color);
                            }
                        }
                    }
                }
                layouts = workspace.layout(ui, content);
            });
        self.dialogs(ctx, fonts, &mut actions, panes.get(&workspace.active_pane));
        #[cfg(target_os = "android")]
        if !self.settings_open
            && !self.commands_open
            && panes
                .get(&workspace.active_pane)
                .is_some_and(|pane| pane.waiting_for_arch)
        {
            egui::Area::new(egui::Id::new("arch-bootstrap-status"))
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .interactable(false)
                .show(ctx, |ui| {
                    ui.label("Preparing Arch Linux…");
                });
        }
        if !self.overlay_open() {
            if let (Some(pane), Some(caret)) = (panes.get(&workspace.active_pane), shell_caret) {
                shell_candidates(ctx, pane, caret, &mut actions);
            }
        }
        self.system.dialogs(ctx);
        self.mcp.show(ctx, &self.settings);
        if had_dialog && !self.overlay_open() {
            actions.push(Action::FocusCurrent);
        }
        if self.settings_dirty && !ctx.input(|input| input.pointer.any_down()) {
            self.settings_dirty = false;
            actions.push(Action::ApplySettings);
        }
        if let Some(notice) = self.notice.clone() {
            if self.notice_deadline.is_none() {
                self.notice_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
            }
            let deadline = self.notice_deadline.unwrap();
            if std::time::Instant::now() >= deadline {
                self.notice = None;
                self.notice_deadline = None;
            } else {
                egui::Area::new(egui::Id::new("action_feedback"))
                    .anchor(egui::Align2::RIGHT_BOTTOM, [-14.0, -14.0])
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        Frame::new()
                            .fill(Color32::from_black_alpha(225))
                            .inner_margin(Margin::same(12))
                            .show(ui, |ui| {
                                ui.set_max_width(520.0);
                                ui.label(notice);
                            });
                    });
                ctx.request_repaint_after(
                    deadline.saturating_duration_since(std::time::Instant::now()),
                );
            }
        } else {
            self.notice_deadline = None;
        }
        (layouts, actions)
    }
    fn system_contents(
        &mut self,
        ui: &mut egui::Ui,
        show_dock: bool,
        pane: Option<&TerminalPane>,
        actions: &mut Vec<Action>,
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("System").strong().size(16.0));
            if show_dock {
                ui.menu_button("Dock", |ui| {
                    for (dock, label) in [
                        (Dock::Left, "Left"),
                        (Dock::Right, "Right"),
                        (Dock::Bottom, "Bottom"),
                    ] {
                        if ui
                            .selectable_label(self.settings.dock == dock, label)
                            .clicked()
                        {
                            self.settings.dock = dock;
                            self.settings_dirty = true;
                            ui.close_menu();
                        }
                    }
                });
            }
            if ui.small_button("Collapse").clicked() {
                self.settings.system_open = false;
                self.settings_dirty = true;
            }
        });
        ui.add_space(6.0);
        self.system.show(ui);
        if self.system.page == Page::ShellValues {
            if let Some(pane) = pane {
                shell_values(ui, pane, actions);
            }
        }
    }
    fn dialogs(
        &mut self,
        ctx: &egui::Context,
        fonts: &[String],
        actions: &mut Vec<Action>,
        active_pane: Option<&TerminalPane>,
    ) {
        #[cfg(not(target_os = "macos"))]
        if self.settings_open {
            let mut page = self.settings_page;
            let close = settings_panel::show(ctx, &mut page, |ui, page| {
                self.draw_settings_page(ui, page, fonts, actions);
            });
            self.settings_page = page;
            self.settings_open = !close;
        }
        #[cfg(target_os = "macos")]
        if window_visible(ctx, "Settings", self.settings_open) {
            let mut open = self.settings_open;
            let max_width = (ctx.screen_rect().width() - 32.0).max(80.0);
            let window = egui::Window::new("Settings")
                .open(&mut open).default_width(480.0_f32.min(max_width))
                .max_width(max_width);
            #[cfg(target_os = "android")]
            let window = window.anchor(egui::Align2::CENTER_TOP, [0.0, 8.0])
                .resizable(false).collapsible(false);
            window
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().max_height((ctx.screen_rect().height() - 88.0).clamp(60.0, 650.0)).show(ui, |ui| {
                        ui.label(RichText::new("Terminal").strong());
                        self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.font_size,10.0..=28.0).text("Font size")).changed();
                        egui::ComboBox::from_label("Font").selected_text(&self.settings.font_family).show_ui(ui, |ui| {
                            for font in fonts {
                                self.settings_dirty |= ui.selectable_value(&mut self.settings.font_family,font.clone(),font).changed();
                            }
                        });
                        let gpu_label = match self.settings.gpu_power_preference {
                            GpuPowerPreference::Auto => "Auto",
                            GpuPowerPreference::LowPower => "Power saving",
                            GpuPowerPreference::HighPerformance => "High performance",
                        };
                        egui::ComboBox::from_label("GPU").selected_text(gpu_label).show_ui(ui, |ui| {
                            for (preference, label) in [
                                (GpuPowerPreference::Auto, "Auto"),
                                (GpuPowerPreference::LowPower, "Power saving"),
                                (GpuPowerPreference::HighPerformance, "High performance"),
                            ] {
                                self.settings_dirty |= ui.selectable_value(&mut self.settings.gpu_power_preference, preference, label).changed();
                            }
                        });
                        ui.small("Restart the app to apply the GPU selection.");
                        ui.separator();
                        ui.label(RichText::new("Shell").strong());
                        #[cfg(target_os = "android")]
                        let default_shell_label = if crate::android_arch::bundled() {
                            "Arch Linux (default)"
                        } else {
                            "System default"
                        };
                        #[cfg(not(target_os = "android"))]
                        let default_shell_label = "System default";
                        #[cfg(target_os = "android")]
                        let aether_available = crate::android_arch::aether_shell().is_ok()
                            && crate::android_arch::system_bridge().is_ok();
                        #[cfg(not(target_os = "android"))]
                        let aether_available = true;
                        let current_shell = match self.settings.shell.program.as_deref() {
                            None => default_shell_label,
                            Some("@aether") if !aether_available => "Aether Shell (unavailable)",
                            Some("@aether") => "Aether Shell",
                            #[cfg(target_os = "android")]
                            Some("/system/bin/sh") => "Android Shell",
                            Some(_) => "Custom executable",
                        };
                        egui::ComboBox::from_label("New sessions").selected_text(current_shell).show_ui(ui, |ui| {
                            if ui.selectable_label(self.settings.shell.program.is_none(), default_shell_label).clicked() {
                                self.settings.shell.program = None;
                                self.settings.shell.integration = session_manager::ShellIntegration::None;
                                self.settings.shell.login = None; self.settings.shell.args.clear(); self.settings_dirty = true;
                            }
                            #[cfg(target_os = "android")]
                            if crate::android_arch::bundled()
                                && ui.selectable_label(self.settings.shell.program.as_deref() == Some("/system/bin/sh"), "Android Shell").clicked()
                            {
                                self.settings.shell.program = Some("/system/bin/sh".into());
                                self.settings.shell.integration = session_manager::ShellIntegration::None;
                                self.settings.shell.login = Some(false); self.settings.shell.args.clear(); self.settings_dirty = true;
                            }
                            if aether_available && ui.selectable_label(self.settings.shell.program.as_deref() == Some("@aether"), "Aether Shell").clicked() {
                                self.settings.shell.program = Some("@aether".into());
                                self.settings.shell.integration = session_manager::ShellIntegration::AetherV1;
                                self.settings.shell.login = Some(false); self.settings.shell.args.clear(); self.settings_dirty = true;
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.shell_program_editor).hint_text("Absolute custom Shell executable"));
                            if ui.button("Use custom Shell").clicked() {
                                let candidate = crate::config::ShellProfile { program: Some(self.shell_program_editor.clone()), ..Default::default() };
                                match candidate.validate() {
                                    Ok(()) => {
                                        self.settings.shell.program = candidate.program;
                                        self.settings.shell.integration = session_manager::ShellIntegration::None;
                                        self.settings.shell.login = Some(false); self.settings.shell.args.clear(); self.settings_dirty = true;
                                    }
                                    Err(error) => self.notice = Some(error.to_string()),
                                }
                            }
                        });
                        if self.settings.shell.program.as_deref() == Some("@aether") {
                            if let Err(error) = self.settings.shell.resolved(None, None) { ui.colored_label(Color32::LIGHT_RED, error.to_string()); }
                            let mut native = self.settings.shell.native_presentation.unwrap_or(true);
                            if ui.checkbox(&mut native, "Native candidates and structured previews").changed() {
                                self.settings.shell.native_presentation = Some(native); self.settings_dirty = true;
                            }
                        }
                        #[cfg(target_os = "android")]
                        if crate::android_arch::available() && ui.button("Arch Linux").clicked() {
                            actions.push(Action::ArchLinux);
                        }
                        ui.separator();
                        ui.label(RichText::new("Glass material").strong());
                        ui.horizontal(|ui| for (effect,label) in [(Effect::Off,"Off"),(Effect::Balanced,"Balanced"),(Effect::Full,"Full")] {
                            self.settings_dirty |= ui.selectable_value(&mut self.settings.effect,effect,label).changed();
                        });
                        self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.opacity,0.0..=1.0).text("Background opacity"))
                            .on_hover_text("Lower values reveal more of the background. The system compositor controls native blur; text stays opaque.").changed();
                        #[cfg(windows)]
                        {
                            let mut blur = self.settings.appearance_ui.material_parameters.backdrop_blur_radius_px > 0.0;
                            if ui.checkbox(&mut blur, "Background blur (Acrylic)")
                                .on_hover_text("Windows controls the blur strength. Turn this off to keep a transparent background without blur. Background opacity is adjusted separately.").changed() {
                                self.settings.appearance_ui.material_parameters.backdrop_blur_radius_px = if blur { 24.0 } else { 0.0 };
                                self.settings_dirty = true;
                            }
                        }
                        egui::ComboBox::from_label("System material").selected_text(format!("{:?}", self.settings.material)).show_ui(ui, |ui| {
                            for material in [Material::LiquidClear,Material::LiquidRegular,Material::Hud,Material::Sidebar,Material::UnderWindowBackground] {
                                self.settings_dirty |= ui.selectable_value(&mut self.settings.material,material,format!("{material:?}")).changed();
                            }
                        });
                        self.settings_dirty |= ui.checkbox(&mut self.settings.reduce_motion,"Reduce motion").changed();
                        ui.small("System accessibility preferences take precedence. Blur uses the selected native material.");
                        self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.appearance_ui.window_corner_radius,0.0..=48.0).text("Window corners").suffix(" pt")).changed();
                        ui.separator();
                        ui.label(RichText::new("Glass panels").strong());
                        egui::ComboBox::from_label("Panel material").selected_text(format!("{:?}", self.settings.appearance_ui.panel_material)).show_ui(ui, |ui| {
                            for material in [PanelMaterial::LiquidClear,PanelMaterial::LiquidRegular] {
                                self.settings_dirty |= ui.selectable_value(&mut self.settings.appearance_ui.panel_material,material,format!("{material:?}")).changed();
                            }
                        });
                        self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.appearance_ui.panel_opacity,0.2..=1.0).text("Panel opacity")).changed();
                        self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.appearance_ui.panel_corner_radius,0..=48).text("Panel corners").suffix(" pt")).changed();
                        self.settings_dirty |= ui.checkbox(&mut self.settings.appearance_ui.animations,"Interaction animations").changed();
                        if self.settings.appearance_ui.animations {
                            self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.appearance_ui.animation_duration_ms,0..=500).text("Animation duration").suffix(" ms")).changed();
                        }
                        #[cfg(target_os = "macos")]
                        {
                            ui.separator();
                            egui::ComboBox::from_label("Quit shortcut")
                                .selected_text(match self.settings.quit_protection.mode {
                                    crate::config::QuitMode::Confirm => "Ask for confirmation",
                                    crate::config::QuitMode::Hold => "Hold to quit",
                                    crate::config::QuitMode::HoldOrDoublePress => "Hold or press twice",
                                })
                                .show_ui(ui, |ui| {
                                    for (mode, label) in [
                                        (crate::config::QuitMode::Confirm, "Ask for confirmation"),
                                        (crate::config::QuitMode::Hold, "Hold to quit"),
                                        (crate::config::QuitMode::HoldOrDoublePress, "Hold or press twice"),
                                    ] {
                                        self.settings_dirty |= ui.selectable_value(&mut self.settings.quit_protection.mode, mode, label).changed();
                                    }
                                });
                            if self.settings.quit_protection.mode != crate::config::QuitMode::Confirm {
                                self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.quit_protection.hold_duration_ms, 500..=3000).text("Hold duration").suffix(" ms")).changed();
                            }
                            if self.settings.quit_protection.mode == crate::config::QuitMode::HoldOrDoublePress {
                                self.settings_dirty |= ui.add(egui::Slider::new(&mut self.settings.quit_protection.double_press_interval_ms, 200..=2000).text("Double-press interval").suffix(" ms")).changed();
                            }
                            ui.separator();
                            self.settings_dirty |= ui.checkbox(&mut self.settings.administrator_persistent,"Keep administrator authorization across launches").changed();
                            ui.small("First use asks for system authentication. Revoke removes retained access.");
                        }
                        ui.separator();
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Reload configuration").clicked() { actions.push(Action::ReloadConfig); }
                            if ui.button("Edit config.json").clicked() {
                                match self.settings.editor_text() {
                                    Ok(text) => self.config_editor = Some(text),
                                    Err(error) => self.notice = Some(error.to_string()),
                                }
                            }
                        });
                        ui.small(Settings::path().display().to_string());
                        ui.small("Colors, ANSI palette, Shell profile, shortcuts and MCP servers share this configuration. Shell changes apply to new sessions. Layout is restored with fresh Shells at launch.");
                        if let Some(text) = &mut self.config_editor {
                            ui.add(egui::TextEdit::multiline(text).font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY).desired_rows(15)
                                .char_limit(crate::config::MAX_CONFIG_BYTES));
                            if ui.button("Validate, save and apply").clicked() { actions.push(Action::SaveConfig(text.clone())); }
                        }
                        ui.separator();
                        if ui.button("Save diagnostics").clicked() { actions.push(Action::SaveDiagnostics); }
                        ui.small(Settings::path().with_file_name("diagnostics.json").display().to_string());
                        if ui.button("Revoke administrator authorization").clicked() { self.system.revoke(); }
                    });
                });
            if !open {
                self.settings_open = false;
            }
        }
        if window_visible(ctx, "Commands", self.commands_open) {
            let mut open = self.commands_open;
            let mut chosen = None;
            let mut shell_operation = None;
            let mut chosen_values = false;
            egui::Window::new("Commands")
                .open(&mut open)
                .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
                .default_width(460.0)
                .max_width((ctx.screen_rect().width() - 32.0).max(80.0))
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    let submitted = query_text_edit(
                        ui,
                        "command_query",
                        &mut self.focus_command,
                        egui::TextEdit::singleline(&mut self.command)
                            .hint_text("Search commands")
                            .desired_width(f32::INFINITY)
                            .char_limit(128),
                    );
                    let filter = self.command.to_lowercase();
                    let commands = [
                        "New Shell",
                        "Split right",
                        "Split below",
                        "SSH connection",
                        "Processes",
                        "Services",
                        "Network",
                        "Devices",
                        "Logs",
                        "Timeline",
                        "X-Ray",
                        "Controlled recovery",
                        "Plugins",
                        "Audit history",
                        "Settings",
                        "Reload configuration",
                        "MCP servers",
                        #[cfg(target_os = "android")]
                        "Copy terminal selection",
                        #[cfg(target_os = "android")]
                        "Paste into terminal",
                    ];
                    egui::ScrollArea::vertical()
                        .max_height((ctx.screen_rect().height() - 160.0).clamp(60.0, 500.0))
                        .show(ui, |ui| {
                            #[cfg(target_os = "android")]
                            if crate::android_arch::available()
                                && "arch linux".contains(&filter)
                                && (ui.button("Arch Linux").clicked()
                                    || (submitted && !filter.is_empty()))
                            {
                                chosen = Some(commands.len());
                            }
                            #[cfg(target_os = "android")]
                            if "settings".contains(&filter)
                                && (ui.button("Settings").clicked()
                                    || (submitted && !filter.is_empty() && chosen.is_none()))
                            {
                                chosen = Some(14);
                            }
                            for (index, label) in commands
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| !cfg!(target_os = "android") || *index != 14)
                                .filter(|(_, label)| label.to_lowercase().contains(&filter))
                            {
                                if ui.button(*label).clicked() || (submitted && chosen.is_none()) {
                                    chosen = Some(index);
                                }
                            }
                            if let Some(pane) = active_pane {
                                let connected = pane
                                    .session
                                    .as_ref()
                                    .is_some_and(|session| session.has_shell_operations());
                                if let Some(session_id) =
                                    pane.shell_session_id.as_ref().filter(|_| connected)
                                {
                                    if "shell jobs".contains(&filter)
                                        && (ui.button("Shell jobs").clicked()
                                            || (submitted && chosen.is_none()))
                                    {
                                        shell_operation = Some(Action::ShellOperation {
                                            pane_id: pane.id,
                                            session_id: session_id.clone(),
                                            operation: "task.list".into(),
                                            args: Vec::new(),
                                        });
                                    }
                                    if "shell values objects".contains(&filter)
                                        && (ui.button("Shell values and objects").clicked()
                                            || (submitted
                                                && chosen.is_none()
                                                && shell_operation.is_none()))
                                    {
                                        chosen_values = true;
                                    }
                                    for task in pane.shell_tasks.values() {
                                        let label = format!(
                                            "Terminate task #{} · PGID {} · {} · {}",
                                            task.id, task.pgid, task.state, task.display
                                        );
                                        if label.to_lowercase().contains(&filter)
                                            && (ui.button(label).clicked()
                                                || (submitted
                                                    && chosen.is_none()
                                                    && shell_operation.is_none()))
                                        {
                                            shell_operation = Some(Action::ShellOperation {
                                                pane_id: pane.id,
                                                session_id: session_id.clone(),
                                                operation: "task.signal".into(),
                                                args: vec![task.id.to_string(), "TERM".into()],
                                            });
                                        }
                                    }
                                }
                            }
                        });
                });
            if chosen_values {
                self.commands_open = false;
                self.system_page(Page::ShellValues);
            }
            if let Some(operation) = shell_operation {
                self.commands_open = false;
                self.system_page(Page::Timeline);
                actions.push(operation);
            }
            if !open {
                self.commands_open = false;
            }
            if let Some(index) = chosen {
                self.commands_open = false;
                match index {
                    0 => actions.push(Action::NewShell),
                    1 => actions.push(Action::Split(Axis::Horizontal)),
                    2 => actions.push(Action::Split(Axis::Vertical)),
                    3 => self.ssh_open = true,
                    4 => self.system_page(Page::Processes),
                    5 => self.system_page(Page::Services),
                    6 => self.system_page(Page::Network),
                    7 => self.system_page(Page::Devices),
                    8 => self.system_page(Page::Logs),
                    9 => self.system_page(Page::Timeline),
                    10 => self.system_page(Page::Xray),
                    11 => self.system_page(Page::Recovery),
                    12 => self.system_page(Page::Plugins),
                    13 => self.system_page(Page::Audit),
                    14 => self.settings_open = true,
                    15 => actions.push(Action::ReloadConfig),
                    #[cfg(target_os = "android")]
                    17 => actions.push(Action::Copy),
                    #[cfg(target_os = "android")]
                    18 => actions.push(Action::Paste),
                    #[cfg(target_os = "android")]
                    19 => actions.push(Action::ArchLinux),
                    _ => self.mcp.open = true,
                }
            }
        }
        if window_visible(ctx, "SSH connection", self.ssh_open) {
            let mut open = self.ssh_open;
            let mut connect = false;
            egui::Window::new("SSH connection")
                .open(&mut open)
                .default_width(390.0)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Host or SSH config alias");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ssh_target)
                            .desired_width(f32::INFINITY)
                            .char_limit(255),
                    );
                    ui.label("User (optional)");
                    ui.add(egui::TextEdit::singleline(&mut self.ssh_user).char_limit(128));
                    ui.add(
                        egui::DragValue::new(&mut self.ssh_port)
                            .range(1..=65535)
                            .prefix("Port "),
                    );
                    connect = ui
                        .add_enabled(
                            !self.ssh_target.trim().is_empty(),
                            egui::Button::new("Connect"),
                        )
                        .clicked();
                    ui.small("Authentication and host-key prompts stay in the SSH terminal.");
                });
            if !open {
                self.ssh_open = false;
            }
            if connect {
                actions.push(Action::NewSsh {
                    target: self.ssh_target.trim().into(),
                    user: self.ssh_user.trim().into(),
                    port: self.ssh_port,
                });
                self.ssh_open = false;
            }
        }
    }
}

pub(crate) fn run_ui(
    context: &egui::Context,
    input: egui::RawInput,
    prepare_queries: bool,
    mut draw: impl FnMut(&egui::Context),
) -> egui::FullOutput {
    if !prepare_queries {
        return context.run(input, draw);
    }
    let mut layout_input = input.clone();
    layout_input.events.clear();
    let mut output = context.run(layout_input, &mut draw);
    output.append(context.run(input, draw));
    output
}

/// Draws a query field and reports an Enter submission from this frame.
fn query_text_edit(
    ui: &mut egui::Ui,
    id_salt: &'static str,
    request_focus: &mut bool,
    edit: egui::TextEdit<'_>,
) -> bool {
    let id = ui.make_persistent_id(id_salt);
    if ui.is_sizing_pass() {
        ui.add(edit.id(id));
        return false;
    }
    if std::mem::take(request_focus) {
        ui.memory_mut(|memory| memory.request_focus(id));
    }
    // A new field can gain focus and surrender it to Enter in the same frame.
    let focused_before_edit = ui.memory(|memory| memory.has_focus(id));
    let response = ui.add(edit.id(id));
    (focused_before_edit || response.has_focus() || response.lost_focus())
        && ui.input(|input| input.key_pressed(egui::Key::Enter))
}

pub(crate) fn terminal_route(focused: bool, window_focused: bool, modal_open: bool) -> bool {
    focused && window_focused && !modal_open
}
pub(crate) fn terminal_pointer_route(hit: bool, order: Option<egui::Order>, modal: bool) -> bool {
    hit && !modal && order.is_none_or(|order| order == egui::Order::Background)
}

fn ui_fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Geist".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/ui-fonts/Geist.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "Geist".into());
    fonts
}

pub fn install_system_fallback(ctx: &egui::Context, family: String, bytes: Vec<u8>, index: u32) {
    let mut fonts = ui_fonts();
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    fonts
        .font_data
        .insert(family.clone(), std::sync::Arc::new(data));
    for font_family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(font_family)
            .or_default()
            .push(family.clone());
    }
    ctx.set_fonts(fonts);
}

pub fn style(ctx: &egui::Context) {
    ctx.set_fonts(ui_fonts());
    apply_style(ctx, &Settings::default());
}

#[derive(Clone, PartialEq)]
struct AppliedStyle {
    appearance: AppearanceUi,
    reduce_motion: bool,
    reduce_transparency: bool,
}

pub fn apply_style(ctx: &egui::Context, settings: &Settings) {
    let preferences = terminal_render::accessibility_preferences();
    let applied = AppliedStyle {
        appearance: settings.appearance_ui.clone(),
        reduce_motion: settings.reduce_motion || preferences.reduce_motion,
        reduce_transparency: preferences.reduce_transparency,
    };
    let id = egui::Id::new("glass_ui_style");
    if ctx.data(|data| data.get_temp::<AppliedStyle>(id)).as_ref() == Some(&applied) {
        return;
    }
    let appearance = &applied.appearance;
    let opacity = if applied.reduce_transparency {
        1.0
    } else {
        appearance.panel_opacity
    };
    let tint = match appearance.panel_material {
        PanelMaterial::LiquidClear => 20,
        PanelMaterial::LiquidRegular => 32,
    };
    let fill = Color32::from_rgba_unmultiplied(tint, tint, tint, (opacity * 255.0).round() as u8);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = fill;
    visuals.window_fill = fill;
    visuals.window_corner_radius = egui::CornerRadius::same(appearance.panel_corner_radius);
    visuals.menu_corner_radius = visuals.window_corner_radius;
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 1,
        color: Color32::from_black_alpha(88),
    };
    visuals.popup_shadow = visuals.window_shadow;
    visuals.override_text_color = Some(Color32::from_gray(225));
    visuals.selection.bg_fill = Color32::from_gray(84);
    visuals.text_cursor.blink = false;
    visuals.text_cursor.stroke.color = Color32::from_gray(225);
    visuals.widgets.inactive.bg_fill = Color32::from_white_alpha(20);
    visuals.widgets.inactive.weak_bg_fill = visuals.widgets.inactive.bg_fill;
    visuals.widgets.inactive.bg_stroke = Stroke::new(0.75_f32, Color32::from_white_alpha(26));
    visuals.widgets.hovered.bg_fill = Color32::from_white_alpha(36);
    visuals.widgets.hovered.weak_bg_fill = visuals.widgets.hovered.bg_fill;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(65));
    visuals.widgets.active.bg_fill = Color32::from_white_alpha(52);
    visuals.widgets.active.weak_bg_fill = visuals.widgets.active.bg_fill;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(90));
    let widget_radius = egui::CornerRadius::same(appearance.panel_corner_radius.min(6));
    visuals.widgets.noninteractive.corner_radius = widget_radius;
    visuals.widgets.inactive.corner_radius = widget_radius;
    visuals.widgets.hovered.corner_radius = widget_radius;
    visuals.widgets.active.corner_radius = widget_radius;
    visuals.widgets.open.corner_radius = widget_radius;
    visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(65));
    visuals.code_bg_color = Color32::from_black_alpha(100);
    visuals.extreme_bg_color = Color32::from_black_alpha(115);
    visuals.faint_bg_color = Color32::from_white_alpha(10);
    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    style.animation_time = if appearance.animations && !applied.reduce_motion {
        f32::from(appearance.animation_duration_ms) / 1000.0
    } else {
        0.0
    };
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(9.0, 5.0);
    ctx.set_style(style);
    ctx.data_mut(|data| data.insert_temp(id, applied));
}

fn window_visible(ctx: &egui::Context, title: &str, open: bool) -> bool {
    let opacity = ctx.animate_bool_with_easing(
        egui::Id::new(("glass_window_visibility", title)),
        open,
        egui::emath::easing::cubic_out,
    );
    open || opacity > 0.0
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_drag_strip_excludes_resize_caption_and_terminal_rows() {
        let size = egui::vec2(1000.0, 680.0);
        let (area, grip) = super::windows_drag_strip_geometry(size).unwrap();
        assert_eq!(area.min, egui::pos2(6.0, 6.0));
        assert_eq!(area.max, egui::pos2(840.0, 14.0));
        assert_eq!(grip.size(), egui::vec2(56.0, 2.0));
        assert!(super::windows_drag_strip_hit(egui::pos2(500.0, 10.0), size));
        for point in [
            egui::pos2(500.0, 5.0),
            egui::pos2(5.0, 10.0),
            egui::pos2(840.0, 10.0),
            egui::pos2(500.0, 14.0),
            egui::pos2(f32::NAN, 10.0),
        ] {
            assert!(!super::windows_drag_strip_hit(point, size));
        }
        assert!(super::windows_drag_strip_geometry(egui::vec2(220.0, 680.0)).is_none());
        assert!(super::windows_drag_strip_geometry(egui::vec2(f32::INFINITY, 680.0)).is_none());
    }

    use super::terminal_route;
    fn paste_and_enter(pasted: &str) -> Vec<egui::Event> {
        vec![
            egui::Event::Paste(pasted.to_owned()),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn query_window_frame(
        context: &egui::Context,
        id_salt: &'static str,
        text: &mut String,
        request_focus: &mut bool,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, bool) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 820.0),
            )),
            events,
            ..Default::default()
        };
        let prepare_queries = *request_focus;
        let mut submitted = false;
        let output = super::run_ui(context, input, prepare_queries, |ctx| {
            egui::CentralPanel::default().show(ctx, |_| {});
            let mut open = true;
            egui::Window::new(id_salt)
                .open(&mut open)
                .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
                .default_width(460.0)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    submitted |= super::query_text_edit(
                        ui,
                        id_salt,
                        request_focus,
                        egui::TextEdit::singleline(text)
                            .desired_width(f32::INFINITY)
                            .char_limit(1024),
                    );
                });
        });
        (output, submitted)
    }

    #[test]
    fn new_command_query_accepts_paste_and_enter_in_its_first_frame() {
        let context = egui::Context::default();
        let mut query = String::new();
        let mut request_focus = true;
        let (output, submitted) = query_window_frame(
            &context,
            "command_query",
            &mut query,
            &mut request_focus,
            paste_and_enter("Processes"),
        );
        assert_eq!(query, "Processes");
        assert!(submitted);
        assert!(!request_focus);
        assert!(!output.textures_delta.set.is_empty());
    }

    #[test]
    fn new_search_query_accepts_paste_and_enter_in_its_first_frame() {
        let context = egui::Context::default();
        let mut query = String::new();
        let mut request_focus = true;
        let (_, submitted) = query_window_frame(
            &context,
            "search_query",
            &mut query,
            &mut request_focus,
            paste_and_enter("FOCUS_OK"),
        );
        assert_eq!(query, "FOCUS_OK");
        assert!(submitted);
        assert!(!request_focus);
    }

    #[test]
    fn query_input_after_empty_first_window_frame_keeps_focus() {
        for id_salt in ["command_query", "search_query"] {
            let context = egui::Context::default();
            let mut query = String::new();
            let mut request_focus = true;
            let (_, submitted) = query_window_frame(
                &context,
                id_salt,
                &mut query,
                &mut request_focus,
                Vec::new(),
            );
            assert!(!submitted);
            assert!(!request_focus);
            assert!(context.memory(|memory| memory.focused().is_some()));
            let (_, submitted) = query_window_frame(
                &context,
                id_salt,
                &mut query,
                &mut request_focus,
                paste_and_enter("Processes"),
            );
            assert_eq!(query, "Processes");
            assert!(submitted);
        }
    }

    #[test]
    fn android_fresh_ime_transaction_replaces_ctrl_a_selection_without_losing_first_character() {
        for fresh_transaction in [false, true] {
            let context = egui::Context::default();
            let mut text = "326400.".to_owned();
            let mut request_focus = true;
            query_window_frame(
                &context,
                "android_ime_selection",
                &mut text,
                &mut request_focus,
                vec![egui::Event::Ime(egui::ImeEvent::Enabled)],
            );
            let mut events = vec![egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    command: true,
                    ..Default::default()
                },
            }];
            if fresh_transaction {
                events.extend([
                    egui::Event::Ime(egui::ImeEvent::Disabled),
                    egui::Event::Ime(egui::ImeEvent::Enabled),
                ]);
            }
            events.extend([
                egui::Event::Ime(egui::ImeEvent::Commit("3".into())),
                egui::Event::Ime(egui::ImeEvent::Disabled),
            ]);
            query_window_frame(
                &context,
                "android_ime_selection",
                &mut text,
                &mut request_focus,
                events,
            );
            assert_eq!(text, if fresh_transaction { "3" } else { "326400." });
            for character in "26400.txt".chars() {
                query_window_frame(
                    &context,
                    "android_ime_selection",
                    &mut text,
                    &mut request_focus,
                    vec![
                        egui::Event::Ime(egui::ImeEvent::Enabled),
                        egui::Event::Ime(egui::ImeEvent::Commit(character.to_string())),
                        egui::Event::Ime(egui::ImeEvent::Disabled),
                    ],
                );
            }
            assert_eq!(
                text,
                if fresh_transaction {
                    "326400.txt"
                } else {
                    "326400.26400.txt"
                }
            );
        }
    }

    #[test]
    fn query_layout_keeps_texture_updates_from_both_runs() {
        let context = egui::Context::default();
        let mut textures = Vec::new();
        let mut actions = Vec::new();
        let output = super::run_ui(&context, egui::RawInput::default(), true, |ctx| {
            let pass = textures.len();
            textures.push(ctx.load_texture(
                format!("query-layout-{pass}"),
                egui::ColorImage::new([2, 2], egui::Color32::WHITE),
                egui::TextureOptions::NEAREST,
            ));
            actions.push(pass);
        });
        assert_eq!(actions, [0, 1]);
        for texture in textures {
            assert!(output
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture.id()));
        }
    }
    #[test]
    fn overlay_transitions_settle_and_reduce_motion_disables_animation() {
        let context = egui::Context::default();
        let mut settings = crate::config::Settings::default();
        super::apply_style(&context, &settings);
        let first_style = context.style();
        super::apply_style(&context, &settings);
        assert!(std::sync::Arc::ptr_eq(&first_style, &context.style()));
        context.style_mut(|style| style.animation_time = 0.16);
        let mut final_visible = true;
        let mut final_delay = std::time::Duration::ZERO;
        for frame in 0..80 {
            let open = (1..14).contains(&frame);
            let output = context.run(
                egui::RawInput {
                    time: Some(f64::from(frame) / 60.0),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(640.0, 480.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    final_visible = super::window_visible(ctx, "finite_transition", open);
                    if final_visible {
                        let mut open = open;
                        egui::Window::new("finite_transition")
                            .open(&mut open)
                            .show(ctx, |ui| {
                                ui.label("Glass");
                            });
                    }
                },
            );
            if frame == 15 {
                assert!(
                    final_visible,
                    "close transition must retain its final frames"
                );
            }
            final_delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        assert!(!final_visible);
        assert_eq!(final_delay, std::time::Duration::MAX);
        settings.reduce_motion = true;
        super::apply_style(&context, &settings);
        assert_eq!(context.style().animation_time, 0.0);
        assert!(!context.style().visuals.text_cursor.blink);
    }
    #[test]
    fn dialog_and_window_focus_gate_terminal_input() {
        assert!(terminal_route(true, true, false));
        assert!(!terminal_route(false, true, false));
        assert!(!terminal_route(true, false, false));
        assert!(!terminal_route(true, true, true));
        assert!(!terminal_route(false, false, true));
    }
    #[test]
    fn terminal_canvas_hits_precede_background_consumption_but_respect_overlays() {
        use egui::Order;
        assert!(super::terminal_pointer_route(
            true,
            Some(Order::Background),
            false
        ));
        assert!(!super::terminal_pointer_route(
            true,
            Some(Order::Middle),
            false
        ));
        assert!(!super::terminal_pointer_route(
            true,
            Some(Order::Foreground),
            false
        ));
        assert!(!super::terminal_pointer_route(
            false,
            Some(Order::Background),
            false
        ));
        assert!(!super::terminal_pointer_route(
            true,
            Some(Order::Background),
            true
        ));
    }
}

#[cfg(test)]
mod android_input_regressions {
    use super::{cancel_android_pointer, AndroidMotionQueue};

    fn button(pressed: bool, x: f32) -> egui::Event {
        egui::Event::PointerButton {
            pos: egui::pos2(x, 30.0),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    fn frame(context: &egui::Context, events: Vec<egui::Event>) -> [bool; 2] {
        let mut clicked = [false; 2];
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(300.0, 200.0),
                )),
                events,
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    for (index, x) in [10.0, 110.0].into_iter().enumerate() {
                        clicked[index] = ui
                            .allocate_rect(
                                egui::Rect::from_min_size(
                                    egui::pos2(x, 10.0),
                                    egui::vec2(80.0, 40.0),
                                ),
                                egui::Sense::click_and_drag(),
                            )
                            .clicked();
                    }
                });
            },
        );
        clicked
    }

    #[test]
    fn android_stale_motion_wake_cannot_take_a_new_gesture() {
        let mut queue = AndroidMotionQueue::default();
        let old = queue.push(1, |_, next| next).unwrap();
        queue.clear(); // End of the old gesture.
        queue.clear(); // Start of the next gesture.
        let current = queue.push(2, |_, next| next).unwrap();
        assert!(queue.push(3, |previous, next| previous + next).is_none());
        assert_eq!(queue.take(&old), None);
        assert_eq!(queue.take(&current), Some(5));
        let later_batch = queue.push(7, |_, next| next).unwrap();
        assert_eq!(queue.take(&current), None);
        assert_eq!(queue.take(&later_batch), Some(7));
    }

    #[test]
    fn android_cancel_resets_processed_pointer_and_drag_without_a_click() {
        let context = egui::Context::default();
        frame(&context, vec![]);
        frame(
            &context,
            vec![
                egui::Event::PointerMoved(egui::pos2(30.0, 30.0)),
                button(true, 30.0),
            ],
        );
        context.set_dragged_id(egui::Id::new("android-cancel-owned-drag"));
        assert!(context.input(|input| input.pointer.primary_down()));
        let mut pending = vec![
            egui::Event::PointerMoved(egui::pos2(70.0, 30.0)),
            egui::Event::Text("保留".into()),
        ];
        cancel_android_pointer(&context, &mut pending);
        cancel_android_pointer(&context, &mut pending);
        assert!(!context.input(|input| input.pointer.any_down()));
        assert_eq!(context.dragged_id(), None);
        assert_eq!(frame(&context, pending), [false, false]);
        assert!(!context.input(|input| input.pointer.any_click() || input.pointer.any_down()));
        assert!(context.input(|input| input.events.contains(&egui::Event::Text("保留".into()))));
        assert_eq!(
            frame(
                &context,
                vec![
                    egui::Event::PointerMoved(egui::pos2(130.0, 30.0)),
                    button(true, 130.0),
                    button(false, 130.0),
                ]
            ),
            [false, true]
        );
        frame(
            &context,
            vec![
                egui::Event::PointerMoved(egui::pos2(30.0, 30.0)),
                button(true, 30.0),
            ],
        );
        let mut pending = Vec::new();
        cancel_android_pointer(&context, &mut pending);
        pending.extend([
            egui::Event::PointerMoved(egui::pos2(130.0, 30.0)),
            button(true, 130.0),
            button(false, 130.0),
        ]);
        assert_eq!(frame(&context, pending), [false, true]);
    }

    #[test]
    fn android_cancel_keeps_completed_clicks_and_keyboard_before_a_new_press() {
        for previous_frame_press in [false, true] {
            let context = egui::Context::default();
            frame(&context, vec![]);
            let press = vec![
                egui::Event::PointerMoved(egui::pos2(30.0, 30.0)),
                button(true, 30.0),
            ];
            let mut pending = if previous_frame_press {
                frame(&context, press);
                Vec::new()
            } else {
                press
            };
            pending.extend([
                button(false, 30.0),
                egui::Event::Text("完成后输入".into()),
                egui::Event::PointerMoved(egui::pos2(130.0, 30.0)),
                button(true, 130.0),
                egui::Event::PointerMoved(egui::pos2(160.0, 30.0)),
            ]);
            cancel_android_pointer(&context, &mut pending);
            assert!(!pending.iter().any(|event| event == &button(true, 130.0)));
            assert_eq!(frame(&context, pending), [true, false]);
            assert!(!context.input(|input| input.pointer.any_down()));
            assert!(context.input(|input| input
                .events
                .contains(&egui::Event::Text("完成后输入".into()))));
        }
    }
}

fn completion_popup_anchor(
    caret: egui::Rect,
    bounds: egui::Rect,
    desired_height: f32,
) -> (egui::Pos2, egui::Align2, f32) {
    let top = caret.top().clamp(bounds.top(), bounds.bottom());
    let bottom = caret.bottom().clamp(top, bounds.bottom());
    let below = (bounds.bottom() - bottom - 4.0).max(0.0);
    let above = (top - bounds.top() - 4.0).max(0.0);
    let x = caret.left().clamp(bounds.left(), bounds.right());
    if below >= desired_height || below >= above {
        (
            egui::pos2(x, (bottom + 4.0).min(bounds.bottom())),
            egui::Align2::LEFT_TOP,
            below,
        )
    } else {
        (
            egui::pos2(x, (top - 4.0).max(bounds.top())),
            egui::Align2::LEFT_BOTTOM,
            above,
        )
    }
}

fn shell_candidates(
    ctx: &egui::Context,
    pane: &TerminalPane,
    caret: [f32; 4],
    actions: &mut Vec<Action>,
) {
    let (Some(snapshot), Some(session_id)) = (&pane.shell_completion, &pane.shell_session_id)
    else {
        return;
    };
    let Some(candidates) = snapshot["candidates"]
        .as_array()
        .filter(|items| !items.is_empty())
    else {
        return;
    };
    let scale = ctx.pixels_per_point();
    let bounds = egui::Rect::from_min_size(
        egui::pos2(pane.rect[0] / scale, pane.rect[1] / scale),
        egui::vec2(pane.rect[2] / scale, pane.rect[3] / scale),
    )
    .intersect(ctx.screen_rect())
    .shrink(4.0);
    if !bounds.is_positive() {
        return;
    }
    let caret = egui::Rect::from_min_size(
        egui::pos2(caret[0] / scale, caret[1] / scale),
        egui::vec2(caret[2] / scale, caret[3] / scale),
    );
    let style = ctx.style();
    let spacing = &style.spacing;
    let desired_height =
        (candidates.len() as f32 * (spacing.interact_size.y + spacing.item_spacing.y) + 16.0)
            .min(240.0);
    let (position, pivot, available_height) =
        completion_popup_anchor(caret, bounds, desired_height);
    egui::Window::new("Shell completion")
        .id(egui::Id::new(("shell_completion", pane.id)))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .pivot(pivot)
        .fixed_pos(position)
        .constrain_to(bounds)
        .min_size(egui::Vec2::ZERO)
        .max_size(egui::vec2(
            bounds.width().min(620.0),
            available_height.max(1.0),
        ))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .max_height((available_height - 16.0).clamp(1.0, 220.0))
                .show(ui, |ui| {
                    for candidate in candidates {
                        let label = candidate["label"].as_str().unwrap_or_default();
                        let selected = snapshot["selected"] == candidate["id"];
                        let label: String = label
                            .chars()
                            .filter(|ch| !ch.is_control())
                            .take(240)
                            .collect();
                        let response = ui.add_enabled(
                            pane.preedit.is_empty(),
                            egui::Button::new(label).selected(selected),
                        );
                        if response.clicked() {
                            actions.push(Action::ShellOperation {
                                pane_id: pane.id,
                                session_id: session_id.clone(),
                                operation: "editor.completion.accept".into(),
                                args: vec![
                                    snapshot["revision"].to_string(),
                                    snapshot["generation"].to_string(),
                                    candidate["id"].to_string(),
                                ],
                            });
                            actions.push(Action::FocusCurrent);
                        }
                        if let Some(description) =
                            candidate["description"].as_str().filter(|description| {
                                !description.trim().is_empty() && description.trim() != "command"
                            })
                        {
                            response.on_hover_text(description);
                        }
                    }
                });
        });
}

fn shell_values(ui: &mut egui::Ui, pane: &TerminalPane, actions: &mut Vec<Action>) {
    if pane.shell_values.is_empty() {
        ui.label("Run a structured pipeline with display to inspect its values.");
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for event in pane.shell_values.iter().rev() {
            let value = &event["value"];
            let title = value["name"]
                .as_str()
                .or(value["label"].as_str())
                .or(value["object_type"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Task {} · row {}", event["task_id"], event["row"]));
            egui::CollapsingHeader::new(title)
                .id_salt((
                    pane.id,
                    event["task_id"].to_string(),
                    event["producer_pid"].to_string(),
                    event["row"].to_string(),
                ))
                .default_open(true)
                .show(ui, |ui| {
                    if event["truncated"] == true {
                        ui.small("Preview truncated");
                    }
                    if event["dropped"].as_u64().unwrap_or(0) > 0 {
                        ui.small(format!(
                            "{} earlier preview records dropped",
                            event["dropped"]
                        ));
                    }
                    let system_object = value["kind"] == "system.object";
                    if let Some(fields) = value.as_object() {
                        let visible_fields: Vec<_> = if system_object {
                            [
                                "pid",
                                "cpu_percent",
                                "rss_bytes",
                                "state",
                                "status",
                                "last_exit",
                                "device_kind",
                            ]
                            .into_iter()
                            .filter_map(|name| fields.get(name).map(|field| (name, field)))
                            .collect()
                        } else {
                            fields
                                .iter()
                                .filter(|(name, _)| name.as_str() != "operations")
                                .map(|(name, field)| (name.as_str(), field))
                                .collect()
                        };
                        let field_width = (ui.available_width() - 88.0).max(40.0);
                        egui::Grid::new((
                            "shell_fields",
                            pane.id,
                            event["task_id"].to_string(),
                            event["producer_pid"].to_string(),
                            event["row"].to_string(),
                        ))
                        .num_columns(2)
                        .spacing(egui::vec2(12.0, 4.0))
                        .min_col_width(0.0)
                        .max_col_width(field_width)
                        .show(ui, |ui| {
                            for (name, field) in visible_fields {
                                let label = match name {
                                    "pid" => "PID",
                                    "cpu_percent" => "CPU",
                                    "rss_bytes" => "RSS",
                                    _ => name,
                                };
                                let text = match name {
                                    "cpu_percent" if system_object && field.is_number() => {
                                        format!("{:.1}%", field.as_f64().unwrap_or_default())
                                    }
                                    "rss_bytes" if system_object && field.is_u64() => {
                                        format!(
                                            "{:.1} MiB",
                                            field.as_u64().unwrap_or_default() as f64 / 1048576.0
                                        )
                                    }
                                    _ => field
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| field.to_string()),
                                };
                                ui.label(label);
                                ui.add(
                                    egui::Label::new(RichText::new(text).monospace())
                                        .selectable(true)
                                        .wrap(),
                                );
                                ui.end_row();
                            }
                        });
                    } else {
                        let text = value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string());
                        ui.add(
                            egui::Label::new(RichText::new(text).monospace())
                                .selectable(true)
                                .wrap(),
                        );
                    }
                    if let (Some(operations), Some(session_id)) =
                        (value["operations"].as_array(), &pane.shell_session_id)
                    {
                        ui.horizontal_wrapped(|ui| {
                            for operation in operations {
                                let Some(name) = operation["name"].as_str().filter(|name| {
                                    ["process.stop", "service.restart"].contains(name)
                                }) else {
                                    continue;
                                };
                                let label = operation["summary"].as_str().unwrap_or(name);
                                if ui.button(label).clicked() {
                                    actions.push(Action::PrepareShellObject {
                                        pane_id: pane.id,
                                        session_id: session_id.clone(),
                                        name: name.into(),
                                        object: value.clone(),
                                    });
                                }
                            }
                        });
                    }
                    if system_object {
                        egui::CollapsingHeader::new("Details").show(ui, |ui| {
                            let text = serde_json::to_string_pretty(value)
                                .unwrap_or_else(|_| value.to_string());
                            ui.add(
                                egui::Label::new(RichText::new(text).monospace())
                                    .selectable(true)
                                    .wrap(),
                            );
                        });
                    }
                });
        }
    });
}

#[cfg(test)]
mod completion_popup_tests {
    use super::completion_popup_anchor;
    use egui::{pos2, vec2, Align2, Rect};

    #[test]
    fn completion_popup_tracks_caret_and_available_pane_space() {
        let bounds = Rect::from_min_max(pos2(100.0, 40.0), pos2(500.0, 400.0));
        let top = Rect::from_min_size(pos2(180.0, 60.0), vec2(10.0, 20.0));
        let (position, pivot, height) = completion_popup_anchor(top, bounds, 220.0);
        assert_eq!(position, pos2(180.0, 84.0));
        assert_eq!(pivot, Align2::LEFT_TOP);
        assert_eq!(height, 316.0);
        let bottom = top.translate(vec2(290.0, 290.0));
        let (position, pivot, height) = completion_popup_anchor(bottom, bounds, 220.0);
        assert_eq!(position, pos2(470.0, 346.0));
        assert_eq!(pivot, Align2::LEFT_BOTTOM);
        assert_eq!(height, 306.0);
        let smaller = Rect::from_min_max(pos2(120.0, 80.0), pos2(260.0, 150.0));
        let (position, _, height) = completion_popup_anchor(bottom, smaller, 220.0);
        assert!(smaller.contains(position));
        assert!(height >= 0.0 && height <= smaller.height());
        assert_eq!(position.x, smaller.right());
    }
}

// FVPROV1 84222 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDZlMTI4NWM0YjFmNGE1ZjI4MDdhOWI0N2Q5Yjk5ZmE2NzM2ODNmMWNhY2FiYzYxMmI5M2ZmZTc3OTNkM2FkYiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOTc5M2U0NzVmNmRmMjM1NDE0YjE3OTdlNjc3YzJjMjM0N2NmYWRkYmQzY2MyNWI0Y2FhMDg4NDAzNTRjMWQ0M2UwYTY5Y2Q1YjcyZGNjODI2MjI3ZWZkYzQ3NDkxNDNjY2Y1ZWJlMDIyMzNmOGJmZDliODE4NDEyMzZhYmFkMGIifV19XQ== FVPROV1-END
