// SPDX-License-Identifier: GPL-3.0-only
//! InputConnection and pointer events supplied by the Android Activity.
use super::*;
use std::sync::{LazyLock, Mutex};
use winit::event::DeviceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

#[derive(Clone, Debug)]
pub enum AndroidInput {
    Commit(String),
    Preedit(String),
    Key {
        name: String,
        modifiers: u8,
    },
    ImeEnabled,
    ImeDisabled,
    Viewport {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    Pointer {
        phase: PointerPhase,
        x: f64,
        y: f64,
    },
    Scroll {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Copy,
    Cut,
    Paste(String),
    SelectAll,
    ArchImportFinished {
        bundled: bool,
        result: std::result::Result<(), String>,
    },
    MotionQueued(Arc<()>),
}

pub enum PlatformAction {
    CopyText(String),
    RequestPaste,
    Keyboard(bool),
    OpenUrl(String),
    OpenFile { name: String, contents: Vec<u8> },
    PickArchArchive,
    PrepareBundledArch,
}

type PlatformHandler = Arc<dyn Fn(PlatformAction) -> std::result::Result<(), String> + Send + Sync>;
static PLATFORM_HANDLER: Mutex<Option<PlatformHandler>> = Mutex::new(None);
static KEYBOARD_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INPUT_DIAGNOSTICS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_input_diagnostics(enabled: bool) {
    INPUT_DIAGNOSTICS.store(
        enabled && cfg!(debug_assertions),
        std::sync::atomic::Ordering::Release,
    );
}
static PENDING_MOTION: LazyLock<Mutex<ui::AndroidMotionQueue<AndroidInput>>> =
    LazyLock::new(|| Mutex::new(ui::AndroidMotionQueue::default()));

pub fn send_input(proxy: &EventLoopProxy<Wake>, input: AndroidInput) -> bool {
    if matches!(
        &input,
        AndroidInput::Pointer {
            phase: PointerPhase::Moved,
            ..
        } | AndroidInput::Scroll { .. }
    ) {
        let Ok(mut pending) = PENDING_MOTION.lock() else {
            return false;
        };
        let wake = pending.push(input, |previous, input| match (previous, input) {
            (
                AndroidInput::Scroll {
                    dx: old_dx,
                    dy: old_dy,
                    ..
                },
                AndroidInput::Scroll { x, y, dx, dy },
            ) => AndroidInput::Scroll {
                x,
                y,
                dx: (old_dx + dx).clamp(-65536.0, 65536.0),
                dy: (old_dy + dy).clamp(-65536.0, 65536.0),
            },
            (_, input) => input,
        });
        if let Some(token) = wake {
            if proxy
                .send_event(Wake::AndroidInput(AndroidInput::MotionQueued(token)))
                .is_err()
            {
                pending.clear();
                return false;
            }
        }
        return true;
    }
    if matches!(
        &input,
        AndroidInput::Pointer { .. } | AndroidInput::ImeDisabled
    ) {
        let Ok(mut pending) = PENDING_MOTION.lock() else {
            return false;
        };
        pending.clear();
        return proxy.send_event(Wake::AndroidInput(input)).is_ok();
    }
    proxy.send_event(Wake::AndroidInput(input)).is_ok()
}

pub fn install_platform_handler(handler: PlatformHandler) {
    *PLATFORM_HANDLER
        .lock()
        .expect("Android platform handler lock") = Some(handler);
}

fn platform_action(action: PlatformAction) -> Result<()> {
    let handler = PLATFORM_HANDLER
        .lock()
        .map_err(|_| anyhow::anyhow!("Android platform handler lock unavailable"))?
        .clone()
        .context("Android platform callbacks are unavailable")?;
    handler(action).map_err(anyhow::Error::msg)
}

pub(crate) fn copy_text(text: String) -> Result<()> {
    platform_action(PlatformAction::CopyText(text))
}

pub(crate) fn request_paste() -> Result<()> {
    platform_action(PlatformAction::RequestPaste)
}

pub(crate) fn pick_arch_archive() -> Result<()> {
    platform_action(PlatformAction::PickArchArchive)
}

pub fn configure_arch_bootstrap(bundled: bool, api_name: String) {
    crate::android_arch::configure(bundled, api_name);
}

pub(crate) fn prepare_bundled_arch() -> Result<()> {
    platform_action(PlatformAction::PrepareBundledArch)
}

pub(crate) fn open_url(url: &str) -> Result<()> {
    anyhow::ensure!(url.len() <= 8192, "Android URL exceeds its size limit");
    platform_action(PlatformAction::OpenUrl(url.into()))
}

pub(crate) fn open_file(path: &std::path::Path) -> Result<()> {
    let root = system_api::default_data_dir()?.join("managed");
    anyhow::ensure!(
        path.parent() == Some(root.as_path()),
        "Android file opening is limited to the managed directory"
    );
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("Android managed file name is not UTF-8")?;
    // RecoveryStore reads through its pinned directory descriptor with NOFOLLOW,
    // single-link/regular-file checks and the existing 1 MiB text limit.
    let contents = system_api::RecoveryStore::new(root)?
        .read(name)?
        .into_bytes();
    platform_action(PlatformAction::OpenFile {
        name: name.into(),
        contents,
    })
}

pub(crate) fn session_environment(
    mut environment: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    environment.entry("HOME".into()).or_insert(
        system_api::android_root::files_dir()?
            .to_str()
            .context("Android private home is not UTF-8")?
            .to_owned(),
    );
    Ok(environment)
}

#[allow(deprecated)]
pub(crate) fn platform_output(output: &egui::PlatformOutput) {
    let mut copied = false;
    for command in &output.commands {
        if let egui::OutputCommand::CopyText(text) = command {
            copied = true;
            if let Err(error) = platform_action(PlatformAction::CopyText(text.clone())) {
                eprintln!("Android clipboard: {error}");
            }
        }
    }
    if !copied && !output.copied_text.is_empty() {
        if let Err(error) = platform_action(PlatformAction::CopyText(output.copied_text.clone())) {
            eprintln!("Android clipboard: {error}");
        }
    }
    let visible = output.ime.is_some();
    if KEYBOARD_VISIBLE.swap(visible, std::sync::atomic::Ordering::AcqRel) != visible {
        if let Err(error) = platform_action(PlatformAction::Keyboard(visible)) {
            eprintln!("Android keyboard: {error}");
        }
    }
}

fn ui_key(name: &str) -> Option<egui::Key> {
    match name {
        "Up" => Some(egui::Key::ArrowUp),
        "Down" => Some(egui::Key::ArrowDown),
        "Left" => Some(egui::Key::ArrowLeft),
        "Right" => Some(egui::Key::ArrowRight),
        _ => egui::Key::from_name(name).or_else(|| egui::Key::from_name(&name.to_uppercase())),
    }
}

impl App {
    fn android_ui_event(&mut self, event: egui::Event) {
        if let Some(state) = &mut self.gui_state {
            state.egui_input_mut().events.push(event);
            self.redraw();
        }
    }

    pub(crate) fn android_input(
        &mut self,
        event_loop: &ActiveEventLoop,
        input: AndroidInput,
    ) -> Result<()> {
        let input = match input {
            AndroidInput::ArchImportFinished { bundled, result } => {
                if bundled {
                    crate::android_arch::finish(result.is_ok());
                    let waiting: Vec<_> = self
                        .panes
                        .values()
                        .filter_map(|pane| pane.waiting_for_arch.then_some(pane.id))
                        .collect();
                    if let Err(error) = result {
                        self.ui.notice = Some(format!("Arch Linux setup failed · {error}"));
                    }
                    for id in waiting {
                        let pane = TerminalPane::spawn(
                            id,
                            None,
                            &self.options,
                            &self.ui.settings,
                            self.proxy.clone(),
                        )?;
                        self.panes.insert(id, pane);
                    }
                    if self.panes.contains_key(&self.workspace.active_pane) {
                        self.focus(self.workspace.active_pane);
                    }
                } else {
                    self.ui.notice = Some(match result {
                        Ok(()) => "Arch Linux installed. Open Arch Linux from Commands.".into(),
                        Err(error) => format!("Arch Linux import failed · {error}"),
                    });
                }
                self.redraw();
                return Ok(());
            }
            AndroidInput::Viewport {
                x,
                y,
                width,
                height,
            } => {
                self.android_viewport = Some([x, y, width, height]);
                self.redraw();
                return Ok(());
            }
            AndroidInput::MotionQueued(token) => {
                let Some(input) = PENDING_MOTION
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.take(&token))
                else {
                    return Ok(());
                };
                input
            }
            input => input,
        };
        if INPUT_DIAGNOSTICS.load(std::sync::atomic::Ordering::Acquire) {
            let category = match &input {
                AndroidInput::Commit(_) => "commit",
                AndroidInput::Preedit(_) => "preedit",
                AndroidInput::Key { .. } => "key",
                AndroidInput::ImeEnabled => "ime-enabled",
                AndroidInput::ImeDisabled => "ime-disabled",
                AndroidInput::Viewport { .. } => "viewport",
                AndroidInput::Pointer { .. } => "pointer",
                AndroidInput::Scroll { .. } => "scroll",
                AndroidInput::Copy => "copy",
                AndroidInput::Cut => "cut",
                AndroidInput::Paste(_) => "paste",
                AndroidInput::SelectAll => "select-all",
                AndroidInput::ArchImportFinished { .. } => "arch-import",
                AndroidInput::MotionQueued(_) => "motion-queued",
            };
            eprintln!("Android input route category={category} terminal_focus={} window_focus={} modal={} overlay={} terminal={} egui_focus={} window={}",
                self.terminal_focused, self.window_focused,
                self.object.is_some() || self.ui.blocks_terminal(), self.ui.overlay_open(),
                self.terminal_input(), self.gui.memory(|memory| memory.focused().is_some()), self.window.is_some());
        }
        let Some((id, scale)) = self
            .window
            .as_ref()
            .map(|window| (window.id(), window.scale_factor()))
        else {
            return Ok(());
        };
        let device_id = DeviceId::dummy();
        if let AndroidInput::Pointer { phase, x, y } = &input {
            // Touch accompanies the pointer stream for egui's drag scrolling.
            // Forward the end even when a UI action has restored shell focus.
            self.android_ui_event(egui::Event::Touch {
                device_id: egui::TouchDeviceId(0),
                id: egui::TouchId(0),
                phase: match phase {
                    PointerPhase::Started => egui::TouchPhase::Start,
                    PointerPhase::Moved => egui::TouchPhase::Move,
                    PointerPhase::Ended => egui::TouchPhase::End,
                    PointerPhase::Cancelled => egui::TouchPhase::Cancel,
                },
                pos: egui::pos2((x / scale) as f32, (y / scale) as f32),
                force: None,
            });
        }
        match input {
            AndroidInput::Commit(text) => {
                self.window_event(event_loop, id, WindowEvent::Ime(Ime::Commit(text)))
            }
            AndroidInput::Preedit(text) => {
                let cursor = text.len();
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::Ime(Ime::Preedit(text, Some((cursor, cursor)))),
                )
            }
            AndroidInput::ImeEnabled => {
                // A new Java transaction must capture the current egui selection,
                // including a Ctrl+A performed since the previous enable.
                if let Some(state) = &mut self.gui_state {
                    state.ime_event_disable();
                }
                self.window_event(event_loop, id, WindowEvent::Ime(Ime::Enabled))
            }
            AndroidInput::ImeDisabled => {
                self.android_selection = None;
                self.window_event(event_loop, id, WindowEvent::Ime(Ime::Disabled))
            }
            AndroidInput::Key { name, modifiers: 0 } if name == "Commands" => {
                if let Some((pane, revision, selection)) = self.android_selection.take() {
                    if let Some(pane) = self.panes.get_mut(&pane) {
                        if pane.output_revision == revision {
                            pane.selection = Some(selection);
                        }
                    }
                }
                let object = self
                    .terminal_input()
                    .then(|| {
                        let pane = self.terminal_pointer()?;
                        self.panes.get(&pane)?.object_at(self.cell(pane)?)
                    })
                    .flatten();
                if let Some(object) = object {
                    self.cancel_composition();
                    self.cancel_mouse_capture();
                    self.object = Some(object);
                    self.terminal_focused = false;
                    self.redraw();
                } else {
                    self.dispatch_shortcut(Shortcut::Commands, event_loop);
                }
            }
            AndroidInput::Key { name, modifiers: 0 } if name == "Settings" => {
                self.dispatch_shortcut(Shortcut::Settings, event_loop);
            }
            AndroidInput::Key { name, modifiers } if self.terminal_input() => {
                return self.key(&name, modifiers)
            }
            AndroidInput::Key { name, modifiers } => {
                let modifiers = egui::Modifiers {
                    shift: modifiers & 1 != 0,
                    alt: modifiers & 2 != 0,
                    ctrl: modifiers & 4 != 0,
                    command: modifiers & 4 != 0,
                    mac_cmd: false,
                };
                match name.to_ascii_lowercase().as_str() {
                    "c" if modifiers.command => self.android_ui_event(egui::Event::Copy),
                    "x" if modifiers.command => self.android_ui_event(egui::Event::Cut),
                    "v" if modifiers.command => platform_action(PlatformAction::RequestPaste)?,
                    _ => {
                        if let Some(key) = ui_key(&name) {
                            for pressed in [true, false] {
                                self.android_ui_event(egui::Event::Key {
                                    key,
                                    physical_key: None,
                                    pressed,
                                    repeat: false,
                                    modifiers,
                                });
                            }
                        }
                    }
                }
            }
            AndroidInput::Pointer {
                phase: PointerPhase::Cancelled,
                x,
                y,
            } => {
                // The cancellation boundary carries the latest physical touch
                // position even when an older coalesced move was discarded.
                self.pointer = PhysicalPosition::new(x, y);
                self.command_press = None;
                self.window_event(event_loop, id, WindowEvent::CursorLeft { device_id });
                if let Some(state) = &mut self.gui_state {
                    ui::cancel_android_pointer(&self.gui, &mut state.egui_input_mut().events);
                }
                self.redraw();
            }
            AndroidInput::Pointer { phase, x, y } => {
                if phase == PointerPhase::Started {
                    self.android_selection = self
                        .terminal_input()
                        .then(|| {
                            let id = self.pane_at(PhysicalPosition::new(x, y))?;
                            let pane = self.panes.get(&id)?;
                            Some((id, pane.output_revision, pane.selection?))
                        })
                        .flatten();
                } else if phase == PointerPhase::Ended {
                    self.android_selection = None;
                }
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::CursorMoved {
                        device_id,
                        position: PhysicalPosition::new(x, y),
                    },
                );
                let state = match phase {
                    PointerPhase::Started => Some(ElementState::Pressed),
                    PointerPhase::Ended => Some(ElementState::Released),
                    PointerPhase::Moved => None,
                    PointerPhase::Cancelled => unreachable!("cancelled pointer handled above"),
                };
                if let Some(state) = state {
                    self.window_event(
                        event_loop,
                        id,
                        WindowEvent::MouseInput {
                            device_id,
                            state,
                            button: MouseButton::Left,
                        },
                    );
                }
                if phase == PointerPhase::Ended {
                    self.window_event(event_loop, id, WindowEvent::CursorLeft { device_id });
                }
            }
            AndroidInput::Scroll { x, y, dx, dy } => {
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::CursorMoved {
                        device_id,
                        position: PhysicalPosition::new(x, y),
                    },
                );
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::MouseWheel {
                        device_id,
                        delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(dx, dy)),
                        phase: TouchPhase::Moved,
                    },
                );
            }
            AndroidInput::Copy if self.terminal_input() => {
                self.copy();
            }
            AndroidInput::Copy => self.android_ui_event(egui::Event::Copy),
            AndroidInput::Cut if !self.terminal_input() => self.android_ui_event(egui::Event::Cut),
            AndroidInput::Cut => {}
            AndroidInput::Paste(text) if self.terminal_input() => {
                let id = self.workspace.active_pane;
                let revision = self
                    .active()
                    .session
                    .as_ref()
                    .map(|session| session.output_revision());
                let result = self.active_mut().paste(&text);
                if result.is_ok() {
                    if let Some(revision) = revision {
                        self.metrics.input(id, revision);
                    }
                }
                self.input_result(result);
            }
            AndroidInput::Paste(text) => self.android_ui_event(egui::Event::Paste(text)),
            AndroidInput::SelectAll if !self.terminal_input() => {
                for pressed in [true, false] {
                    self.android_ui_event(egui::Event::Key {
                        key: egui::Key::A,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers {
                            ctrl: true,
                            command: true,
                            ..Default::default()
                        },
                    });
                }
            }
            AndroidInput::SelectAll => {}
            AndroidInput::Viewport { .. }
            | AndroidInput::MotionQueued(_)
            | AndroidInput::ArchImportFinished { .. } => {
                unreachable!("platform state events resolved before window dispatch")
            }
        }
        Ok(())
    }
}

// FVPROV1 21009 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNzk2YWY4YzdmNmI3NmIxODRmNTE3NGE5NGFkNWViZjk1YjAzYzg1YjhkZGNiZTEzZTE5NTdkNzkyZmNlYTZkNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOWE5N2E5ZmE5NzA5OWRhMjZlYmQ2ODgxYmZiYmMyNTU0YTkxNjIyNDRiMWNiNDRjZDZiMmY2MTc4Y2FiNDMwMzEzOTdiZjRmNmZlYmQ5Njg2YjE1YWY3ZGNjMzRkY2M2NjIzYTIyZDZmYmQ5ZjM5YzRmMjBkMDVmNTY4YTEzMGYifV19XQ== FVPROV1-END
