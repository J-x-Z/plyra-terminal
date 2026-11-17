// SPDX-License-Identifier: GPL-3.0-only
//! Owned, ordinary-user terminal sessions with coalesced UI notifications.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use terminal_core::{
    Cursor, Line, Run, Screen, TerminalPalette, MAX_COLS, MAX_EVENT_BYTES, MAX_HISTORY_BYTES,
    MAX_HISTORY_CELLS, MAX_HISTORY_LINES, MAX_ROWS, MIN_COLS, MIN_ROWS,
};
mod aether_events;
mod shell_integration;
pub use aether_events::{ShellEvent, ShellIntegration};

const MAX_INPUT: usize = 65_536;
const MAX_PENDING_INPUT: usize = 262_144;
const MAX_EVENTS: usize = 64;
const MAX_TERMINAL_REPLIES: usize = 16_384;

#[derive(Default)]
struct PortableCallbacks {
    replies: Vec<u8>,
    overflow: bool,
    #[cfg(test)]
    cursor_reports: usize,
}

impl vt100::Callbacks for PortableCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        second_intermediate: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if intermediate.is_some()
            || second_intermediate.is_some()
            || command != 'n'
            || params.len() != 1
            || params[0].len() != 1
        {
            return;
        }
        let reply = match params[0][0] {
            5 => b"\x1b[0n".to_vec(),
            6 => {
                let (row, column) = screen.cursor_position();
                #[cfg(test)]
                {
                    self.cursor_reports += 1;
                }
                format!("\x1b[{};{}R", u32::from(row) + 1, u32::from(column) + 1).into_bytes()
            }
            _ => return,
        };
        if self.replies.len() + reply.len() > MAX_TERMINAL_REPLIES {
            self.overflow = true;
        } else {
            self.replies.extend_from_slice(&reply);
        }
    }
}

type PortableParser = vt100::Parser<PortableCallbacks>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Backend {
    #[default]
    Auto,
    Helper,
    Portable,
}

#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub backend: Backend,
    pub cols: u16,
    pub rows: u16,
    pub shell: Option<String>,
    pub command: Option<Vec<String>>,
    pub shell_args: Vec<String>,
    pub login: Option<bool>,
    pub environment: BTreeMap<String, String>,
    pub palette: TerminalPalette,
    pub shell_integration: ShellIntegration,
    pub shell_event_dir: Option<PathBuf>,
    pub native_presentation: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            cols: 80,
            rows: 24,
            shell: None,
            command: None,
            shell_args: Vec::new(),
            login: None,
            environment: BTreeMap::new(),
            palette: TerminalPalette::default(),
            shell_integration: ShellIntegration::None,
            shell_event_dir: None,
            native_presentation: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Shell {
        at_ms: u64,
        event: ShellEvent,
    },
    ShellWarning(String),
    Command {
        at_ms: u64,
        phase: String,
        detail: String,
    },
    Ready {
        helper_pid: Option<u32>,
        shell_pid: Option<u32>,
        shell: String,
    },
    Exit(i32),
    Error(String),
}

enum WriterMessage {
    Bytes(Vec<u8>),
    Stop,
}

struct State {
    screen: Screen,
    palette: TerminalPalette,
    events: VecDeque<SessionEvent>,
    exit_seen: bool,
}

struct Shared {
    state: Mutex<State>,
    pending: AtomicBool,
    stopping: AtomicBool,
    writer_stop: Mutex<Option<SyncSender<WriterMessage>>>,
    reader_done: AtomicBool,
    queued_bytes: AtomicUsize,
    output_revision: AtomicUsize,
    shell_identity: Mutex<Option<u32>>,
    shell_ready: Condvar,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    fn event(&self, event: SessionEvent) {
        if let SessionEvent::Ready {
            shell_pid: Some(pid),
            ..
        } = &event
        {
            *self.shell_identity.lock().unwrap() = Some(*pid);
            self.shell_ready.notify_all();
        }
        let mut state = self.state.lock().unwrap();
        if matches!(event, SessionEvent::Exit(_)) {
            if state.exit_seen {
                return;
            }
            state.exit_seen = true;
        }
        if state.events.len() == MAX_EVENTS {
            state.events.pop_front();
        }
        state.events.push_back(event);
        drop(state);
        self.notify();
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.shell_ready.notify_all();
        if let Some(sender) = self.writer_stop.lock().unwrap().take() {
            // A full queue already wakes the receiver. It checks stopping before writing.
            let _ = sender.try_send(WriterMessage::Stop);
        }
    }

    fn fail(&self, error: impl std::fmt::Display) {
        self.stop();
        self.event(SessionEvent::Error(error.to_string()));
    }
}

enum OwnedChild {
    Helper(Child),
    Portable(Box<dyn portable_pty::Child + Send + Sync>),
}

impl OwnedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Helper(child) => child
                .try_wait()
                .map(|status| status.map(|status| status.code().unwrap_or(1))),
            Self::Portable(child) => child
                .try_wait()
                .map(|status| status.map(|status| status.exit_code() as i32)),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Helper(child) => child.kill(),
            Self::Portable(child) => child.kill(),
        }
    }
}

struct ChildState {
    child: OwnedChild,
    exited: bool,
}

pub struct Session {
    shared: Arc<Shared>,
    writer: Option<SyncSender<WriterMessage>>,
    child: Arc<Mutex<ChildState>>,
    master: Option<Box<dyn MasterPty + Send>>,
    parser: Option<Arc<Mutex<PortableParser>>>,
    helper: bool,
    #[cfg(unix)]
    shell_pid: Option<u32>,
    closed: bool,
    #[cfg(unix)]
    shell_channel: Option<aether_events::ShellChannel>,
}

impl Session {
    pub fn spawn(options: SessionOptions, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        validate_shell_configuration(&options)?;
        #[cfg(not(unix))]
        ensure!(
            options.shell_integration == ShellIntegration::None,
            "Aether Shell structured integration currently requires a Unix host"
        );
        ensure!(
            (MIN_COLS..=MAX_COLS).contains(&options.cols)
                && (MIN_ROWS..=MAX_ROWS).contains(&options.rows),
            "terminal dimensions outside supported bounds"
        );
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                screen: Screen::new(options.cols, options.rows),
                palette: options.palette.clone(),
                events: VecDeque::new(),
                exit_seen: false,
            }),
            pending: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            writer_stop: Mutex::new(None),
            reader_done: AtomicBool::new(false),
            queued_bytes: AtomicUsize::new(0),
            output_revision: AtomicUsize::new(0),
            shell_identity: Mutex::new(None),
            shell_ready: Condvar::new(),
            wake,
        });
        let helper = match options.backend {
            Backend::Helper => Some(helper_path()?),
            Backend::Portable => None,
            Backend::Auto if cfg!(any(windows, target_os = "android")) => None,
            Backend::Auto => {
                // An explicit override must validate even in automatic mode.
                if std::env::var_os("PLYRA_TERMINAL_HELPER").is_some() {
                    Some(helper_path()?)
                } else {
                    helper_path().ok()
                }
            }
        };
        match helper {
            Some(path) => Self::spawn_helper(options, shared, &path),
            None => Self::spawn_portable(options, shared),
        }
    }

    fn spawn_helper(options: SessionOptions, shared: Arc<Shared>, path: &Path) -> Result<Self> {
        #[cfg(unix)]
        let mut shell_channel = prepare_shell_channel(&options)?;
        let mut command = Command::new(path);
        command.args([
            "--timeline",
            "1",
            "--cols",
            &options.cols.to_string(),
            "--rows",
            &options.rows.to_string(),
        ]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .envs(&options.environment)
            .arg("--palette")
            .arg(encode_palette(&options.palette));
        #[cfg(unix)]
        if let Some(channel) = &shell_channel {
            command.envs(channel.environment());
            command.env(
                "AETHER_GUI_PRESENTATION",
                if options.native_presentation {
                    "1"
                } else {
                    "0"
                },
            );
        }
        if let Some(shell) = options.shell.as_deref() {
            validate_shell(shell)?;
            command.env("SHELL", shell);
        }
        if let Some(arguments) = options.command.as_ref() {
            validate_command(arguments)?;
            command.arg("--exec").args(arguments);
        } else {
            let shell = default_shell(
                options
                    .shell
                    .clone()
                    .or_else(|| options.environment.get("SHELL").cloned()),
            )?;
            command.env("SHELL", &shell);
            let login = options
                .login
                .unwrap_or(cfg!(target_os = "macos") && options.shell.is_none());
            if login {
                command.args(["--login", "1"]);
            }
            shell_integration::configure(&mut command, Some(&shell))?;
            if !options.shell_args.is_empty() {
                command.arg("--exec").arg(&shell);
                if login {
                    command.arg("-l");
                }
                command.arg("-i").args(&options.shell_args);
            }
        }
        let mut child = command
            .spawn()
            .context("starting the native terminal helper")?;
        let helper_pid = child.id();
        let input = child.stdin.take().context("helper stdin unavailable")?;
        let output = child.stdout.take().context("helper stdout unavailable")?;
        let writer = spawn_writer(input, shared.clone());
        let child = Arc::new(Mutex::new(ChildState {
            child: OwnedChild::Helper(child),
            exited: false,
        }));
        spawn_waiter(child.clone(), shared.clone(), true);
        spawn_helper_reader(output, shared.clone(), helper_pid);
        #[cfg(unix)]
        if let Some(channel) = &mut shell_channel {
            channel.start(shared.clone());
        }
        Ok(Self {
            shared,
            writer: Some(writer),
            child,
            master: None,
            parser: None,
            helper: true,
            #[cfg(unix)]
            shell_pid: None,
            closed: false,
            #[cfg(unix)]
            shell_channel,
        })
    }

    fn spawn_portable(options: SessionOptions, shared: Arc<Shared>) -> Result<Self> {
        #[cfg(unix)]
        let mut shell_channel = prepare_shell_channel(&options)?;
        let pair =
            portable_pty::native_pty_system().openpty(pty_size(options.cols, options.rows))?;
        let explicit = options.command.as_ref();
        if let Some(arguments) = explicit {
            validate_command(arguments)?;
        }
        let shell = match explicit {
            Some(arguments) => arguments[0].clone(),
            None => default_shell(
                options
                    .shell
                    .clone()
                    .or_else(|| options.environment.get("SHELL").cloned()),
            )?,
        };
        let mut command = CommandBuilder::new(&shell);
        for (name, value) in &options.environment {
            command.env(name, value);
        }
        if let Some(arguments) = explicit {
            command.args(&arguments[1..]);
        }
        if explicit.is_none() {
            #[cfg(unix)]
            {
                if options
                    .login
                    .unwrap_or(cfg!(target_os = "macos") && options.shell.is_none())
                {
                    command.arg("-l");
                }
                command.arg("-i");
            }
            command.args(&options.shell_args);
            let mut integration = Command::new(&shell);
            integration.envs(&options.environment);
            shell_integration::configure(&mut integration, Some(&shell))?;
            for (name, value) in integration.get_envs() {
                if let Some(value) = value {
                    command.env(name, value);
                }
            }
            command.env("SHELL", &shell);
        }
        #[cfg(unix)]
        if let Some(channel) = &shell_channel {
            for (name, value) in channel.environment() {
                command.env(name, value);
            }
            command.env(
                "AETHER_GUI_PRESENTATION",
                if options.native_presentation {
                    "1"
                } else {
                    "0"
                },
            );
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        if let Some(home) = command
            .get_env(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(ToOwned::to_owned)
        {
            if Path::new(&home).is_dir() {
                command.cwd(home);
            }
        }
        // Acquire all fallible handles before spawning an owned child.
        let output = pair.master.try_clone_reader()?;
        let input = pair.master.take_writer()?;
        let child = pair
            .slave
            .spawn_command(command)
            .context("starting the PTY shell")?;
        let shell_pid = child.process_id();
        drop(pair.slave);
        let parser = Arc::new(Mutex::new(PortableParser::new_with_callbacks(
            options.rows,
            options.cols,
            2000,
            PortableCallbacks::default(),
        )));
        let writer = spawn_writer(input, shared.clone());
        let child = Arc::new(Mutex::new(ChildState {
            child: OwnedChild::Portable(child),
            exited: false,
        }));
        spawn_waiter(child.clone(), shared.clone(), false);
        spawn_portable_reader(output, parser.clone(), shared.clone());
        shared.event(SessionEvent::Ready {
            helper_pid: None,
            shell_pid,
            shell,
        });
        #[cfg(unix)]
        if let Some(channel) = &mut shell_channel {
            channel.start(shared.clone());
        }
        Ok(Self {
            shared,
            writer: Some(writer),
            child,
            master: Some(pair.master),
            parser: Some(parser),
            helper: false,
            #[cfg(unix)]
            shell_pid,
            closed: false,
            #[cfg(unix)]
            shell_channel,
        })
    }

    pub fn has_shell_operations(&self) -> bool {
        #[cfg(unix)]
        return self
            .shell_channel
            .as_ref()
            .is_some_and(|channel| channel.is_connected());
        #[cfg(not(unix))]
        false
    }

    pub fn host_operation_result(
        &self,
        request_id: u64,
        status: i32,
        result: Value,
        error: Option<String>,
    ) -> Result<()> {
        #[cfg(unix)]
        return self
            .shell_channel
            .as_ref()
            .context("session has no Shell host channel")?
            .host_operation_result(request_id, status, result, error);
        #[cfg(not(unix))]
        {
            let _ = (request_id, status, result, error);
            bail!("Shell host operations require a Unix host")
        }
    }

    pub fn shell_operation(&self, request_id: u64, operation: &str, args: &[String]) -> Result<()> {
        #[cfg(unix)]
        return self
            .shell_channel
            .as_ref()
            .context("session has no Aether Shell operation channel")?
            .operation(request_id, operation, args);
        #[cfg(not(unix))]
        {
            let _ = (request_id, operation, args);
            bail!("Aether Shell structured operations currently require a Unix host")
        }
    }

    pub fn snapshot(&self) -> Screen {
        self.shared.state.lock().unwrap().screen.clone()
    }

    /// Counts applied screen updates, including portable resize and palette changes.
    pub fn output_revision(&self) -> u64 {
        self.shared.output_revision.load(Ordering::Acquire) as u64
    }

    pub fn set_palette(&mut self, palette: TerminalPalette) -> Result<()> {
        ensure!(
            !self.closed && !self.shared.stopping.load(Ordering::Acquire),
            "session is closed"
        );
        if self.shared.state.lock().unwrap().palette == palette {
            return Ok(());
        }
        if self.helper {
            self.queue(format!("palette {}\n", encode_palette(&palette)).into_bytes())?;
            self.shared.state.lock().unwrap().palette = palette;
        } else {
            let mut parser = self
                .parser
                .as_ref()
                .context("terminal parser unavailable")?
                .lock()
                .unwrap();
            self.shared.state.lock().unwrap().palette = palette;
            update_portable_snapshot(&mut parser, &self.shared);
            self.shared.output_revision.fetch_add(1, Ordering::Release);
            self.shared.notify();
        }
        Ok(())
    }

    pub fn take_events(&self) -> Vec<SessionEvent> {
        let mut state = self.shared.state.lock().unwrap();
        self.shared.pending.store(false, Ordering::Release);
        state.events.drain(..).collect()
    }

    pub fn input(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= MAX_INPUT, "input exceeds 64 KiB");
        if bytes.is_empty() {
            return Ok(());
        }
        if self.helper {
            self.queue(format!("input {}\n", STANDARD.encode(bytes)).into_bytes())
        } else {
            self.queue(bytes.to_vec())
        }
    }

    pub fn paste(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= MAX_INPUT, "paste exceeds 64 KiB");
        if self.helper {
            return self.queue(format!("paste {}\n", STANDARD.encode(bytes)).into_bytes());
        }
        let bracketed = self
            .parser
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .screen()
            .bracketed_paste();
        let mut data = Vec::with_capacity(bytes.len() + 12);
        if bracketed {
            data.extend_from_slice(b"\x1b[200~");
        }
        data.extend_from_slice(bytes);
        if bracketed {
            data.extend_from_slice(b"\x1b[201~");
        }
        self.queue(data)
    }

    pub fn key(&mut self, name: &str, modifiers: u8) -> Result<()> {
        ensure!(modifiers <= 7, "invalid key modifiers");
        // Validate the same named-key vocabulary for both backends.
        let application_cursor = self
            .parser
            .as_ref()
            .is_some_and(|parser| parser.lock().unwrap().screen().application_cursor());
        let encoded = encode_key(name, modifiers, application_cursor)?;
        if self.helper {
            self.queue(format!("key {name} {modifiers}\n").into_bytes())
        } else {
            self.queue(encoded)
        }
    }

    pub fn mouse(
        &mut self,
        row: u16,
        col: u16,
        button: u8,
        pressed: bool,
        modifiers: u8,
    ) -> Result<()> {
        let screen = self.snapshot();
        ensure!(
            row < screen.rows && col < screen.cols && button <= 7 && modifiers <= 7,
            "invalid mouse event"
        );
        if screen.mouse_mode == 0 {
            return Ok(());
        }
        if self.helper {
            return self.queue(
                format!(
                    "mouse {row} {col} {button} {} {modifiers}\n",
                    u8::from(pressed)
                )
                .into_bytes(),
            );
        }
        let parser = self.parser.as_ref().unwrap().lock().unwrap();
        let source = parser.screen();
        if button == 0 && source.mouse_protocol_mode() != vt100::MouseProtocolMode::AnyMotion {
            return Ok(());
        }
        if !pressed && source.mouse_protocol_mode() == vt100::MouseProtocolMode::Press {
            return Ok(());
        }
        let code = if button == 0 {
            35
        } else if button <= 3 {
            button - 1
        } else {
            64 + button - 4
        };
        let bytes = encode_mouse_payload(
            source.mouse_protocol_encoding(),
            row,
            col,
            code,
            pressed,
            modifiers,
        );
        drop(parser);
        self.queue(bytes)
    }

    pub fn mouse_move(&mut self, row: u16, col: u16, modifiers: u8) -> Result<()> {
        self.mouse_motion(row, col, 0, modifiers)
    }

    /// Report motion with the captured button (1=left, 2=middle, 3=right),
    /// or zero for unpressed motion. Motion never presses or releases a button.
    pub fn mouse_motion(&mut self, row: u16, col: u16, button: u8, modifiers: u8) -> Result<()> {
        let screen = self.snapshot();
        ensure!(
            row < screen.rows && col < screen.cols && button <= 3 && modifiers <= 7,
            "invalid mouse motion"
        );
        if screen.mouse_mode < if button == 0 { 3 } else { 2 } {
            return Ok(());
        }
        if self.helper {
            // libvterm tracks the buttons established by mouse press/release.
            // Its move primitive emits 32/33/34 for that capture, or 35 with
            // no pressed button; sending another button event would duplicate it.
            return self.queue(format!("mouse {row} {col} 0 1 {modifiers}\n").into_bytes());
        }
        let parser = self.parser.as_ref().unwrap().lock().unwrap();
        let source = parser.screen();
        let bytes = encode_mouse_motion(
            source.mouse_protocol_mode(),
            source.mouse_protocol_encoding(),
            row,
            col,
            button,
            modifiers,
        );
        drop(parser);
        match bytes {
            Some(bytes) => self.queue(bytes),
            None => Ok(()),
        }
    }

    pub fn focus(&mut self, focused: bool) -> Result<()> {
        if self.helper {
            self.queue(format!("focus {}\n", u8::from(focused)).into_bytes())
        } else {
            Ok(())
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        ensure!(
            (MIN_COLS..=MAX_COLS).contains(&cols) && (MIN_ROWS..=MAX_ROWS).contains(&rows),
            "terminal dimensions outside supported bounds"
        );
        if self.helper {
            return self.queue(format!("resize {cols} {rows}\n").into_bytes());
        }
        ensure!(!self.closed, "session is closed");
        self.master
            .as_ref()
            .context("PTY master closed")?
            .resize(pty_size(cols, rows))?;
        let mut parser = self.parser.as_ref().unwrap().lock().unwrap();
        parser.screen_mut().set_size(rows, cols);
        update_portable_snapshot(&mut parser, &self.shared);
        drop(parser);
        self.shared.output_revision.fetch_add(1, Ordering::Release);
        self.shared.notify();
        Ok(())
    }

    // fetch_update is available at the workspace's Rust 1.82 minimum.
    #[allow(deprecated)]
    fn queue(&self, bytes: Vec<u8>) -> Result<()> {
        ensure!(
            !self.closed && !self.shared.stopping.load(Ordering::Acquire),
            "session is closed"
        );
        let writer = self.writer.as_ref().context("session writer closed")?;
        queue_writer(writer, &self.shared, bytes)
    }

    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shared.stop();
        #[cfg(unix)]
        self.shell_channel.take();
        self.writer.take();
        if !self.helper {
            let mut process = self.child.lock().unwrap();
            if !process.exited {
                #[cfg(unix)]
                if let Some(pid) = self.shell_pid {
                    // Keep the child unreaped through escalation so its session
                    // identifier cannot be reused while signals are issued.
                    retire_owned_session(pid);
                }
                process.child.kill().ok();
            }
        }
        self.master.take();
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if self.child.lock().unwrap().exited {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let mut process = self.child.lock().unwrap();
        if !process.exited {
            process
                .child
                .kill()
                .context("terminating the owned terminal process")?;
        }
        // The waiter retains the child handle and reaps it after this bounded close.
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn spawn_writer(
    mut writer: impl Write + Send + 'static,
    shared: Arc<Shared>,
) -> SyncSender<WriterMessage> {
    let (sender, receiver) = mpsc::sync_channel::<WriterMessage>(256);
    *shared.writer_stop.lock().unwrap() = Some(sender.clone());
    thread::spawn(move || {
        while !shared.stopping.load(Ordering::Acquire) {
            let Ok(WriterMessage::Bytes(bytes)) = receiver.recv() else {
                break;
            };
            if shared.stopping.load(Ordering::Acquire) {
                shared.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
                break;
            }
            let result = writer.write_all(&bytes).and_then(|_| writer.flush());
            shared.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            if let Err(error) = result {
                if !shared.stopping.load(Ordering::Acquire) {
                    shared.fail(format!("terminal write failed: {error}"));
                }
                break;
            }
        }
        // Dropping stdin is the helper's authoritative close signal.
    });
    sender
}

fn encode_mouse_payload(
    encoding: vt100::MouseProtocolEncoding,
    row: u16,
    col: u16,
    button_code: u8,
    pressed: bool,
    modifiers: u8,
) -> Vec<u8> {
    let code = button_code | (modifiers << 2);
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => format!(
            "\x1b[<{code};{};{}{}",
            col + 1,
            row + 1,
            if pressed { "M" } else { "m" }
        )
        .into_bytes(),
        vt100::MouseProtocolEncoding::Utf8 => {
            let code = if pressed { code } else { 3 | (modifiers << 2) };
            format!(
                "\x1b[M{}{}{}",
                char::from_u32(u32::from(code) + 32).unwrap(),
                char::from_u32(u32::from(col) + 33).unwrap(),
                char::from_u32(u32::from(row) + 33).unwrap()
            )
            .into_bytes()
        }
        vt100::MouseProtocolEncoding::Default => vec![
            27,
            b'[',
            b'M',
            if pressed {
                code + 32
            } else {
                35 | (modifiers << 2)
            },
            (col + 33).min(255) as u8,
            (row + 33).min(255) as u8,
        ],
    }
}

fn encode_mouse_motion(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    row: u16,
    col: u16,
    button: u8,
    modifiers: u8,
) -> Option<Vec<u8>> {
    let tracked = mode == vt100::MouseProtocolMode::AnyMotion
        || (button != 0 && mode == vt100::MouseProtocolMode::ButtonMotion);
    tracked.then(|| {
        encode_mouse_payload(
            encoding,
            row,
            col,
            32 + if button == 0 { 3 } else { button - 1 },
            true,
            modifiers,
        )
    })
}

fn queue_writer(sender: &SyncSender<WriterMessage>, shared: &Shared, bytes: Vec<u8>) -> Result<()> {
    ensure!(
        !shared.stopping.load(Ordering::Acquire),
        "session is closed"
    );
    let length = bytes.len();
    shared
        .queued_bytes
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
            pending
                .checked_add(length)
                .filter(|total| *total <= MAX_PENDING_INPUT)
        })
        .map_err(|_| anyhow::anyhow!("terminal input queue is full"))?;
    if sender.try_send(WriterMessage::Bytes(bytes)).is_err() {
        shared.queued_bytes.fetch_sub(length, Ordering::AcqRel);
        bail!("terminal input queue is full or closed");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_for_owned_child_exit(child: &Mutex<ChildState>) -> std::io::Result<()> {
    let pid = {
        let process = child.lock().unwrap();
        if process.exited {
            return Ok(());
        }
        match &process.child {
            OwnedChild::Helper(child) => Some(child.id()),
            OwnedChild::Portable(child) => child.process_id(),
        }
        .ok_or_else(|| std::io::Error::other("owned child PID is unavailable"))?
    };
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // WNOWAIT preserves the owned PID until the lifecycle lock is reacquired for reap.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            // Revalidate the cached child handle under its lock, including an external reap.
            Some(libc::ECHILD) => return Ok(()),
            _ => return Err(error),
        }
    }
}

#[cfg(windows)]
fn wait_for_owned_child_exit(child: &Mutex<ChildState>) -> std::io::Result<i32> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{DuplicateHandle, WAIT_OBJECT_0},
        System::Threading::{
            GetCurrentProcess, GetExitCodeProcess, WaitForSingleObject, INFINITE,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        },
    };

    let handle = {
        let process = child.lock().unwrap();
        let raw = match &process.child {
            OwnedChild::Helper(child) => child.as_raw_handle(),
            OwnedChild::Portable(child) => child
                .as_raw_handle()
                .ok_or_else(|| std::io::Error::other("owned child handle is unavailable"))?,
        };
        let mut duplicate = std::ptr::null_mut();
        let copied = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                raw,
                GetCurrentProcess(),
                &mut duplicate,
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                0,
            )
        };
        if copied == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // This noninherited duplicate pins the owned process after releasing the lifecycle lock.
        unsafe { OwnedHandle::from_raw_handle(duplicate) }
    };
    // Session::close can acquire the lifecycle lock and terminate the process during this wait.
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut code = 0;
    if unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // A signaled process can genuinely exit with STILL_ACTIVE (259).
    Ok(code as i32)
}

fn spawn_waiter(child: Arc<Mutex<ChildState>>, shared: Arc<Shared>, helper: bool) {
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        if let Err(error) = wait_for_owned_child_exit(&child) {
            // Preserve the existing reap path if native exit notification is unavailable.
            eprintln!("terminal child exit notification failed: {error}");
        }
        #[cfg(windows)]
        let mut native_exit_code = match wait_for_owned_child_exit(&child) {
            Ok(code) => Some(code),
            Err(error) => {
                eprintln!("terminal child exit notification failed: {error}");
                None
            }
        };
        loop {
            let status = {
                let mut process = child.lock().unwrap();
                #[cfg(unix)]
                if let OwnedChild::Portable(owned) = &process.child {
                    if let Some(pid) = owned.process_id() {
                        if owned_child_finished(pid) && !shared.stopping.load(Ordering::Acquire) {
                            // WNOWAIT preserves the child's PID while remaining
                            // processes in its PTY session are retired.
                            retire_owned_session(pid);
                        }
                    }
                }
                #[cfg(windows)]
                let status = match native_exit_code.take() {
                    Some(code) => Ok(Some(code)),
                    None => process.child.try_wait(),
                };
                #[cfg(not(windows))]
                let status = process.child.try_wait();
                match status {
                    Ok(Some(code)) => {
                        process.exited = true;
                        Some(Ok(code))
                    }
                    Ok(None) => None,
                    Err(error) => {
                        process.exited = true;
                        Some(Err(error))
                    }
                }
            };
            if let Some(status) = status {
                let deadline = Instant::now() + Duration::from_millis(300);
                while !shared.reader_done.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                shared.stop();
                match status {
                    Ok(code) if !helper => shared.event(SessionEvent::Exit(code)),
                    Ok(code) if code != 0 => shared.event(SessionEvent::Error(format!(
                        "helper exited with status {code}"
                    ))),
                    Err(error) => shared.event(SessionEvent::Error(format!(
                        "waiting for terminal process failed: {error}"
                    ))),
                    _ => {}
                }
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
}

fn spawn_helper_reader(output: impl Read + Send + 'static, shared: Arc<Shared>, helper_pid: u32) {
    thread::spawn(move || {
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        loop {
            line.clear();
            let result = Read::by_ref(&mut reader)
                .take((MAX_EVENT_BYTES + 1) as u64)
                .read_until(b'\n', &mut line);
            match result {
                Ok(0) => break,
                Ok(_) if line.len() > MAX_EVENT_BYTES || !line.ends_with(b"\n") => {
                    shared.fail("helper event exceeds limit or is truncated");
                    break;
                }
                Ok(_) => {
                    let event =
                        serde_json::from_slice::<Value>(&line).context("invalid helper JSON");
                    match event.and_then(|event| apply_helper_event(&shared, &event, helper_pid)) {
                        Ok(()) => {}
                        Err(error) => {
                            shared.fail(error);
                            break;
                        }
                    }
                }
                Err(error) => {
                    if !shared.stopping.load(Ordering::Acquire) {
                        shared.fail(error);
                    }
                    break;
                }
            }
        }
        if !shared.stopping.load(Ordering::Acquire) && !shared.state.lock().unwrap().exit_seen {
            shared.fail("helper output ended without an exit event");
        }
        shared.reader_done.store(true, Ordering::Release);
    });
}

fn apply_helper_event(shared: &Shared, event: &Value, helper_pid: u32) -> Result<()> {
    match event["type"].as_str().context("helper event has no type")? {
        "command" => {
            let phase = event["phase"]
                .as_str()
                .filter(|phase| ["start", "finish", "prompt"].contains(phase))
                .context("invalid command phase")?;
            let raw = event["detail"].as_str().context("invalid command detail")?;
            let detail = if raw.len() <= 128
                && raw
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/_.-".contains(&byte))
            {
                raw.to_owned()
            } else {
                "shell".to_owned()
            };
            shared.event(SessionEvent::Command {
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                phase: phase.to_owned(),
                detail,
            });
        }
        "screen" | "scrollback" | "scrollback_clear" => {
            shared
                .state
                .lock()
                .unwrap()
                .screen
                .apply_helper_event(event)?;
            shared.output_revision.fetch_add(1, Ordering::Release);
            shared.notify();
        }
        "ready" => {
            ensure!(
                event["protocol"].as_u64() == Some(1),
                "unsupported helper protocol"
            );
            let pid = event["pid"]
                .as_u64()
                .filter(|pid| *pid > 0 && *pid <= i32::MAX as u64)
                .context("invalid shell PID")?;
            let shell = event["shell"]
                .as_str()
                .filter(|shell| shell.len() <= 4096)
                .context("invalid shell name")?;
            shared.event(SessionEvent::Ready {
                helper_pid: Some(helper_pid),
                shell_pid: Some(pid as u32),
                shell: shell.to_owned(),
            });
        }
        "exit" => {
            let code = event["code"]
                .as_i64()
                .filter(|code| (0..=255).contains(code))
                .context("invalid shell exit code")?;
            shared.stop();
            shared.event(SessionEvent::Exit(code as i32));
        }
        "error" => {
            let message = event["message"]
                .as_str()
                .filter(|message| message.len() <= 4096)
                .context("invalid helper error")?;
            shared.event(SessionEvent::Error(message.to_owned()));
        }
        "bell" => {}
        _ => bail!("unknown helper event"),
    }
    Ok(())
}

fn spawn_portable_reader(
    mut output: Box<dyn Read + Send>,
    parser: Arc<Mutex<PortableParser>>,
    shared: Arc<Shared>,
) {
    thread::spawn(move || {
        let mut bytes = [0u8; 32_768];
        loop {
            match output.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    let mut parser = parser.lock().unwrap();
                    parser.process(&bytes[..count]);
                    let replies = std::mem::take(&mut parser.callbacks_mut().replies);
                    let overflow = std::mem::take(&mut parser.callbacks_mut().overflow);
                    update_portable_snapshot(&mut parser, &shared);
                    shared.output_revision.fetch_add(1, Ordering::Release);
                    drop(parser);
                    // The parser determines sequence boundaries and the cursor at
                    // each query. Never send or report an error while holding it.
                    if overflow {
                        shared.fail("terminal query response budget exceeded");
                        break;
                    }
                    if !replies.is_empty() && !shared.stopping.load(Ordering::Acquire) {
                        let writer = shared.writer_stop.lock().unwrap().as_ref().cloned();
                        let result = writer
                            .context("terminal query writer is unavailable")
                            .and_then(|writer| queue_writer(&writer, &shared, replies));
                        if let Err(error) = result {
                            if !shared.stopping.load(Ordering::Acquire) {
                                shared.fail(error);
                            }
                            break;
                        }
                    }
                    shared.notify();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                // Unix PTY reads commonly signal EIO when the slave closes.
                Err(error) if cfg!(unix) && error.raw_os_error() == Some(5) => break,
                Err(error) => {
                    if !shared.stopping.load(Ordering::Acquire) {
                        shared.fail(error);
                    }
                    break;
                }
            }
        }
        shared.reader_done.store(true, Ordering::Release);
    });
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn validate_shell_configuration(options: &SessionOptions) -> Result<()> {
    ensure!(options.shell_args.len() <= 128, "too many shell arguments");
    ensure!(
        options
            .shell_args
            .iter()
            .all(|argument| argument.len() <= 16_384 && !argument.contains('\0')),
        "invalid shell argument"
    );
    ensure!(
        options.environment.len() <= 128,
        "too many shell environment entries"
    );
    for (name, value) in &options.environment {
        let mut bytes = name.bytes();
        ensure!(
            name.len() <= 256
                && bytes
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            "invalid shell environment name"
        );
        ensure!(
            value.len() <= 65_536 && !value.contains('\0'),
            "invalid shell environment value"
        );
    }
    Ok(())
}

fn encode_palette(palette: &TerminalPalette) -> String {
    std::iter::once(&palette.foreground)
        .chain(std::iter::once(&palette.background))
        .chain(palette.ansi.iter())
        .map(|color| format!("{:02x}{:02x}{:02x}", color[0], color[1], color[2]))
        .collect::<Vec<_>>()
        .join(" ")
}

fn helper_path() -> Result<PathBuf> {
    let path = if let Some(path) = std::env::var_os("PLYRA_TERMINAL_HELPER") {
        PathBuf::from(path)
    } else {
        let sibling = std::env::current_exe()?.with_file_name("plyra-terminal-helper");
        if sibling.is_file() {
            sibling
        } else {
            PathBuf::from(env!("PLYRA_BUILT_HELPER"))
        }
    };
    ensure!(
        path.is_absolute() && path.is_file(),
        "native helper unavailable or override is not an absolute file path"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            path.metadata()?.permissions().mode() & 0o111 != 0,
            "native helper is not executable"
        );
    }
    Ok(path)
}

fn validate_shell(shell: &str) -> Result<()> {
    ensure!(
        !shell.is_empty() && !shell.contains('\0'),
        "invalid shell path"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = Path::new(shell);
        ensure!(
            path.is_absolute() && path.is_file(),
            "shell must be an absolute executable file"
        );
        ensure!(
            path.metadata()?.permissions().mode() & 0o111 != 0,
            "shell is not executable"
        );
    }
    Ok(())
}

fn validate_command(arguments: &[String]) -> Result<()> {
    ensure!(
        !arguments.is_empty() && arguments.len() <= 128,
        "command requires an executable and at most 127 arguments"
    );
    ensure!(
        arguments
            .iter()
            .all(|argument| argument.len() <= 16_384 && !argument.contains('\0')),
        "invalid command arguments"
    );
    validate_shell(&arguments[0])
}

fn default_shell(explicit: Option<String>) -> Result<String> {
    if let Some(shell) = explicit {
        validate_shell(&shell)?;
        return Ok(shell);
    }
    #[cfg(windows)]
    {
        Ok(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()))
    }
    #[cfg(not(windows))]
    {
        if let Ok(shell) = std::env::var("SHELL") {
            if validate_shell(&shell).is_ok() {
                return Ok(shell);
            }
        }
        #[cfg(unix)]
        {
            let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result = std::ptr::null_mut();
            let mut buffer = vec![0_u8; 16_384];
            let status = unsafe {
                libc::getpwuid_r(
                    libc::getuid(),
                    &mut entry,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            };
            if status == 0 && !result.is_null() && !entry.pw_shell.is_null() {
                if let Ok(shell) = unsafe { std::ffi::CStr::from_ptr(entry.pw_shell) }.to_str() {
                    if validate_shell(shell).is_ok() {
                        return Ok(shell.to_owned());
                    }
                }
            }
        }
        Ok(if cfg!(target_os = "android") {
            "/system/bin/sh"
        } else {
            "/bin/sh"
        }
        .into())
    }
}

fn project_screen(source: &vt100::Screen, palette: &TerminalPalette) -> Screen {
    let (rows, cols) = source.size();
    let mut screen = Screen::new(cols, rows);
    screen.alternate = source.alternate_screen();
    screen.mouse_mode = match source.mouse_protocol_mode() {
        vt100::MouseProtocolMode::None => 0,
        vt100::MouseProtocolMode::Press | vt100::MouseProtocolMode::PressRelease => 1,
        vt100::MouseProtocolMode::ButtonMotion => 2,
        vt100::MouseProtocolMode::AnyMotion => 3,
    };
    let (row, col) = source.cursor_position();
    screen.cursor = Cursor {
        row,
        col,
        visible: !source.hide_cursor(),
    };
    screen.lines = (0..rows)
        .map(|row| project_line(source, row, palette))
        .collect();
    screen
}

fn project_line(source: &vt100::Screen, row: u16, palette: &TerminalPalette) -> Line {
    let (_, cols) = source.size();
    let mut runs: Vec<Run> = Vec::with_capacity(usize::from(cols));
    let mut previous: Option<&vt100::Cell> = None;
    let mut used_columns = 0;
    for col in 0..MAX_COLS {
        let Some(cell) = source.cell(row, col) else {
            break;
        };
        {
            if cell.has_contents()
                || cell.bgcolor() != vt100::Color::Default
                || cell.underline()
                || cell.inverse()
                || cell.strike()
                || cell.hyperlink().is_some()
            {
                used_columns = col.saturating_add(if cell.is_wide() { 2 } else { 1 });
            }
            if cell.is_wide_continuation() {
                continue;
            }
            if cell.hyperlink().is_some()
                && previous.is_some_and(|previous| cell.same_style(previous))
            {
                let run = runs.last_mut().unwrap();
                run.text.push_str(if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                });
                run.width += if cell.is_wide() { 2 } else { 1 };
                previous = Some(cell);
                continue;
            }
            runs.push(Run {
                text: if cell.has_contents() {
                    cell.contents().to_owned()
                } else {
                    " ".into()
                },
                width: if cell.is_wide() { 2 } else { 1 },
                fg: rgb(cell.fgcolor(), palette.foreground, palette),
                bg: rgb(cell.bgcolor(), palette.background, palette),
                fg_default: cell.fgcolor() == vt100::Color::Default,
                fg_index: match cell.fgcolor() {
                    vt100::Color::Idx(index) => Some(index),
                    _ => None,
                },
                bg_index: match cell.bgcolor() {
                    vt100::Color::Idx(index) => Some(index),
                    _ => None,
                },
                bg_default: cell.bgcolor() == vt100::Color::Default,
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
                strike: cell.strike(),
                hyperlink: cell.hyperlink().map(str::to_owned),
            });
            previous = Some(cell);
        }
    }
    Line {
        runs,
        wrapped: source.row_wrapped(row),
        used_columns: Some(used_columns),
    }
}

fn project_parser<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    palette: &TerminalPalette,
) -> Screen {
    let previous_offset = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);
    let mut screen = project_screen(parser.screen(), palette);
    if !screen.alternate {
        parser.screen_mut().set_scrollback(usize::MAX);
        let available = parser.screen().scrollback();
        let mut history = VecDeque::new();
        let mut cells = 0;
        let mut bytes = 0;
        // Read newest history first, retaining only the common history budget.
        for offset in 1..=available {
            parser.screen_mut().set_scrollback(offset);
            let line = project_line(parser.screen(), 0, palette);
            cells += line.cell_count();
            bytes += line.storage_cost();
            if history.len() == MAX_HISTORY_LINES
                || cells > MAX_HISTORY_CELLS
                || bytes > MAX_HISTORY_BYTES
            {
                break;
            }
            history.push_front(line);
        }
        screen.replace_history(history);
        // vt100 exposes retained rows, but no count of its own evicted rows.
        screen.history_dropped = available.saturating_sub(screen.history.len());
        if screen
            .history
            .iter()
            .any(|line| line.cell_count() != usize::from(screen.cols))
        {
            screen.reflow_history(screen.cols);
        }
    }
    parser.screen_mut().set_scrollback(previous_offset);
    screen
}

fn update_portable_snapshot<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>, shared: &Shared) {
    let mut state = shared.state.lock().unwrap();
    let mut screen = project_parser(parser, &state.palette);
    if screen.alternate {
        screen.history = std::mem::take(&mut state.screen.history);
        screen.history_dropped = state.screen.history_dropped;
    } else if !screen.history.is_empty() {
        // This is a lower bound for projection-budget exclusions. vt100 does
        // not expose its own eviction count, so it cannot represent total
        // discarded rows once the parser's retained history is full.
        screen.history_dropped = screen.history_dropped.max(state.screen.history_dropped);
    }
    state.screen = screen;
}

fn rgb(color: vt100::Color, default: [u8; 3], palette: &TerminalPalette) -> [u8; 3] {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        vt100::Color::Idx(index @ 0..=15) => palette.ansi[index as usize],
        vt100::Color::Idx(index @ 16..=231) => {
            let cube = index - 16;
            let level = |n| if n == 0 { 0 } else { 55 + 40 * n };
            [level(cube / 36), level(cube / 6 % 6), level(cube % 6)]
        }
        vt100::Color::Idx(index) => [8 + 10 * (index - 232); 3],
    }
}

fn encode_key(name: &str, modifiers: u8, application_cursor: bool) -> Result<Vec<u8>> {
    let modifier = modifiers + 1;
    let final_byte = match name {
        "Up" => Some('A'),
        "Down" => Some('B'),
        "Right" => Some('C'),
        "Left" => Some('D'),
        "Home" => Some('H'),
        "End" => Some('F'),
        _ => None,
    };
    if let Some(final_byte) = final_byte {
        return Ok(if modifiers != 0 {
            format!("\x1b[1;{modifier}{final_byte}")
        } else {
            format!(
                "\x1b{}{final_byte}",
                if application_cursor { "O" } else { "[" }
            )
        }
        .into_bytes());
    }
    let number = match name {
        "Insert" => Some(2),
        "Delete" => Some(3),
        "PageUp" => Some(5),
        "PageDown" => Some(6),
        _ => None,
    };
    if let Some(number) = number {
        return Ok(if modifiers != 0 {
            format!("\x1b[{number};{modifier}~")
        } else {
            format!("\x1b[{number}~")
        }
        .into_bytes());
    }
    if let Some(function) = name
        .strip_prefix('F')
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=12).contains(value))
    {
        return Ok(if function <= 4 {
            let final_byte = ['P', 'Q', 'R', 'S'][function - 1];
            if modifiers != 0 {
                format!("\x1b[1;{modifier}{final_byte}")
            } else {
                format!("\x1bO{final_byte}")
            }
        } else {
            let number = [15, 17, 18, 19, 20, 21, 23, 24][function - 5];
            if modifiers != 0 {
                format!("\x1b[{number};{modifier}~")
            } else {
                format!("\x1b[{number}~")
            }
        }
        .into_bytes());
    }
    let bytes: &[u8] = match name {
        "Enter" => b"\r",
        "Backspace" => b"\x7f",
        "Escape" => b"\x1b",
        "Tab" if modifiers & 1 != 0 => b"\x1b[Z",
        "Tab" => b"\t",
        _ => bail!("unsupported terminal key"),
    };
    let mut result = Vec::new();
    if modifiers & 2 != 0 {
        result.push(27);
    }
    result.extend_from_slice(bytes);
    Ok(result)
}

#[cfg(unix)]
fn owned_child_finished(pid: u32) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    result == 0 && unsafe { info.si_pid() } == pid as libc::pid_t
}

#[cfg(unix)]
fn retire_owned_session(pid: u32) {
    signal_owned_session(pid, libc::SIGHUP);
    thread::sleep(Duration::from_millis(120));
    signal_owned_session(pid, libc::SIGTERM);
    thread::sleep(Duration::from_millis(120));
    signal_owned_session(pid, libc::SIGKILL);
}

#[cfg(unix)]
fn signal_owned_session(pid: u32, signal: i32) {
    let sid = pid as libc::pid_t;
    // The owned leader remains unreaped under the lifecycle lock. macOS may
    // reject getsid(leader) after its exit, while live descendants still retain
    // this SID. Keeping the leader unreaped prevents SID reuse during cleanup.
    #[cfg(target_os = "macos")]
    let candidates: Vec<libc::pid_t> = {
        unsafe extern "C" {
            fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut libc::c_void, size: i32)
                -> i32;
        }
        let mut candidates = vec![0; 65_536];
        let bytes = unsafe {
            proc_listpids(
                1,
                0,
                candidates.as_mut_ptr().cast(),
                (candidates.len() * std::mem::size_of::<libc::pid_t>()) as i32,
            )
        };
        candidates.truncate(bytes.max(0) as usize / std::mem::size_of::<libc::pid_t>());
        candidates
    };
    #[cfg(not(target_os = "macos"))]
    let candidates: Vec<libc::pid_t> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .take(65_536)
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    for candidate in candidates {
        if candidate > 1
            && candidate != unsafe { libc::getpid() }
            && unsafe { libc::getsid(candidate) } == sid
        {
            unsafe {
                libc::kill(candidate, signal);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_motion_preserves_captured_button_and_unpressed_move() {
        use vt100::{
            MouseProtocolEncoding::Sgr,
            MouseProtocolMode::{AnyMotion, ButtonMotion, PressRelease},
        };
        for (button, expected) in [
            (1, "\x1b[<32;3;2M"),
            (2, "\x1b[<33;3;2M"),
            (3, "\x1b[<34;3;2M"),
            (0, "\x1b[<35;3;2M"),
        ] {
            assert_eq!(
                encode_mouse_motion(AnyMotion, Sgr, 1, 2, button, 0).unwrap(),
                expected.as_bytes()
            );
            if button != 0 {
                assert_eq!(
                    encode_mouse_motion(ButtonMotion, Sgr, 1, 2, button, 0).unwrap(),
                    expected.as_bytes()
                );
            }
        }
        assert!(encode_mouse_motion(ButtonMotion, Sgr, 1, 2, 0, 0).is_none());
        assert!(encode_mouse_motion(PressRelease, Sgr, 1, 2, 1, 0).is_none());
        assert_eq!(
            encode_mouse_motion(AnyMotion, Sgr, 1, 2, 1, 5).unwrap(),
            b"\x1b[<52;3;2M"
        );
    }

    #[test]
    fn motion_keeps_existing_x10_and_utf8_encoding_rules() {
        use vt100::{
            MouseProtocolEncoding::{Default, Utf8},
            MouseProtocolMode::AnyMotion,
        };
        for encoding in [Default, Utf8] {
            assert_eq!(
                encode_mouse_motion(AnyMotion, encoding, 1, 2, 1, 0).unwrap(),
                b"\x1b[M@#\""
            );
            assert_eq!(
                encode_mouse_motion(AnyMotion, encoding, 1, 2, 0, 0).unwrap(),
                b"\x1b[MC#\""
            );
        }
        assert_eq!(
            encode_mouse_payload(Default, 1, 2, 0, false, 1),
            b"\x1b[M'#\""
        );
    }

    #[test]
    fn portable_dsr_accepts_fragmented_queries_and_reports_one_based_cursor() {
        let mut parser =
            PortableParser::new_with_callbacks(12, 40, 0, PortableCallbacks::default());
        parser.process(b"\x1b[4;9H\x1b[");
        assert!(parser.callbacks().replies.is_empty());
        parser.process(b"6");
        assert!(parser.callbacks().replies.is_empty());
        parser.process(b"n");
        assert_eq!(parser.callbacks().replies, b"\x1b[4;9R");
        parser.callbacks_mut().replies.clear();
        parser.process(b"\x1b[2;3H\x1b[6n\x1b[7;11H\x1b[6n\x1b[5n");
        assert_eq!(parser.callbacks().replies, b"\x1b[2;3R\x1b[7;11R\x1b[0n");
        assert_eq!(parser.callbacks().cursor_reports, 3);
    }

    #[test]
    fn portable_dsr_ignores_osc_payloads_and_other_csi_parameters() {
        let mut parser =
            PortableParser::new_with_callbacks(12, 40, 0, PortableCallbacks::default());
        parser.process(b"\x1b]0;title containing [6n and 6n\x07");
        parser.process(b"\x1b]8;;https://example.invalid/[6n\x1b\\link\x1b]8;;\x1b\\");
        parser.process(b"\x1b[?6n\x1b[>6n\x1b[6;1n\x1b[6:1n\x1b[5;1n");
        assert!(parser.callbacks().replies.is_empty());
        assert_eq!(parser.callbacks().cursor_reports, 0);
        parser.process(b"\x1b[6n");
        assert_eq!(parser.callbacks().replies, b"\x1b[1;5R");
    }

    #[test]
    fn portable_dsr_reply_buffer_is_bounded() {
        let mut parser =
            PortableParser::new_with_callbacks(12, 40, 0, PortableCallbacks::default());
        parser.process(&b"\x1b[6n".repeat(MAX_TERMINAL_REPLIES));
        assert!(parser.callbacks().overflow);
        assert!(parser.callbacks().replies.len() <= MAX_TERMINAL_REPLIES);
    }

    #[cfg(windows)]
    #[test]
    fn windows_portable_conpty_cursor_handshake_allows_cmd_echo_and_exit() {
        let shell = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is required"))
            .join("System32")
            .join("cmd.exe");
        assert!(shell.is_absolute() && shell.is_file());
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Portable,
                command: Some(vec![
                    shell.to_string_lossy().into_owned(),
                    "/d".into(),
                    "/q".into(),
                    "/c".into(),
                    "echo GLASS_CONPTY_DSR_READY & exit /b 0".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut exit = None;
        let mut output = String::new();
        while Instant::now() < deadline {
            for event in session.take_events() {
                match event {
                    SessionEvent::Exit(code) => exit = Some(code),
                    SessionEvent::Error(error) => panic!("ConPTY failed: {error}"),
                    _ => {}
                }
            }
            output = session.snapshot().plain_text();
            if output.contains("GLASS_CONPTY_DSR_READY") && exit.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let cursor_reports = session
            .parser
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .callbacks()
            .cursor_reports;
        session.close().unwrap();
        assert!(
            cursor_reports > 0,
            "ConPTY did not send the inherited-cursor query"
        );
        assert!(
            output.contains("GLASS_CONPTY_DSR_READY"),
            "CMD output missing: {output:?}"
        );
        assert_eq!(exit, Some(0));
        eprintln!("ConPTY cursor reports: {cursor_reports}; CMD echo observed; exit 0");
    }

    struct WriterDropSignal(mpsc::Sender<()>);

    impl Write for WriterDropSignal {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for WriterDropSignal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn idle_writer_stop_wakes_with_a_retained_session_sender() {
        for reason in ["close", "failure", "helper-exit"] {
            let shared = Arc::new(Shared {
                state: Mutex::new(State {
                    screen: Screen::new(10, 4),
                    palette: TerminalPalette::default(),
                    events: VecDeque::new(),
                    exit_seen: false,
                }),
                pending: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                writer_stop: Mutex::new(None),
                reader_done: AtomicBool::new(false),
                queued_bytes: AtomicUsize::new(0),
                output_revision: AtomicUsize::new(0),
                shell_identity: Mutex::new(None),
                shell_ready: Condvar::new(),
                wake: Arc::new(|| {}),
            });
            let (dropped, observed) = mpsc::channel();
            let sender = spawn_writer(WriterDropSignal(dropped), shared.clone());
            assert!(matches!(
                observed.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            match reason {
                "close" => shared.stop(),
                "failure" => shared.fail("owned writer stop fixture"),
                _ => apply_helper_event(&shared, &serde_json::json!({"type":"exit","code":0}), 0)
                    .unwrap(),
            }
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("stopping must wake the idle writer even while Session retains its sender");
            assert!(shared.stopping.load(Ordering::Acquire));
            assert!(shared.writer_stop.lock().unwrap().is_none());
            drop(sender);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_idle_child_close_does_not_hold_the_lifecycle_lock() {
        for backend in [Backend::Helper, Backend::Portable] {
            let mut session = Session::spawn(
                SessionOptions {
                    backend,
                    command: Some(vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf '__idle_wait_ready__\n'; exec /bin/sleep 3".into(),
                    ]),
                    ..SessionOptions::default()
                },
                Arc::new(|| {}),
            )
            .unwrap();
            wait_for_text(&session, "__idle_wait_ready__");
            // This fixture also exits on its own if a lifecycle-lock regression blocks close.
            let deadline = Instant::now() + Duration::from_millis(500);
            let lock_available = loop {
                if let Ok(process) = session.child.try_lock() {
                    drop(process);
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                thread::sleep(Duration::from_millis(1));
            };
            assert!(
                lock_available,
                "exit notification held the child lifecycle lock"
            );
            let started = Instant::now();
            session.close().unwrap();
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(session.child.lock().unwrap().exited);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_portable_exit_259_is_reported_after_the_process_is_signaled() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Portable,
                command: Some(vec![
                    default_shell(None).unwrap(),
                    "/d".into(),
                    "/c".into(),
                    "exit 259".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exit_code = None;
        while exit_code.is_none() && Instant::now() < deadline {
            for event in session.take_events() {
                match event {
                    SessionEvent::Exit(code) => exit_code = Some(code),
                    SessionEvent::Error(error) => panic!("terminal process failed: {error}"),
                    _ => {}
                }
            }
            if exit_code.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(exit_code, Some(259));
        assert!(session.child.lock().unwrap().exited);
        session.close().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn portable_unchanged_palette_preserves_revision_and_wake() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let observed_wakes = wakes.clone();
        let palette = TerminalPalette::default();
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Portable,
                command: Some(vec![if cfg!(target_os = "android") {
                    "/system/bin/cat"
                } else {
                    "/bin/cat"
                }
                .into()]),
                palette: palette.clone(),
                ..SessionOptions::default()
            },
            Arc::new(move || {
                observed_wakes.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .unwrap();
        let _ = session.take_events();
        let revision = session.output_revision();
        let notifications = wakes.load(Ordering::Relaxed);
        session.set_palette(palette.clone()).unwrap();
        assert_eq!(session.output_revision(), revision);
        assert_eq!(wakes.load(Ordering::Relaxed), notifications);
        session.resize(91, 27).unwrap();
        assert_eq!(session.output_revision(), revision + 1);
        assert_eq!(wakes.load(Ordering::Relaxed), notifications + 1);
        let screen = session.snapshot();
        assert_eq!((screen.cols, screen.rows), (91, 27));
        session.close().unwrap();
        assert!(session.set_palette(palette).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_exit_stops_input_and_drains_final_screen() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf 'FINAL_DRAIN_MARKER\\n'; exit 7".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session
            .take_events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Exit(7)))
        {
            assert!(
                Instant::now() < deadline,
                "helper exit event was not delivered"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(session.shared.stopping.load(Ordering::Acquire));
        assert!(session
            .snapshot()
            .plain_text()
            .contains("FINAL_DRAIN_MARKER"));
        assert!(session.input(b"ignored").is_err());
        assert!(session.set_palette(TerminalPalette::default()).is_err());
        while !session.shared.reader_done.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "helper reader did not finish draining"
            );
            thread::sleep(Duration::from_millis(10));
        }
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_hup_with_live_child_remains_idle_and_exits() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec </dev/null >/dev/null 2>&1; sleep 2".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let helper_pid = loop {
            if let Some(pid) = session.take_events().iter().find_map(|event| match event {
                SessionEvent::Ready { helper_pid, .. } => *helper_pid,
                _ => None,
            }) {
                break pid;
            }
            assert!(Instant::now() < deadline, "helper did not become ready");
            thread::sleep(Duration::from_millis(10));
        };
        let cpu_time = || {
            let output = Command::new("/bin/ps")
                .args(["-o", "time=", "-p", &helper_pid.to_string()])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "owned helper exited before the sampling window"
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .split(':')
                .fold(0.0_f64, |seconds, component| {
                    seconds * 60.0 + component.parse::<f64>().unwrap()
                })
        };
        let before = cpu_time();
        thread::sleep(Duration::from_secs(1));
        let consumed = cpu_time() - before;
        assert!(
            consumed < 0.1,
            "closed PTY consumed {consumed:.3}s CPU in one second"
        );
        while !session
            .take_events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Exit(0)))
        {
            assert!(
                Instant::now() < deadline,
                "helper did not retire its naturally exited child"
            );
            thread::sleep(Duration::from_millis(10));
        }
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn configured_home_zdotdir_and_shell_work_on_both_backends() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("glass-config-home-{}-{nonce}", std::process::id()));
        let home = directory.join("home");
        let startup = directory.join("startup");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(&startup).unwrap();
        std::fs::write(
            startup.join(".zshrc"),
            "export GLASS_CONFIG_STARTUP=fixture_loaded\n",
        )
        .unwrap();
        for backend in [Backend::Helper, Backend::Portable] {
            let mut session = Session::spawn(
                SessionOptions {
                    backend,
                    shell: Some("/bin/zsh".into()),
                    login: Some(false),
                    environment: BTreeMap::from([
                        ("HOME".into(), home.to_str().unwrap().into()),
                        ("ZDOTDIR".into(), startup.to_str().unwrap().into()),
                        ("SHELL".into(), "/bin/sh".into()),
                    ]),
                    ..SessionOptions::default()
                },
                Arc::new(|| {}),
            )
            .unwrap();
            session
                .input(b"stty -echo; printf '\\nCONFIG_%s\\n' INPUT_READY\r")
                .unwrap();
            wait_for_text(&session, "CONFIG_INPUT_READY");
            session.input(b"[[ \"$PWD\" -ef \"$HOME\" ]] && print CONFIG_HOME_CWD_OK; [[ \"$SHELL\" == /bin/zsh ]] && print CONFIG_SELECTED_SHELL_OK; [[ \"$GLASS_CONFIG_STARTUP\" == fixture_loaded ]] && print CONFIG_STARTUP_OK; (( ${preexec_functions[(Ie)__glass_command_start]} > 0 )) && print CONFIG_INTEGRATION_OK\r").unwrap();
            for marker in [
                "CONFIG_HOME_CWD_OK",
                "CONFIG_SELECTED_SHELL_OK",
                "CONFIG_STARTUP_OK",
                "CONFIG_INTEGRATION_OK",
            ] {
                wait_for_text(&session, marker);
            }
            session.close().unwrap();
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn configured_shell_environment_arguments_and_palette_work_on_both_backends() {
        for backend in [Backend::Helper, Backend::Portable] {
            let mut palette = TerminalPalette {
                foreground: [160, 170, 180],
                background: [3, 4, 5],
                ..TerminalPalette::default()
            };
            palette.ansi[1] = [1, 2, 3];
            let mut session = Session::spawn(
                SessionOptions {
                    backend,
                    shell: Some("/bin/sh".into()),
                    shell_args: vec!["-e".into()],
                    login: Some(false),
                    environment: BTreeMap::from([(
                        "GLASS_CONFIG_MARKER".into(),
                        "literal $(false); 中文".into(),
                    )]),
                    palette: palette.clone(),
                    ..SessionOptions::default()
                },
                Arc::new(|| {}),
            )
            .unwrap();
            session.input(b"stty -echo; printf '\\033[2J\\033[HD\\033[31mR\\033[38;2;160;170;180mT\\033[0m\\n'; printf 'ENV=[%s]\\n' \"$GLASS_CONFIG_MARKER\"; case \"$-\" in *e*) printf 'ARG_E_OK\\n';; esac\r").unwrap();
            wait_for_text(&session, "ENV=[literal $(false); 中文]");
            wait_for_text(&session, "ARG_E_OK");
            let screen = session.snapshot();
            let first = &screen.lines[0].runs;
            assert_eq!(first[0].fg, palette.foreground);
            assert!(first[0].fg_default);
            assert_eq!(first[1].fg, [1, 2, 3]);
            assert_eq!(first[1].fg_index, Some(1));
            assert_eq!(first[2].fg, [160, 170, 180]);
            assert!(!first[2].fg_default);
            assert_eq!(first[2].fg_index, None);
            palette.foreground = [200, 210, 220];
            palette.ansi[1] = [5, 6, 7];
            session.set_palette(palette.clone()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let screen = session.snapshot();
                if screen.lines[0].runs[0].fg == palette.foreground {
                    assert_eq!(screen.lines[0].runs[1].fg, [5, 6, 7]);
                    assert_eq!(screen.lines[0].runs[2].fg, [160, 170, 180]);
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Palette did not update for {backend:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
            session.close().unwrap();
            assert!(session.set_palette(palette).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_login_preserves_user_startup_files_and_zdotdir() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("glass-zsh-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let original = directory.join("original");
        std::fs::create_dir(&original).unwrap();
        for (name, contents) in [
            (".zshenv", "source \"$ZDOTDIR/fragment.zsh\"\n"),
            ("fragment.zsh", "export GLASS_TEST_SEQUENCE=ENV\n"),
            (
                ".zprofile",
                "export GLASS_TEST_SEQUENCE=${GLASS_TEST_SEQUENCE}_PROFILE\n",
            ),
            (
                ".zshrc",
                "export GLASS_TEST_SEQUENCE=${GLASS_TEST_SEQUENCE}_RC\n",
            ),
            (
                ".zlogin",
                "export GLASS_TEST_SEQUENCE=${GLASS_TEST_SEQUENCE}_LOGIN\n",
            ),
        ] {
            std::fs::write(original.join(name), contents).unwrap();
        }
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::helper_login_environment_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("HOME", &directory)
            .env("ZDOTDIR", &original)
            .env("SHELL", "/bin/zsh")
            .env("GLASS_STARTUP_FIXTURE", "1")
            .output()
            .unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "isolated environment subprocess, invoked by helper_login_preserves_user_startup_files_and_zdotdir"]
    fn helper_login_environment_fixture() {
        assert_eq!(std::env::var("GLASS_STARTUP_FIXTURE").unwrap(), "1");
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        session
            .input(b"printf '\\nSTARTUP_%s\\n' \"$GLASS_TEST_SEQUENCE\"\r")
            .unwrap();
        wait_for_text(&session, "STARTUP_ENV_PROFILE_RC_LOGIN");
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_exec_uses_structured_argv_without_shell_interpretation() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                command: Some(vec![
                    "/usr/bin/printf".into(),
                    "%s".into(),
                    "ARGV_中文; $(false)".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for_text(&session, "ARGV_中文; $(false)");
        session.close().unwrap();
    }

    #[test]
    fn portable_projection_keeps_osc8_targets_without_per_cell_uri_copies() {
        let mut parser = vt100::Parser::new(3, 40, 10);
        parser.process(b"\x1b]8;;file:///owned/note.txt\x1b");
        parser.process("\\LINK中\x1b[1mB\x1b]8;;\x07 plain".as_bytes());
        let screen = project_screen(parser.screen(), &TerminalPalette::default());
        let linked = screen.lines[0]
            .runs
            .iter()
            .filter(|run| run.hyperlink.is_some())
            .collect::<Vec<_>>();
        assert_eq!(linked.len(), 2);
        assert_eq!(linked[0].text, "LINK中");
        assert_eq!(linked[0].width, 6);
        assert_eq!(
            linked[0].hyperlink.as_deref(),
            Some("file:///owned/note.txt")
        );
        assert_eq!(linked[1].text, "B");
        assert!(linked[1].bold);
        assert!(screen.lines[0]
            .runs
            .iter()
            .any(|run| run.text == "p" && run.hyperlink.is_none()));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn helper_sgr_motion_keeps_left_middle_right_and_unpressed_codes() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                shell: Some("/bin/sh".into()),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        session.input(b"stty -echo -icanon min 1 time 0; printf '\\033[?1002h\\033[?1006h'; dd bs=1 count=84 2>/dev/null | od -An -tx1; printf '\\033[?1002l\\033[?1003h'; dd bs=1 count=10 2>/dev/null | od -An -tx1; stty sane; printf '\\033[?1003l\\033[?1006l__MOUSE_MOTION_%s__\\n' DONE\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.snapshot().mouse_mode != 2 {
            assert!(
                Instant::now() < deadline,
                "helper did not enable button motion"
            );
            thread::sleep(Duration::from_millis(10));
        }
        for button in 1..=3 {
            session.mouse(1, 2, button, true, 0).unwrap();
            session.mouse_motion(2, 3, button, 0).unwrap();
            session.mouse(2, 3, button, false, 0).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.snapshot().mouse_mode != 3 {
            assert!(
                Instant::now() < deadline,
                "helper did not enable unpressed motion"
            );
            thread::sleep(Duration::from_millis(10));
        }
        session.mouse_move(3, 4, 0).unwrap();
        wait_for_text(&session, "__MOUSE_MOTION_DONE__");
        let text = session
            .snapshot()
            .plain_text()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "1b 5b 3c 33 32 3b 34 3b 33 4d",
            "1b 5b 3c 33 33 3b 34 3b 33 4d",
            "1b 5b 3c 33 34 3b 34 3b 33 4d",
            "1b 5b 3c 33 35 3b 35 3b 34 4d",
        ] {
            assert!(
                text.contains(expected),
                "missing SGR motion {expected}: {text}"
            );
        }
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_preserves_osc8_links_and_reports_sgr_mouse_only_when_enabled() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                shell: Some("/bin/sh".into()),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        session.input(b"stty -echo; printf '\\033[2J\\033[H\\033]8;;https://example.org/path\\007LINK\\033]8;;\\007 plain\\n'\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = session.snapshot();
            if screen.lines.iter().flat_map(|line| &line.runs).any(|run| {
                run.text.contains("LINK")
                    && run.hyperlink.as_deref() == Some("https://example.org/path")
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "OSC8 link metadata missing: {screen:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(session
            .snapshot()
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .any(|run| run.text.contains("plain") && run.hyperlink.is_none()));
        session.input(b"stty -icanon min 1 time 0; printf '\\033[?1000h\\033[?1006h'; dd bs=1 count=9 2>/dev/null | od -An -tx1; stty sane; printf '\\033[?1000l\\033[?1006l__MOUSE_%s__\\n' DONE\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.snapshot().mouse_mode != 1 {
            assert!(
                Instant::now() < deadline,
                "application did not enable mouse tracking"
            );
            thread::sleep(Duration::from_millis(10));
        }
        session.mouse(1, 2, 1, true, 0).unwrap();
        wait_for_text(&session, "__MOUSE_DONE__");
        let words = session
            .snapshot()
            .plain_text()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            words.contains("1b 5b 3c 30 3b 33 3b 32 4d"),
            "incorrect SGR mouse payload: {words}"
        );
        assert_eq!(session.snapshot().mouse_mode, 0);
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_osc8_survives_resize_and_native_reflow() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                cols: 80,
                rows: 4,
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r"printf '\033[2J\033[H\033]8;;https://example.org/a\007LINK_A\033]8;;\007\r\n\033]8;;https://example.org/b\007LINK_B\033]8;;\007'; read stop".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for_text(&session, "LINK_B");
        for (cols, rows) in [(80, 5), (12, 5)] {
            session.resize(cols, rows).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let screen = session.snapshot();
                if (screen.cols, screen.rows) == (cols, rows) {
                    for (caption, uri) in [
                        ("LINK_A", "https://example.org/a"),
                        ("LINK_B", "https://example.org/b"),
                    ] {
                        assert!(
                            screen.lines.iter().flat_map(|line| &line.runs).any(|run| {
                                run.text.contains(caption) && run.hyperlink.as_deref() == Some(uri)
                            }),
                            "resize {cols}x{rows} lost {caption} metadata: {screen:?}"
                        );
                    }
                    break;
                }
                assert!(Instant::now() < deadline, "PTY resize was not projected");
                thread::sleep(Duration::from_millis(10));
            }
        }
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_osc8_survives_reverse_screen_repaint() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r"printf '\033[2J\033[H\033]8;;https://example.org/repaint\007REPAINT_LINK\033]8;;\007\033[?5h'; read stop".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for_text(&session, "REPAINT_LINK");
        let screen = session.snapshot();
        assert!(
            screen.lines.iter().flat_map(|line| &line.runs).any(|run| {
                run.text.contains("REPAINT_LINK")
                    && run.hyperlink.as_deref() == Some("https://example.org/repaint")
                    && run.inverse
            }),
            "reverse-screen repaint changed the URI metadata: {screen:?}"
        );
        session.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_osc8_multiline_scroll_keeps_each_rows_uri() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                cols: 80,
                rows: 4,
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r"printf '\033[2J\033[H\033]8;;https://example.org/first\007FIRST_LINK\033]8;;\007\r\n\033]8;;https://example.org/second\007SECOND_LINK\033]8;;\007\033[2S'; read stop".into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = session.snapshot();
            let complete = [
                ("FIRST_LINK", "https://example.org/first"),
                ("SECOND_LINK", "https://example.org/second"),
            ]
            .iter()
            .all(|(caption, uri)| {
                screen
                    .history
                    .iter()
                    .flat_map(|line| &line.runs)
                    .any(|run| {
                        run.text.contains(*caption) && run.hyperlink.as_deref() == Some(*uri)
                    })
            });
            if complete {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "multi-row scroll mismatched URI metadata: {screen:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        session.close().unwrap();
    }

    #[test]
    fn portable_projection_retains_sgr9_and_clears_29_and_reset() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"\x1b[9mX \x1b[29mY\x1b[9mZ\x1b[0mQ");
        let screen = project_screen(parser.screen(), &TerminalPalette::default());
        let styles: Vec<_> = screen.lines[0]
            .runs
            .iter()
            .take(5)
            .map(|run| run.strike)
            .collect();
        assert_eq!(styles, [true, true, false, true, false]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_history_reflow_preserves_wide_combining_style_and_link() {
        let expected = "REFLOW_中文e\u{301}_abcdefghijklmnop_END";
        let uri = "https://example.org/reflow";
        for backend in [Backend::Helper, Backend::Portable] {
            let script = format!("printf '\\033[31m\\033]8;;{uri}\\007{expected}\\033]8;;\\007\\033[0m\\n\\n\\n\\n\\nREFLOW_READY\\n'; read -r stop");
            let mut session = Session::spawn(
                SessionOptions {
                    backend,
                    cols: 12,
                    rows: 4,
                    command: Some(vec!["/bin/sh".into(), "-c".into(), script]),
                    ..SessionOptions::default()
                },
                Arc::new(|| {}),
            )
            .unwrap();
            wait_for_text(&session, "REFLOW_READY");
            let original = session.snapshot();
            assert!(
                original.history.iter().any(|line| line.wrapped),
                "{backend:?} lost source soft wrapping"
            );
            assert!(
                original
                    .history
                    .iter()
                    .all(|line| line.used_columns.is_some()),
                "{backend:?} lost used-column metadata"
            );
            let color = original
                .history
                .iter()
                .flat_map(|line| &line.runs)
                .find(|run| run.hyperlink.as_deref() == Some(uri))
                .unwrap()
                .fg;
            for cols in [10, 20, 12] {
                session.resize(cols, 4).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let screen = session.snapshot();
                    let linked: String = screen
                        .history
                        .iter()
                        .chain(&screen.lines)
                        .flat_map(|line| &line.runs)
                        .filter(|run| run.hyperlink.as_deref() == Some(uri))
                        .map(|run| run.text.as_str())
                        .collect();
                    if screen.cols == cols && linked == expected {
                        assert!(screen
                            .history
                            .iter()
                            .chain(&screen.lines)
                            .flat_map(|line| &line.runs)
                            .filter(|run| run.hyperlink.as_deref() == Some(uri))
                            .all(|run| run.fg == color));
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{backend:?} reflow cols={cols} lost linked UTF-8 content: {linked:?}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            }
            session.close().unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_high_volume_history_transport_keeps_every_row_below_retention_limit() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Helper,
                cols: 80,
                rows: 24,
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r#"index=0; while [ "$index" -lt 1000 ]; do printf 'STRESS_ROW_%04d\n' "$index"; index=$((index + 1)); done; printf 'STRESS_READY_%s\n' DONE; read stop"#.into(),
                ]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for_text(&session, "STRESS_READY_DONE");
        let screen = session.snapshot();
        let rows: std::collections::BTreeSet<String> = screen
            .history
            .iter()
            .chain(&screen.lines)
            .map(|line| {
                line.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end_matches(' ')
                    .to_owned()
            })
            .collect();
        for index in 0..1000 {
            let row = format!("STRESS_ROW_{index:04}");
            assert!(
                rows.contains(&row),
                "history transport lost {row}, received {} distinct rows",
                rows.len()
            );
        }
        assert_eq!(
            screen.history_dropped, 0,
            "output fits within configured history retention"
        );
        session.close().unwrap();
    }

    #[test]
    fn mode_aware_keys_and_modifiers() {
        assert_eq!(encode_key("Up", 0, true).unwrap(), b"\x1bOA");
        assert_eq!(encode_key("Left", 4, false).unwrap(), b"\x1b[1;5D");
        assert_eq!(encode_key("Tab", 1, false).unwrap(), b"\x1b[Z");
        assert!(encode_key("unexpected", 0, false).is_err());
    }

    #[test]
    fn portable_grid_preserves_wide_combining_and_background() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/portable_ansi_cjk_alternate.json"
        ))
        .unwrap();
        let mut parser = vt100::Parser::new(4, 10, 0);
        for step in fixture["steps"].as_array().unwrap() {
            let name = step["name"].as_str().unwrap();
            parser.process(step["input"].as_str().unwrap().as_bytes());
            let screen = project_screen(parser.screen(), &TerminalPalette::default());
            assert_eq!(
                serde_json::to_value(&screen).unwrap(),
                fixture["screens"][name],
                "{name}"
            );
        }
    }

    #[test]
    fn portable_history_projection_preserves_live_parser_offset() {
        let mut parser = vt100::Parser::new(4, 20, 2000);
        for index in 0..20 {
            parser.process(format!("line{index}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(3);
        let screen = project_parser(&mut parser, &TerminalPalette::default());
        assert_eq!(parser.screen().scrollback(), 3);
        assert!(!screen.history.is_empty());
        assert!(screen
            .history
            .front()
            .unwrap()
            .runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
            .starts_with("line0"));
        assert!(!screen.plain_text().contains("line0\n"));
        assert!(screen
            .viewport(screen.history.len())
            .plain_text()
            .starts_with("line0"));
        parser.process(b"\x1b[?1049hALTERNATE");
        let alternate = project_parser(&mut parser, &TerminalPalette::default());
        assert!(alternate.alternate);
        assert!(alternate.plain_text().contains("ALTERNATE"));
        assert_eq!(parser.screen().scrollback(), 0);
        parser.process(b"\x1b[?1049l");
        let restored = project_parser(&mut parser, &TerminalPalette::default());
        assert!(!restored.history.is_empty());
    }

    #[cfg(unix)]
    fn wait_for_text(session: &Session, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if session.snapshot().plain_text().contains(expected) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "missing terminal output {expected:?}: {}",
            session.snapshot().plain_text()
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_shell_input_resize_interrupt_and_exit() {
        for backend in [Backend::Helper, Backend::Portable] {
            if backend == Backend::Helper && helper_path().is_err() {
                continue;
            }
            let mut session = Session::spawn(
                SessionOptions {
                    backend,
                    shell: Some("/bin/sh".into()),
                    ..SessionOptions::default()
                },
                Arc::new(|| {}),
            )
            .unwrap();
            session
                .input(b"stty -echo; printf '__session_%s__\\n' ready\r")
                .unwrap();
            wait_for_text(&session, "__session_ready__");
            session.resize(91, 27).unwrap();
            session
                .input(b"stty size; printf '__size_%s__\\n' done\r")
                .unwrap();
            wait_for_text(&session, "__size_done__");
            assert!(session.snapshot().plain_text().contains("27 91"));
            session.input(b"sleep 30\r").unwrap();
            thread::sleep(Duration::from_millis(80));
            session.input(&[3]).unwrap();
            session
                .input("printf '__utf8_%s__\\n' 中文e\u{301}; exit 7\r".as_bytes())
                .unwrap();
            wait_for_text(&session, "__utf8_中文e\u{301}__");
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut exited = false;
            while Instant::now() < deadline {
                if session
                    .take_events()
                    .iter()
                    .any(|event| matches!(event, SessionEvent::Exit(7)))
                {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(exited, "shell exit code was not propagated for {backend:?}");
            session.close().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_shell_history_round_trip_and_helper_clear() {
        for backend in [Backend::Helper, Backend::Portable] {
            if backend == Backend::Helper && helper_path().is_err() {
                continue;
            }
            let mut session = Session::spawn(
                SessionOptions {
                    backend,
                    shell: Some("/bin/sh".into()),
                    cols: 40,
                    rows: 4,
                    command: None,
                    ..SessionOptions::default()
                },
                Arc::new(|| {}),
            )
            .unwrap();
            session
                .input(b"stty -echo; printf '__history_%s__\\n' ready\r")
                .unwrap();
            wait_for_text(&session, "__history_ready__");
            session.input(b"i=0; while [ $i -lt 30 ]; do printf '__line_%s__\\n' \"$i\"; i=$((i+1)); done; printf '__history_%s__\\n' done\r").unwrap();
            wait_for_text(&session, "__history_done__");
            let snapshot = session.snapshot();
            assert!(!snapshot.plain_text().contains("__line_0__"));
            let index = snapshot
                .history
                .iter()
                .position(|line| {
                    line.runs
                        .iter()
                        .map(|run| run.text.as_str())
                        .collect::<String>()
                        .contains("__line_0__")
                })
                .expect("old output must be retained in history");
            let view = snapshot.viewport(snapshot.history.len() - index);
            assert!(view
                .plain_text()
                .lines()
                .next()
                .unwrap()
                .contains("__line_0__"));
            assert!(!view.cursor.visible);
            if backend == Backend::Helper {
                session
                    .input(b"printf '\\033[3J'; printf '__history_%s__\\n' clear\r")
                    .unwrap();
                wait_for_text(&session, "__history_clear__");
                assert!(session.snapshot().history.len() < 3);
            }
            session.close().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn portable_close_retires_owned_background_job() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Portable,
                shell: Some("/bin/sh".into()),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        session
            .input(b"set +H\rstty -echo; sleep 30 & printf '__owned_%s__\\n' \"$!\"\r")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            let text = session.snapshot().plain_text();
            if let Some(pid) = text.lines().find_map(|line| {
                line.strip_prefix("__owned_")?
                    .strip_suffix("__")?
                    .parse::<i32>()
                    .ok()
            }) {
                break pid;
            }
            assert!(Instant::now() < deadline, "background PID missing: {text}");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        let start = Instant::now();
        session.close().unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "owned background process survived close"
        );
    }

    #[cfg(unix)]
    #[test]
    fn portable_exit_drains_output_and_retires_background_job() {
        let mut session = Session::spawn(
            SessionOptions {
                backend: Backend::Portable,
                shell: Some("/bin/sh".into()),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        session
            .input(b"set +H\rstty -echo; sleep 30 & printf '__final_%s__\\n' \"$!\"; exit 9\r")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session
            .take_events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Exit(9)))
        {
            assert!(
                Instant::now() < deadline,
                "natural shell exit was not delivered"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let text = session.snapshot().plain_text();
        let pid = text
            .lines()
            .find_map(|line| {
                line.strip_prefix("__final_")?
                    .strip_suffix("__")?
                    .parse::<i32>()
                    .ok()
            })
            .expect("final PTY output must be readable before Exit");
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "background process {pid} survived natural shell exit: {:?}",
            Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "pid=,ppid=,stat=,comm="])
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        );
        session.close().unwrap();
    }
}

#[cfg(unix)]
fn prepare_shell_channel(
    options: &SessionOptions,
) -> Result<Option<aether_events::ShellChannel>> {
    match options.shell_integration {
        ShellIntegration::None => Ok(None),
        ShellIntegration::AetherV1 => {
            aether_events::ShellChannel::prepare(options.shell_event_dir.as_deref()).map(Some)
        }
    }
}

// FVPROV1 109174 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMjA1ZWQ1ZTNmNGJmNWI2YjFhMWZjYzNiODdmNDk3ZDI3MTg4Njc1MDM1YzBlZGIzOWZjNGFlMzJiOTNkOTEyNiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZWQzYmY5YTZmY2Y5ZTkyMjBjMzM2MzhlZTk0NTdkZjk2MTIxYTZjNmEzMzQ2NzA5MGY2NDZkYTQwMzIwNjMxMWRkN2Y2MzJhNDg0MzEwMWE0Njk4MjVkNjgwMTYyNThiMjk2YWMwNDUwNDRjZTgyZDlhNjczMWEyODMzM2JjMDkifV19XQ== FVPROV1-END
