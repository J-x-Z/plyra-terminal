use crate::{config::Settings, Options, Wake};
use anyhow::{Context, Result};
use session_manager::{Session, SessionEvent, SessionOptions};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use terminal_core::{Screen, TerminalPalette};
use terminal_render::Selection;
use winit::{event_loop::EventLoopProxy, keyboard::NamedKey};

const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ShellTask {
    pub id: u64,
    pub pgid: u32,
    pub state: String,
    pub display: String,
}

pub struct TerminalPane {
    pub id: u64,
    pub session: Option<Session>,
    #[cfg(target_os = "android")]
    pub waiting_for_arch: bool,
    pub shell_session_id: Option<String>,
    pub shell_tasks: BTreeMap<u64, ShellTask>,
    pub shell_completion: Option<serde_json::Value>,
    pub shell_values: std::collections::VecDeque<serde_json::Value>,
    pub live_screen: Screen,
    pub screen: Screen,
    pub scroll_offset: usize,
    pub selection: Option<Selection>,
    pub selection_anchor: Option<(u16, u16)>,
    pub preedit: String,
    pub preedit_cursor: Option<(usize, usize)>,
    pub status: Option<String>,
    pub rect: [f32; 4],
    pub grid: (u16, u16),
    pub wheel_remainder: f64,
    pub output_revision: u64,
    pub pending_palette: Option<(TerminalPalette, Instant)>,
    search_query: String,
    search_position: Option<(usize, u16)>,
}

impl TerminalPane {
    pub fn spawn(
        id: u64,
        command: Option<Vec<String>>,
        options: &Options,
        settings: &Settings,
        proxy: EventLoopProxy<Wake>,
    ) -> Result<Self> {
        #[cfg(target_os = "android")]
        let command = if command.is_none()
            && options.shell.is_none()
            && settings.shell.program.is_none()
            && crate::android_arch::default_enabled()
        {
            if let Some(command) = crate::android_arch::command()? {
                Some(command)
            } else {
                crate::android_arch::prepare()?;
                let mut pane = Self::empty(id);
                pane.waiting_for_arch = true;
                return Ok(pane);
            }
        } else {
            command
        };
        let profile = if command.is_some() {
            crate::config::ShellProfile {
                environment: settings.shell.environment.clone(),
                ..Default::default()
            }
        } else {
            settings
                .shell
                .resolved(options.shell.as_deref(), options.shell_integration)?
        };
        #[cfg(target_os = "android")]
        let shell_event_dir = if profile.integration == session_manager::ShellIntegration::AetherV1
        {
            Some(system_api::android_root::files_dir()?)
        } else {
            None
        };
        #[cfg(not(target_os = "android"))]
        let shell_event_dir = None;
        let session = Session::spawn(
            SessionOptions {
                backend: options.backend,
                cols: 80,
                rows: 24,
                shell: profile.program,
                command,
                shell_args: profile.args,
                shell_integration: profile.integration,
                shell_event_dir,
                native_presentation: profile.native_presentation.unwrap_or(true),
                login: profile.login,
                environment: {
                    #[cfg(target_os = "android")]
                    {
                        crate::android_input::session_environment(profile.environment.clone())?
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        profile.environment
                    }
                },
                palette: settings.palette(),
            },
            Arc::new(move || {
                let _ = proxy.send_event(Wake::Session);
            }),
        )?;
        let mut pane = Self::empty(id);
        pane.session = Some(session);
        Ok(pane)
    }

    fn empty(id: u64) -> Self {
        Self {
            id,
            session: None,
            #[cfg(target_os = "android")]
            waiting_for_arch: false,
            shell_session_id: None,
            shell_tasks: BTreeMap::new(),
            shell_completion: None,
            shell_values: std::collections::VecDeque::new(),
            live_screen: Screen::default(),
            screen: Screen::default(),
            scroll_offset: 0,
            selection: None,
            selection_anchor: None,
            preedit: String::new(),
            preedit_cursor: None,
            status: None,
            rect: [0.0; 4],
            grid: (80, 24),
            wheel_remainder: 0.0,
            output_revision: 0,
            pending_palette: None,
            search_query: String::new(),
            search_position: None,
        }
    }
    pub fn receive(&mut self, visible: bool) -> (Vec<SessionEvent>, bool) {
        let Some(session) = &self.session else {
            return (Vec::new(), false);
        };
        let events = session.take_events();
        self.track_shell_events(&events);
        (events, visible && self.sync_screen())
    }
    fn track_shell_events(&mut self, events: &[SessionEvent]) {
        for event in events {
            match event {
                SessionEvent::Shell { event, .. } => {
                    let identity = event.data["session_id"].as_str();
                    if event.event_type == "hello" {
                        self.shell_tasks.clear();
                        self.shell_completion = None;
                        self.shell_values.clear();
                        self.shell_session_id = identity.map(str::to_owned);
                    }
                    if self.shell_session_id.as_deref() != identity || identity.is_none() {
                        continue;
                    }
                    match event.event_type.as_str() {
                        "completion.snapshot" => {
                            self.shell_completion =
                                (event.data["active"] == true).then(|| event.data.clone());
                        }
                        "values.record" => {
                            self.shell_values.push_back(event.data.clone());
                            while self.shell_values.len() > 128 {
                                self.shell_values.pop_front();
                            }
                        }
                        "task.started" => {
                            if self.shell_tasks.len() < 4096 {
                                if let Some(task) = shell_task(&event.data) {
                                    self.shell_tasks.insert(task.id, task);
                                }
                            }
                        }
                        "task.state" => {
                            let Some(id) = event.task_id else { continue };
                            if event.data["state"] == "completed" {
                                self.shell_tasks.remove(&id);
                            } else if let Some(task) = self.shell_tasks.get_mut(&id) {
                                task.state =
                                    event.data["state"].as_str().unwrap_or("running").into();
                            }
                        }
                        "operation.result" => {
                            if let Some(jobs) = event.data["result"]["jobs"].as_array() {
                                self.shell_tasks.clear();
                                for job in jobs.iter().take(4096) {
                                    if job["session_id"].as_str() == identity {
                                        if let Some(task) = shell_task(job) {
                                            self.shell_tasks.insert(task.id, task);
                                        }
                                    }
                                }
                            }
                        }
                        "session.exiting" => self.shell_tasks.clear(),
                        _ => {}
                    }
                }
                SessionEvent::Exit(_) | SessionEvent::Error(_) | SessionEvent::ShellWarning(_) => {
                    self.shell_session_id = None;
                    self.shell_completion = None;
                    self.shell_tasks.clear();
                }
                SessionEvent::Ready { .. } | SessionEvent::Command { .. } => {}
            }
        }
    }
    pub fn sync_screen(&mut self) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let revision = session.output_revision();
        let Some(next) = self.changed_snapshot(revision, || session.snapshot()) else {
            return false;
        };
        self.output_revision = revision;
        if self.live_screen.alternate != next.alternate {
            self.search_position = None;
        }
        self.scroll_offset = anchored_offset(&self.live_screen, &next, self.scroll_offset);
        self.live_screen = next;
        self.clear_selection();
        self.refresh();
        true
    }
    fn changed_snapshot(&self, revision: u64, capture: impl FnOnce() -> Screen) -> Option<Screen> {
        (revision != self.output_revision).then(capture)
    }
    pub fn refresh(&mut self) {
        if self.live_screen.alternate {
            self.scroll_offset = 0;
        }
        self.scroll_offset = self.scroll_offset.min(self.live_screen.history.len());
        self.screen = self.live_screen.viewport(self.scroll_offset);
    }
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_anchor = None;
    }
    pub fn bottom(&mut self) {
        self.clear_selection();
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            self.refresh();
        }
    }
    pub fn scroll(&mut self, lines: i64) {
        if self.live_screen.alternate {
            return;
        }
        let amount = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        let next = if lines >= 0 {
            self.scroll_offset
                .saturating_add(amount)
                .min(self.live_screen.history.len())
        } else {
            self.scroll_offset.saturating_sub(amount)
        };
        if next != self.scroll_offset {
            self.clear_selection();
            self.scroll_offset = next;
            self.refresh();
        }
    }
    pub fn drag(&mut self, cell: (u16, u16)) {
        let Some(anchor) = self.selection_anchor else {
            return;
        };
        self.selection = Some(if cell >= anchor {
            Selection {
                start: anchor,
                end: (cell.0, cell.1.saturating_add(1).min(self.screen.cols)),
            }
        } else {
            Selection {
                start: cell,
                end: (anchor.0, anchor.1.saturating_add(1).min(self.screen.cols)),
            }
        });
    }
    pub fn selected_text(&self) -> Result<Option<String>> {
        self.selection
            .as_ref()
            .map(|selection| selected_text(&self.screen, selection))
            .transpose()
    }
    pub fn preedit(&mut self, text: String) -> bool {
        self.update_preedit(text, None)
    }
    pub fn update_preedit(&mut self, text: String, cursor: Option<(usize, usize)>) -> bool {
        if text.len() > 16_384 || (self.preedit == text && self.preedit_cursor == cursor) {
            return false;
        }
        if !text.is_empty() {
            self.bottom();
        }
        self.preedit = text;
        self.preedit_cursor = cursor;
        true
    }
    pub fn clear_preedit(&mut self) {
        self.preedit.clear();
        self.preedit_cursor = None;
    }
    pub fn commit(&mut self, text: &str) -> Result<()> {
        self.clear_preedit();
        self.bottom();
        self.session
            .as_mut()
            .context("No active Shell")?
            .input(text.as_bytes())
    }
    pub fn paste(&mut self, text: &str) -> Result<()> {
        validate_clipboard_text(text)?;
        self.clear_preedit();
        self.bottom();
        self.session
            .as_mut()
            .context("No active Shell")?
            .paste(text.as_bytes())
    }
    pub fn key(&mut self, name: &str, modifiers: u8) -> Result<()> {
        if !self.preedit.is_empty() {
            return Ok(());
        }
        self.bottom();
        let session = self.session.as_mut().context("No active Shell")?;
        if name.chars().count() == 1 {
            let mut bytes = Vec::with_capacity(name.len() + 1);
            if modifiers & 2 != 0 {
                bytes.push(27);
            }
            if modifiers & 4 != 0 {
                if let Some(byte) = control_byte(name) {
                    bytes.push(byte);
                }
            } else {
                bytes.extend_from_slice(name.as_bytes());
            }
            session.input(&bytes)
        } else {
            session.key(name, modifiers)
        }
    }
    pub fn resize(&mut self, grid: (u16, u16)) -> Result<()> {
        if grid != self.grid {
            self.clear_selection();
            if let Some(session) = &mut self.session {
                session.resize(grid.0, grid.1)?;
            }
            self.grid = grid;
        }
        Ok(())
    }
    pub fn close(&mut self) {
        self.pending_palette = None;
        self.shell_session_id = None;
        self.shell_completion = None;
        self.shell_values.clear();
        self.shell_tasks.clear();
        if let Some(mut session) = self.session.take() {
            if let Err(error) = session.close() {
                eprintln!("Pane {} close: {error:#}", self.id);
            }
        }
    }
    pub fn request_palette(&mut self, palette: TerminalPalette) -> Option<String> {
        self.pending_palette = Some((palette, Instant::now() + Duration::from_secs(3)));
        self.sync_palette()
    }
    pub fn sync_palette(&mut self) -> Option<String> {
        let Some((palette, deadline)) = &self.pending_palette else {
            return None;
        };
        let Some(session) = &mut self.session else {
            self.pending_palette = None;
            return None;
        };
        match session.set_palette(palette.clone()) {
            Ok(()) => self.pending_palette = None,
            Err(error) if Instant::now() >= *deadline => {
                self.pending_palette = None;
                return Some(format!(
                    "Pane {} palette query update failed · {error}. Reload configuration to retry.",
                    self.id
                ));
            }
            Err(_) => {}
        }
        None
    }
    pub fn find(&mut self, query: &str, forward: bool) -> Result<()> {
        if query.is_empty() {
            self.clear_selection();
            self.status = None;
            return Ok(());
        }
        anyhow::ensure!(query.len() <= 4096, "Search text exceeds 4 KiB");
        if self.search_query != query {
            self.search_query = query.into();
            self.search_position = None;
        }
        let history = if self.live_screen.alternate {
            0
        } else {
            self.live_screen.history.len()
        };
        let mut matches = Vec::new();
        for (row, line) in self
            .live_screen
            .history
            .iter()
            .take(history)
            .chain(self.live_screen.lines.iter())
            .enumerate()
        {
            let text: String = line.runs.iter().map(|run| run.text.as_str()).collect();
            for (start, matched) in text.match_indices(query) {
                let left = byte_column(line, start, false);
                let right = byte_column(line, start + matched.len(), true);
                matches.push((row, left, right));
            }
        }
        anyhow::ensure!(!matches.is_empty(), "No matches");
        let dropped = self.live_screen.history_dropped;
        let position = if forward {
            matches
                .iter()
                .position(|(row, col, _)| {
                    self.search_position
                        .is_none_or(|previous| (dropped.saturating_add(*row), *col) > previous)
                })
                .unwrap_or(0)
        } else {
            matches
                .iter()
                .rposition(|(row, col, _)| {
                    self.search_position
                        .is_none_or(|previous| (dropped.saturating_add(*row), *col) < previous)
                })
                .unwrap_or(matches.len() - 1)
        };
        let (row, left, right) = matches[position];
        self.search_position = Some((dropped.saturating_add(row), left));
        self.scroll_offset = history
            .saturating_add(usize::from(self.live_screen.rows) / 2)
            .saturating_sub(row)
            .min(history);
        self.refresh();
        let visible_row = row.saturating_sub(history.saturating_sub(self.scroll_offset));
        self.selection = Some(Selection {
            start: (visible_row as u16, left.min(self.screen.cols)),
            end: (visible_row as u16, right.min(self.screen.cols)),
        });
        self.selection_anchor = None;
        self.status = Some(format!("Match {} of {}", position + 1, matches.len()));
        Ok(())
    }

    pub fn object_at(&self, cell: (u16, u16)) -> Option<crate::OpenObject> {
        let line = self.screen.lines.get(usize::from(cell.0))?;
        let mut column = 0_u16;
        let mut clicked_byte = None;
        let mut offset = 0;
        for run in &line.runs {
            let end = column.saturating_add(run.width);
            if cell.1 >= column && cell.1 < end {
                if let Some(link) = &run.hyperlink {
                    return parse_object(link);
                }
                clicked_byte = Some(
                    offset
                        + if run.text.is_ascii() && run.text.len() == usize::from(run.width) {
                            usize::from(cell.1 - column)
                        } else {
                            0
                        },
                );
            }
            column = end;
            offset += run.text.len();
        }
        let text: String = line.runs.iter().map(|run| run.text.as_str()).collect();
        let clicked_byte = clicked_byte?;
        let mut offset = 0;
        for word in text.split_inclusive(char::is_whitespace) {
            let end = offset + word.len();
            if clicked_byte >= offset && clicked_byte < end {
                return parse_object(word.trim().trim_end_matches([',', ';']));
            }
            offset = end;
        }
        None
    }
    pub fn blank_at(&self, cell: (u16, u16)) -> bool {
        let Some(line) = self.screen.lines.get(usize::from(cell.0)) else {
            return true;
        };
        let mut column = 0;
        for run in &line.runs {
            let end = column + run.width;
            if cell.1 >= column && cell.1 < end {
                return if run.text.is_ascii() && run.text.len() == usize::from(run.width) {
                    run.text.as_bytes()[usize::from(cell.1 - column)] == b' '
                } else {
                    run.text.chars().all(char::is_whitespace)
                };
            }
            column = end;
        }
        true
    }
}

fn shell_task(value: &serde_json::Value) -> Option<ShellTask> {
    let id = value["task_id"].as_u64().filter(|id| *id > 0)?;
    let pgid = value["pgid"]
        .as_u64()
        .filter(|pid| *pid > 0 && *pid <= i32::MAX as u64)? as u32;
    let state = value["state"].as_str().unwrap_or("running");
    if !matches!(state, "running" | "stopped") {
        return None;
    }
    let display = value["display"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect();
    Some(ShellTask {
        id,
        pgid,
        state: state.into(),
        display,
    })
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        self.close();
    }
}

fn byte_column(line: &terminal_core::Line, byte: usize, end_boundary: bool) -> u16 {
    let mut offset = 0;
    let mut column = 0_u16;
    for run in &line.runs {
        let next = offset + run.text.len();
        if byte < next || (end_boundary && byte == next) {
            if run.text.is_ascii() && run.text.len() == usize::from(run.width) {
                return column.saturating_add(byte.saturating_sub(offset) as u16);
            }
            return if end_boundary {
                column.saturating_add(run.width)
            } else {
                column
            };
        }
        offset = next;
        column = column.saturating_add(run.width);
    }
    column
}

fn parse_object(text: &str) -> Option<crate::OpenObject> {
    if text.len() > 4096 || text.chars().any(char::is_control) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if let Some(value) = lower
        .strip_prefix("pid:")
        .or_else(|| lower.strip_prefix("pid="))
    {
        if let Ok(pid) = value.parse::<u32>() {
            if pid > 0 {
                return Some(crate::OpenObject::Process(pid));
            }
        }
    } else if let Ok(pid) = text.parse::<u32>() {
        if pid > 1 {
            return Some(crate::OpenObject::Process(pid));
        }
    }
    if (lower.starts_with("https://") && text.len() > 8)
        || (lower.starts_with("http://") && text.len() > 7)
    {
        return Some(crate::OpenObject::Url(text.into()));
    }
    let path = if let Some(uri) = text.strip_prefix("file://") {
        let uri = uri.strip_prefix("localhost").unwrap_or(uri);
        if !uri.starts_with('/') {
            return None;
        }
        let mut bytes = Vec::new();
        let source = uri.as_bytes();
        let mut index = 0;
        while index < source.len() {
            if source[index] == b'%' {
                let a = char::from(*source.get(index + 1)?).to_digit(16)?;
                let b = char::from(*source.get(index + 2)?).to_digit(16)?;
                bytes.push((a * 16 + b) as u8);
                index += 3;
            } else {
                bytes.push(source[index]);
                index += 1;
            }
        }
        std::path::PathBuf::from(String::from_utf8(bytes).ok()?)
    } else if let Some(relative) = text.strip_prefix("~/") {
        #[cfg(target_os = "android")]
        let home = system_api::android_root::files_dir().ok()?;
        #[cfg(not(target_os = "android"))]
        let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
        home.join(relative)
    } else {
        std::path::PathBuf::from(text)
    };
    if !path.is_absolute() {
        return None;
    }
    path.canonicalize().ok().map(crate::OpenObject::File)
}

#[cfg(test)]
mod feature_tests {
    use super::*;
    use terminal_core::{Line, Run};
    fn pane(screen: Screen) -> TerminalPane {
        let display = screen.viewport(0);
        TerminalPane {
            id: 1,
            session: None,
            #[cfg(target_os = "android")]
            waiting_for_arch: false,
            shell_session_id: None,
            shell_tasks: BTreeMap::new(),
            shell_completion: None,
            shell_values: std::collections::VecDeque::new(),
            live_screen: screen,
            screen: display,
            scroll_offset: 0,
            selection: None,
            selection_anchor: None,
            preedit: String::new(),
            preedit_cursor: None,
            status: None,
            rect: [0.0; 4],
            grid: (40, 4),
            wheel_remainder: 0.0,
            output_revision: 0,
            pending_palette: None,
            search_query: String::new(),
            search_position: None,
        }
    }
    fn run(text: &str, width: u16) -> Run {
        Run {
            text: text.into(),
            width,
            ..Run::default()
        }
    }
    fn shell_event(kind: &str, identity: &str, mut data: serde_json::Value) -> SessionEvent {
        data["session_id"] = identity.into();
        SessionEvent::Shell {
            at_ms: 1,
            event: session_manager::ShellEvent {
                sequence: 1,
                event_type: kind.into(),
                task_id: data["task_id"].as_u64(),
                data,
            },
        }
    }
    #[test]
    fn shell_task_actions_keep_session_identity_and_retire_completed_tasks() {
        use serde_json::json;
        let mut pane = pane(Screen::new(40, 4));
        pane.track_shell_events(&[
            shell_event("hello", "first", json!({})),
            shell_event(
                "task.started",
                "first",
                json!({"task_id":1,"pgid":200,"display":"sleep 20\n"}),
            ),
            SessionEvent::Ready {
                helper_pid: None,
                shell_pid: Some(199),
                shell: "aether-shell".into(),
            },
            shell_event(
                "task.started",
                "other",
                json!({"task_id":2,"pgid":300,"display":"foreign"}),
            ),
        ]);
        assert_eq!(pane.shell_tasks.len(), 1);
        assert_eq!(pane.shell_tasks[&1].display, "sleep 20");
        pane.track_shell_events(&[shell_event(
            "task.state",
            "first",
            json!({"task_id":1,"state":"completed"}),
        )]);
        assert!(pane.shell_tasks.is_empty());
        pane.track_shell_events(&[shell_event("operation.result", "first", json!({"result":{"jobs":[
            {"session_id":"first","task_id":2,"pgid":300,"state":"stopped","display":"sleep 30"},
            {"session_id":"first","task_id":3,"pgid":400,"state":"done","display":"finished"},
            {"session_id":"other","task_id":4,"pgid":500,"state":"running","display":"foreign"}
        ]}}))]);
        assert_eq!(pane.shell_tasks.len(), 1);
        assert_eq!(pane.shell_tasks[&2].state, "stopped");
        pane.track_shell_events(&[shell_event("hello", "second", json!({}))]);
        assert!(pane.shell_tasks.is_empty());
        assert_eq!(pane.shell_session_id.as_deref(), Some("second"));
        pane.track_shell_events(&[SessionEvent::Exit(0)]);
        assert_eq!(pane.shell_session_id, None);
    }
    #[test]
    fn preedit_updates_preserve_caret_changes_and_clear_composition_once() {
        let mut pane = pane(Screen::new(40, 4));
        assert!(!pane.update_preedit(String::new(), None));
        assert!(pane.update_preedit("中文".into(), Some((0, 3))));
        assert!(!pane.update_preedit("中文".into(), Some((0, 3))));
        assert!(pane.update_preedit("中文".into(), Some((3, 6))));
        assert!(pane.update_preedit(String::new(), None));
        assert!(!pane.update_preedit(String::new(), None));
        assert!(pane.update_preedit("候选".into(), Some((6, 6))));
        pane.clear_preedit();
        assert!(pane.preedit.is_empty());
        assert_eq!(pane.preedit_cursor, None);
    }
    #[test]
    fn unrelated_wake_preserves_selection_and_skips_snapshot_capture() {
        let mut pane = pane(Screen::new(40, 4));
        pane.output_revision = 7;
        let selection = Selection {
            start: (0, 0),
            end: (0, 3),
        };
        pane.selection = Some(selection);
        pane.preedit = "composition".into();
        assert!(pane
            .changed_snapshot(7, || panic!("unchanged pane must not clone its snapshot"))
            .is_none());
        assert_eq!(pane.selection, Some(selection));
        assert!(pane.changed_snapshot(8, || Screen::new(40, 4)).is_some());
        let (events, changed) = pane.receive(true);
        assert!(events.is_empty());
        assert!(!changed);
        assert_eq!(pane.selection, Some(selection));
        assert_eq!(pane.preedit, "composition");
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn hidden_receive_defers_snapshot_and_revision_until_first_visible_sync() {
        let session = Session::spawn(
            SessionOptions {
                backend: session_manager::Backend::Portable,
                command: Some(vec!["/bin/cat".into()]),
                ..SessionOptions::default()
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let mut pane = pane(Screen::default());
        pane.session = Some(session);
        let selection = Selection {
            start: (0, 0),
            end: (0, 3),
        };
        pane.selection = Some(selection);
        pane.preedit = "composition".into();
        pane.session
            .as_mut()
            .unwrap()
            .input(b"HIDDEN_FIRST\nHIDDEN_LATEST\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let latest_revision = loop {
            let session = pane.session.as_ref().unwrap();
            let revision = session.output_revision();
            if revision > 0 && session.snapshot().plain_text().contains("HIDDEN_LATEST") {
                break revision;
            }
            assert!(Instant::now() < deadline, "owned PTY output was not ready");
            std::thread::sleep(Duration::from_millis(10));
        };
        let (_, changed) = pane.receive(false);
        assert!(!changed);
        assert_eq!(pane.output_revision, 0);
        assert!(!pane.live_screen.plain_text().contains("HIDDEN_LATEST"));
        assert_eq!(pane.selection, Some(selection));
        assert_eq!(pane.preedit, "composition");
        assert!(pane.sync_screen());
        assert!(pane.live_screen.plain_text().contains("HIDDEN_LATEST"));
        assert!(pane.output_revision >= latest_revision);
        assert_eq!(pane.preedit, "composition");
        pane.close();
    }
    #[test]
    fn history_find_highlights_complete_wide_and_combining_cells() {
        let mut screen = Screen::new(40, 4);
        screen.history.push_back(Line {
            runs: vec![
                run("old ", 4),
                run("中", 2),
                run(" ", 1),
                run("e\u{301}", 1),
            ],
            ..Line::default()
        });
        let mut pane = pane(screen);
        pane.find("中", true).unwrap();
        assert_eq!(pane.scroll_offset, 1);
        assert_eq!(
            pane.selection.unwrap(),
            Selection {
                start: (0, 4),
                end: (0, 6)
            }
        );
        pane.find("\u{301}", true).unwrap();
        assert_eq!(
            pane.selection.unwrap(),
            Selection {
                start: (0, 7),
                end: (0, 8)
            }
        );
        assert_eq!(pane.selected_text().unwrap().unwrap(), "e\u{301}");
    }
    #[test]
    fn alternate_search_stays_on_the_alternate_screen() {
        let mut screen = Screen::new(40, 4);
        screen.history.push_back(Line {
            runs: vec![run("primary", 7)],
            ..Line::default()
        });
        screen.lines[0].runs = vec![run("alternate", 9)];
        screen.alternate = true;
        let mut pane = pane(screen);
        assert!(pane.find("primary", true).is_err());
        pane.find("alternate", true).unwrap();
        assert_eq!(pane.scroll_offset, 0);
    }
    #[test]
    fn blank_terminal_area_has_no_semantic_object() {
        let mut screen = Screen::new(40, 4);
        screen.lines[0].runs = vec![run("https://example.com", 19)];
        let pane = pane(screen);
        assert!(matches!(
            pane.object_at((0, 4)),
            Some(crate::OpenObject::Url(_))
        ));
        assert!(pane.object_at((0, 30)).is_none());
    }
    #[test]
    fn object_protocols_are_limited_before_os_dispatch() {
        assert!(matches!(
            parse_object("pid:1234"),
            Some(crate::OpenObject::Process(1234))
        ));
        assert!(matches!(
            parse_object("PID=1234"),
            Some(crate::OpenObject::Process(1234))
        ));
        assert!(matches!(
            parse_object("1234"),
            Some(crate::OpenObject::Process(1234))
        ));
        assert!(parse_object("pid:0").is_none());
        assert!(parse_object("javascript:alert(1)").is_none());
        assert!(parse_object("file://remote/tmp/a").is_none());
        assert!(parse_object("https://example.com\nother").is_none());
        assert!(matches!(
            parse_object("https://example.com"),
            Some(crate::OpenObject::Url(_))
        ));
    }
}

fn anchored_offset(previous: &Screen, next: &Screen, offset: usize) -> usize {
    if let Some(reflowed) = next.reflowed_offset_from(previous, offset) {
        return reflowed;
    }
    if offset == 0 || next.alternate || next.history_dropped < previous.history_dropped {
        return 0;
    }
    let previous_total = previous
        .history
        .len()
        .saturating_add(previous.history_dropped);
    let next_total = next.history.len().saturating_add(next.history_dropped);
    if next_total < previous_total {
        return 0;
    }
    offset
        .saturating_add(next_total - previous_total)
        .min(next.history.len())
}

fn validate_clipboard_text(text: &str) -> Result<()> {
    anyhow::ensure!(
        text.len() <= MAX_CLIPBOARD_BYTES,
        "Clipboard text exceeds 64 KiB"
    );
    Ok(())
}

fn selected_text(screen: &Screen, selection: &Selection) -> Result<String> {
    let clamp = |point: (u16, u16)| (point.0.min(screen.rows), point.1.min(screen.cols));
    let first = clamp(selection.start);
    let last = clamp(selection.end);
    let (start, end) = if first <= last {
        (first, last)
    } else {
        (last, first)
    };
    let mut text = String::new();
    for row in start.0..=end.0.min(screen.rows.saturating_sub(1)) {
        if row > start.0
            && !screen
                .lines
                .get(usize::from(row - 1))
                .is_some_and(|line| line.wrapped)
        {
            text.push('\n');
        }
        let left = if row == start.0 { start.1 } else { 0 };
        let right = if row == end.0 { end.1 } else { screen.cols };
        let mut line_text = String::new();
        let mut column = 0_u16;
        if let Some(line) = screen.lines.get(usize::from(row)) {
            for run in &line.runs {
                let next_column = column.saturating_add(run.width).min(screen.cols);
                let selected_start = column.max(left);
                let selected_end = next_column.min(right);
                if selected_start < selected_end {
                    // Helper ASCII runs merge adjacent cells. A Unicode run
                    // carries one terminal cell, including combining marks.
                    let piece = if run.text.is_ascii() && run.text.len() == usize::from(run.width) {
                        &run.text[usize::from(selected_start - column)
                            ..usize::from(selected_end - column)]
                    } else {
                        run.text.as_str()
                    };
                    anyhow::ensure!(
                        line_text.len().saturating_add(piece.len()) <= MAX_CLIPBOARD_BYTES,
                        "Clipboard text exceeds 64 KiB"
                    );
                    line_text.push_str(piece);
                }
                column = next_column;
            }
        }
        if right == screen.cols {
            line_text.truncate(line_text.trim_end_matches(' ').len());
        }
        anyhow::ensure!(
            text.len().saturating_add(line_text.len()) <= MAX_CLIPBOARD_BYTES,
            "Clipboard text exceeds 64 KiB"
        );
        text.push_str(&line_text);
    }
    validate_clipboard_text(&text)?;
    Ok(text)
}

pub(crate) fn control_byte(text: &str) -> Option<u8> {
    if text.len() != 1 {
        return None;
    }
    let byte = text.as_bytes()[0].to_ascii_uppercase();
    match byte {
        b'@'..=b'_' => Some(byte & 0x1f),
        b' ' => Some(0),
        b'?' => Some(127),
        _ => None,
    }
}

pub(crate) fn terminal_key(key: NamedKey) -> Option<&'static str> {
    match key {
        NamedKey::Space => Some(" "),
        NamedKey::Enter => Some("Enter"),
        NamedKey::Backspace => Some("Backspace"),
        NamedKey::Tab => Some("Tab"),
        NamedKey::Escape => Some("Escape"),
        NamedKey::ArrowUp => Some("Up"),
        NamedKey::ArrowDown => Some("Down"),
        NamedKey::ArrowLeft => Some("Left"),
        NamedKey::ArrowRight => Some("Right"),
        NamedKey::Home => Some("Home"),
        NamedKey::End => Some("End"),
        NamedKey::PageUp => Some("PageUp"),
        NamedKey::PageDown => Some("PageDown"),
        NamedKey::Insert => Some("Insert"),
        NamedKey::Delete => Some("Delete"),
        NamedKey::F1 => Some("F1"),
        NamedKey::F2 => Some("F2"),
        NamedKey::F3 => Some("F3"),
        NamedKey::F4 => Some("F4"),
        NamedKey::F5 => Some("F5"),
        NamedKey::F6 => Some("F6"),
        NamedKey::F7 => Some("F7"),
        NamedKey::F8 => Some("F8"),
        NamedKey::F9 => Some("F9"),
        NamedKey::F10 => Some("F10"),
        NamedKey::F11 => Some("F11"),
        NamedKey::F12 => Some("F12"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::{Line, Run};
    #[test]
    fn control_characters_match_terminal_input() {
        assert_eq!(control_byte("c"), Some(3));
        assert_eq!(control_byte("["), Some(27));
        assert_eq!(terminal_key(NamedKey::Space), Some(" "));
        assert_eq!(
            control_byte(terminal_key(NamedKey::Space).unwrap()),
            Some(0)
        );
        assert_eq!(control_byte("中"), None);
    }

    #[test]
    fn selection_joins_soft_wrapped_url_unicode_and_preserves_hard_newline() {
        let mut screen = Screen::new(20, 3);
        screen.lines[0] = Line {
            runs: vec![Run {
                text: "https://example/".into(),
                width: 16,
                hyperlink: Some("https://example/中文".into()),
                ..Run::default()
            }],
            wrapped: true,
            used_columns: Some(16),
        };
        screen.lines[1] = Line {
            runs: vec![Run {
                text: "中文e\u{301}".into(),
                width: 5,
                hyperlink: Some("https://example/中文".into()),
                ..Run::default()
            }],
            ..Line::default()
        };
        screen.lines[2] = Line {
            runs: vec![Run {
                text: "hard".into(),
                width: 4,
                ..Run::default()
            }],
            ..Line::default()
        };
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (0, 0),
                    end: (2, 4)
                }
            )
            .unwrap(),
            "https://example/中文e\u{301}\nhard"
        );
    }
    #[test]
    fn selection_slices_ascii_by_columns_and_preserves_unicode_cells() {
        let mut screen = Screen::new(10, 4);
        screen.lines[0].runs = vec![
            Run {
                text: "AB".into(),
                width: 2,
                ..Run::default()
            },
            Run {
                text: "中".into(),
                width: 2,
                ..Run::default()
            },
            Run {
                text: "e\u{301}".into(),
                width: 1,
                ..Run::default()
            },
        ];
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (0, 1),
                    end: (0, 2)
                }
            )
            .unwrap(),
            "B"
        );
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (0, 3),
                    end: (0, 4)
                }
            )
            .unwrap(),
            "中"
        );
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (0, 4),
                    end: (0, 5)
                }
            )
            .unwrap(),
            "e\u{301}"
        );
    }

    #[test]
    fn selection_handles_reverse_ranges_and_exclusive_next_row() {
        let mut screen = Screen::new(10, 4);
        screen.lines[0].runs = vec![Run {
            text: "first     ".into(),
            width: 10,
            ..Run::default()
        }];
        screen.lines[1].runs = vec![Run {
            text: "second    ".into(),
            width: 10,
            ..Run::default()
        }];
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (1, 3),
                    end: (0, 2)
                }
            )
            .unwrap(),
            "rst\nsec"
        );
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (0, 0),
                    end: (1, 0)
                }
            )
            .unwrap(),
            "first\n"
        );
        assert_eq!(
            selected_text(
                &screen,
                &Selection {
                    start: (0, 2),
                    end: (0, 2)
                }
            )
            .unwrap(),
            ""
        );
    }

    #[test]
    fn clipboard_byte_budget_rejects_oversized_unicode_without_truncating() {
        let allowed = "中".repeat(MAX_CLIPBOARD_BYTES / 3);
        assert!(validate_clipboard_text(&allowed).is_ok());
        assert!(validate_clipboard_text(&(allowed + "中")).is_err());
        let mut screen = Screen::new(10, 4);
        screen.lines[0].runs = vec![Run {
            text: "中".repeat(MAX_CLIPBOARD_BYTES / 3 + 1),
            width: 1,
            ..Run::default()
        }];
        assert!(selected_text(
            &screen,
            &Selection {
                start: (0, 0),
                end: (0, 1)
            }
        )
        .is_err());
    }

    #[test]
    fn scroll_anchor_tracks_output_and_history_eviction() {
        let mut previous = Screen {
            history: std::iter::repeat_with(Line::default).take(5).collect(),
            ..Screen::default()
        };
        let mut next = previous.clone();
        next.history
            .extend(std::iter::repeat_with(Line::default).take(3));
        assert_eq!(anchored_offset(&previous, &next, 2), 5);
        previous = next.clone();
        next.history_dropped = 3;
        assert_eq!(anchored_offset(&previous, &next, 2), 5);
        assert_eq!(anchored_offset(&previous, &next, 0), 0);
        next.history_dropped = 100;
        assert_eq!(anchored_offset(&previous, &next, 2), next.history.len());
    }

    #[test]
    fn history_clear_and_alternate_screen_return_to_bottom() {
        let previous = Screen {
            history: std::iter::repeat_with(Line::default).take(5).collect(),
            history_dropped: 8,
            ..Screen::default()
        };
        assert_eq!(anchored_offset(&previous, &Screen::default(), 3), 0);
        let mut next = previous.clone();
        next.alternate = true;
        assert_eq!(anchored_offset(&previous, &next, 3), 0);
    }
}

// FVPROV1 43524 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTVlMjg4ZGY1OTE0MjJmNzM0YWMyZmZkODgzZWZjNTJmNzU1ZDNjODY4MzQ1ZmVhZGQyMTRhNmQxMWYwYzY0MiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMDQxMTFlNGY0Mjc3YmI1ZjI3YjhiNTJlZTExMjk5ODUyMDU1NzkwMzg1ZGJhNDYxYWIyMDdkY2E1YjgxYzVmY2I0NjhkZGQ0Njk2NjY3OGVhNTYzYmM3MDBkNTQ4YTczY2MxNGJmNWQ1ZjI0NjBiMGQ0MTNhMTYwMzc0ZDg3MGQifV19XQ== FVPROV1-END
