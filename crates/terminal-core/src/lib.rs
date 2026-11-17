// SPDX-License-Identifier: GPL-3.0-only
//! Display-only projection of the Plyra libvterm protocol. Terminal output has
//! no execution, clipboard, URL-opening, or privileged-operation capability.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
mod history;

pub const MIN_COLS: u16 = 10;
pub const MAX_COLS: u16 = 240;
pub const MIN_ROWS: u16 = 4;
pub const MAX_ROWS: u16 = 100;
pub const MAX_EVENT_BYTES: usize = 1_048_576;
pub const MAX_HISTORY_LINES: usize = 2000;
pub const MAX_HISTORY_CELLS: usize = 200_000;
pub const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPalette {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub ansi: [[u8; 3]; 16],
}

impl Default for TerminalPalette {
    fn default() -> Self {
        Self {
            foreground: [187, 187, 187],
            background: [0, 0, 0],
            ansi: [
                [0, 0, 0],
                [187, 0, 0],
                [0, 187, 0],
                [187, 187, 0],
                [0, 0, 187],
                [187, 0, 187],
                [0, 187, 187],
                [187, 187, 187],
                [85, 85, 85],
                [255, 85, 85],
                [85, 255, 85],
                [255, 255, 85],
                [85, 85, 255],
                [255, 85, 255],
                [85, 255, 255],
                [255, 255, 255],
            ],
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    pub width: u16,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    #[serde(default)]
    pub fg_default: bool,
    #[serde(default)]
    pub fg_index: Option<u8>,
    #[serde(default)]
    pub bg_index: Option<u8>,
    pub bg_default: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strike: bool,
    #[serde(default)]
    pub hyperlink: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Line {
    pub runs: Vec<Run>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub wrapped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_columns: Option<u16>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Line {
    pub fn cell_count(&self) -> usize {
        self.runs.iter().map(|run| usize::from(run.width)).sum()
    }

    /// Conservative retained-history budget including allocation overhead.
    pub fn storage_cost(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .runs
                .iter()
                .map(|run| run.text.len() + run.hyperlink.as_ref().map_or(0, String::len) + 128)
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Line>,
    #[serde(default)]
    pub history: VecDeque<Line>,
    #[serde(default)]
    pub history_dropped: usize,
    pub cursor: Cursor,
    pub alternate: bool,
    pub title: String,
    #[serde(default)]
    pub mouse_mode: u8,
    #[serde(default)]
    pub focus_report: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.clamp(MIN_COLS, MAX_COLS);
        let rows = rows.clamp(MIN_ROWS, MAX_ROWS);
        Self {
            cols,
            rows,
            lines: vec![Line::default(); rows as usize],
            history: VecDeque::new(),
            history_dropped: 0,
            cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            alternate: false,
            title: String::new(),
            mouse_mode: 0,
            focus_report: false,
        }
    }

    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                line.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end_matches(' ')
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn append_history(&mut self, lines: impl IntoIterator<Item = Line>, dropped: usize) {
        self.history.extend(lines);
        self.history_dropped = self.history_dropped.saturating_add(dropped);
        self.trim_history();
    }

    pub fn replace_history(&mut self, lines: impl IntoIterator<Item = Line>) {
        self.history = lines.into_iter().collect();
        self.history_dropped = 0;
        self.trim_history();
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_dropped = 0;
    }

    /// Repack retained logical lines at a new width while preserving cell styles.
    pub fn reflow_history(&mut self, columns: u16) {
        let columns = columns.clamp(MIN_COLS, MAX_COLS);
        if self
            .history
            .iter()
            .all(|line| !line.wrapped && line.cell_count() <= usize::from(columns))
        {
            return;
        }
        self.history = history::reflow(&self.history, columns);
        self.trim_history();
    }

    pub fn reflowed_offset_from(&self, previous: &Screen, offset: usize) -> Option<usize> {
        if self.cols == previous.cols || self.history_dropped != previous.history_dropped {
            return None;
        }
        Some(history::reflowed_offset(
            &previous.history,
            &self.history,
            offset,
        ))
    }

    fn trim_history(&mut self) {
        let mut cells = self.history.iter().map(Line::cell_count).sum::<usize>();
        let mut bytes = self.history.iter().map(Line::storage_cost).sum::<usize>();
        while self.history.len() > MAX_HISTORY_LINES
            || cells > MAX_HISTORY_CELLS
            || bytes > MAX_HISTORY_BYTES
        {
            let line = self.history.pop_front().unwrap();
            cells -= line.cell_count();
            bytes -= line.storage_cost();
            self.history_dropped = self.history_dropped.saturating_add(1);
        }
    }

    /// Return the visible rows at a history offset without changing live state.
    /// Retained rows are reflowed when the terminal width changes.
    pub fn viewport(&self, offset: usize) -> Screen {
        let offset = if self.alternate {
            0
        } else {
            offset.min(self.history.len())
        };
        let mut view = Screen::new(self.cols, self.rows);
        view.lines = self
            .history
            .iter()
            .chain(&self.lines)
            .skip(self.history.len() - offset)
            .take(usize::from(self.rows))
            .cloned()
            .collect();
        view.cursor = self.cursor.clone();
        view.cursor.visible &= offset == 0;
        view.alternate = self.alternate;
        view.title.clone_from(&self.title);
        view.mouse_mode = if offset == 0 { self.mouse_mode } else { 0 };
        view.focus_report = self.focus_report;
        view
    }

    /// Validate the whole screen delta before changing any visible state.
    /// A full screen can span several events, with `full` set on its first part.
    pub fn apply_helper_event(&mut self, value: &Value) -> Result<bool> {
        match value.get("type").and_then(Value::as_str) {
            Some("scrollback") => {
                let raw_lines = value["lines"].as_array().context("invalid history lines")?;
                ensure!(raw_lines.len() <= 128, "history event exceeds line limit");
                let dropped = match value.get("dropped") {
                    None => 0,
                    Some(value) => value
                        .as_u64()
                        .filter(|n| *n <= i32::MAX as u64)
                        .context("invalid history dropped count")?
                        as usize,
                };
                let lines = raw_lines
                    .iter()
                    .map(|line| parse_line(line, MAX_COLS))
                    .collect::<Result<Vec<_>>>()?;
                self.append_history(lines, dropped);
                return Ok(true);
            }
            Some("scrollback_clear") => {
                self.clear_history();
                return Ok(true);
            }
            Some("screen") => {}
            _ => return Ok(false),
        }
        let cols = integer(&value["cols"], MIN_COLS, MAX_COLS)?;
        let rows = integer(&value["rows"], MIN_ROWS, MAX_ROWS)?;
        let cursor = Cursor {
            col: integer(&value["cursor"]["col"], 0, cols - 1)?,
            row: integer(&value["cursor"]["row"], 0, rows - 1)?,
            visible: value["cursor"]["visible"]
                .as_bool()
                .context("invalid cursor visibility")?,
        };
        let alternate = value["alternate"]
            .as_bool()
            .context("invalid alternate screen")?;
        let title = value["title"].as_str().context("invalid terminal title")?;
        ensure!(title.len() <= 4096, "terminal title exceeds limit");
        let full = optional_flag(value, "full")?;
        let mouse_mode = value
            .get("mouse_mode")
            .map(|value| integer(value, 0, 3))
            .transpose()?
            .unwrap_or(0) as u8;
        let focus_report = optional_flag(value, "focus_report")?;
        let updates = value["lines"].as_array().context("invalid screen lines")?;
        ensure!(updates.len() <= rows as usize, "too many screen lines");
        let mut changed = Vec::with_capacity(updates.len());
        let mut seen = vec![false; rows as usize];
        for update in updates {
            let row = integer(&update["row"], 0, rows - 1)? as usize;
            ensure!(!seen[row], "duplicate screen row");
            seen[row] = true;
            changed.push((row, parse_line(update, cols)?));
        }
        if full || cols != self.cols || rows != self.rows {
            self.lines = vec![Line::default(); rows as usize];
        }
        for (row, line) in changed {
            self.lines[row] = line;
        }
        if self.cols != cols {
            self.reflow_history(cols);
        }
        self.cols = cols;
        self.rows = rows;
        self.cursor = cursor;
        self.alternate = alternate;
        self.mouse_mode = mouse_mode;
        self.focus_report = focus_report;
        self.title = title.chars().filter(|c| !c.is_control()).collect();
        Ok(true)
    }
}

fn parse_line(value: &Value, columns: u16) -> Result<Line> {
    let raw_runs = value["runs"].as_array().context("invalid line runs")?;
    ensure!(raw_runs.len() <= usize::from(columns), "too many line runs");
    let mut runs = Vec::with_capacity(raw_runs.len());
    let mut width = 0u16;
    for raw in raw_runs {
        let text = raw["text"].as_str().context("invalid cell text")?;
        ensure!(
            !text.is_empty() && text.len() <= 16_384,
            "cell text exceeds limit"
        );
        ensure!(
            !text.chars().any(char::is_control),
            "control character in cell text"
        );
        let run_width = integer(&raw["width"], 1, columns)?;
        width = width
            .checked_add(run_width)
            .context("line width overflow")?;
        ensure!(width <= columns, "line exceeds grid width");
        runs.push(Run {
            hyperlink: match raw.get("hyperlink") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let uri = value.as_str().context("invalid hyperlink")?;
                    ensure!(
                        uri.len() <= 2048 && !uri.chars().any(char::is_control),
                        "invalid hyperlink text"
                    );
                    Some(uri.to_owned())
                }
            },
            text: text.to_owned(),
            width: run_width,
            fg: color(&raw["fg"])?,
            bg: color(&raw["bg"])?,
            fg_default: optional_flag(raw, "fg_default")?,
            fg_index: optional_color_index(raw.get("fg_index"))?,
            bg_index: optional_color_index(raw.get("bg_index"))?,
            bg_default: optional_flag(raw, "bg_default")?,
            bold: optional_flag(raw, "bold")?,
            italic: optional_flag(raw, "italic")?,
            underline: optional_flag(raw, "underline")?,
            inverse: optional_flag(raw, "inverse")?,
            strike: optional_flag(raw, "strike")?,
        });
    }
    let wrapped = optional_flag(value, "wrapped")?;
    let used_columns = match value.get("used_columns") {
        None | Some(Value::Null) => None,
        Some(value) => Some(integer(value, 0, width)?),
    };
    Ok(Line {
        runs,
        wrapped,
        used_columns,
    })
}

fn optional_color_index(value: Option<&Value>) -> Result<Option<u8>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let index = value.as_u64().context("invalid palette index")?;
            Ok(Some(
                u8::try_from(index).context("palette index exceeds 255")?,
            ))
        }
    }
}

fn integer(value: &Value, low: u16, high: u16) -> Result<u16> {
    let n = value.as_u64().context("expected unsigned integer")?;
    ensure!(
        n >= low as u64 && n <= high as u64,
        "integer outside grid bounds"
    );
    Ok(n as u16)
}

fn optional_flag(value: &Value, key: &str) -> Result<bool> {
    match value.get(key) {
        None => Ok(false),
        Some(value) => value.as_bool().context("invalid boolean flag"),
    }
}

fn color(value: &Value) -> Result<[u8; 3]> {
    let text = value.as_str().context("invalid color")?;
    if text.len() != 7
        || !text.starts_with('#')
        || !text.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    {
        bail!("invalid RGB color");
    }
    Ok([
        u8::from_str_radix(&text[1..3], 16)?,
        u8::from_str_radix(&text[3..5], 16)?,
        u8::from_str_radix(&text[5..7], 16)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> Value {
        json!({"type":"screen","full":true,"cols":80,"rows":24,
            "cursor":{"col":0,"row":0,"visible":true},"alternate":false,"title":"test",
            "lines":[{"row":0,"runs":[{"text":"中文e\u{301}","width":5,"fg":"#ffffff","bg":"#10141c"}]}]})
    }

    #[test]
    fn golden_screen_projection_covers_styles_unicode_and_alternate_frames() {
        let cases: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/ansi_cjk_alternate.events.json"
        ))
        .unwrap();
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/ansi_cjk_alternate.screens.json"
        ))
        .unwrap();
        let mut screen = Screen::default();
        for case in cases.as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            for event in case["events"].as_array().unwrap() {
                assert!(screen.apply_helper_event(event).unwrap());
            }
            assert_eq!(
                serde_json::to_value(&screen).unwrap(),
                expected[name],
                "{name}"
            );
        }
    }

    #[test]
    fn preserves_authoritative_wide_and_combining_cells() {
        let mut screen = Screen::default();
        screen.apply_helper_event(&event()).unwrap();
        assert_eq!(screen.lines[0].runs[0].width, 5);
        assert!(screen.plain_text().starts_with("中文e\u{301}"));
    }

    #[test]
    fn palette_sources_validate_and_older_runs_keep_explicit_colors() {
        let mut screen = Screen::default();
        screen.apply_helper_event(&event()).unwrap();
        assert!(!screen.lines[0].runs[0].fg_default);
        assert_eq!(screen.lines[0].runs[0].fg_index, None);
        let mut themed = event();
        themed["lines"][0]["runs"][0]["fg_default"] = json!(true);
        themed["lines"][0]["runs"][0]["fg_index"] = json!(7);
        themed["lines"][0]["runs"][0]["bg_index"] = json!(255);
        screen.apply_helper_event(&themed).unwrap();
        assert!(screen.lines[0].runs[0].fg_default);
        assert_eq!(screen.lines[0].runs[0].fg_index, Some(7));
        assert_eq!(screen.lines[0].runs[0].bg_index, Some(255));
        themed["lines"][0]["runs"][0]["fg_index"] = json!(256);
        assert!(screen.apply_helper_event(&themed).is_err());
        assert_eq!(screen.lines[0].runs[0].fg_index, Some(7));
    }

    #[test]
    fn invalid_event_is_atomic() {
        let mut screen = Screen::default();
        screen.apply_helper_event(&event()).unwrap();
        let before = screen.plain_text();
        let mut bad = event();
        bad["lines"][0]["runs"][0]["width"] = json!(81);
        assert!(screen.apply_helper_event(&bad).is_err());
        assert_eq!(screen.plain_text(), before);
    }

    #[test]
    fn split_full_frame_keeps_earlier_rows() {
        let mut screen = Screen::default();
        screen.apply_helper_event(&event()).unwrap();
        let mut next = event();
        next["full"] = json!(false);
        next["lines"][0]["row"] = json!(9);
        screen.apply_helper_event(&next).unwrap();
        assert_eq!(screen.lines[0].runs[0].text, screen.lines[9].runs[0].text);
    }

    fn line(text: &str, width: u16) -> Line {
        Line {
            runs: vec![Run {
                text: text.into(),
                width,
                ..Run::default()
            }],
            ..Line::default()
        }
    }

    #[test]
    fn history_events_preserve_unicode_and_validate_atomically() {
        let mut screen = Screen::default();
        let history = json!({"type":"scrollback","dropped":3,"lines":[
            {"runs":[{"text":"中文e\u{301}","width":5,"fg":"#ffffff","bg":"#10141c"}]}]});
        screen.apply_helper_event(&history).unwrap();
        assert_eq!(screen.history[0].runs[0].width, 5);
        assert_eq!(screen.history_dropped, 3);
        assert!(!screen.plain_text().contains("中文"));
        let mut invalid = history.clone();
        invalid["lines"][0]["runs"][0]["width"] = json!(241);
        assert!(screen.apply_helper_event(&invalid).is_err());
        assert_eq!(screen.history.len(), 1);
        assert_eq!(screen.history_dropped, 3);
        screen
            .apply_helper_event(&json!({"type":"scrollback_clear"}))
            .unwrap();
        assert!(screen.history.is_empty());
        assert_eq!(screen.history_dropped, 0);
    }

    #[test]
    fn history_line_cell_and_storage_limits_keep_recent_rows() {
        let mut screen = Screen::default();
        screen.append_history((0..2005).map(|index| line(&index.to_string(), 1)), 0);
        assert_eq!(screen.history.len(), MAX_HISTORY_LINES);
        assert_eq!(screen.history_dropped, 5);
        assert_eq!(screen.history.front().unwrap().runs[0].text, "5");
        assert_eq!(screen.history.back().unwrap().runs[0].text, "2004");
        screen.replace_history((0..1000).map(|_| line("wide row", 240)));
        assert!(screen.history.iter().map(Line::cell_count).sum::<usize>() <= MAX_HISTORY_CELLS);
        assert_eq!(screen.history.len(), MAX_HISTORY_CELLS / 240);
        let large = "x".repeat(16_384);
        screen.replace_history((0..1200).map(|_| line(&large, 1)));
        assert!(screen.history.iter().map(Line::storage_cost).sum::<usize>() <= MAX_HISTORY_BYTES);
        assert!(screen.history.len() < 1200);
    }

    #[test]
    fn viewport_scrolls_clamps_and_keeps_alternate_screen_live() {
        let mut screen = Screen::new(10, 4);
        screen.lines = (0..4)
            .map(|index| line(&format!("live{index}"), 5))
            .collect();
        screen.append_history([line("old0", 4), line("old1", 4)], 0);
        let latest = screen.viewport(0);
        assert!(latest.cursor.visible);
        assert!(latest.plain_text().starts_with("live0"));
        assert!(latest.history.is_empty());
        let previous = screen.viewport(1);
        assert_eq!(previous.plain_text(), "old1\nlive0\nlive1\nlive2");
        assert!(!previous.cursor.visible);
        assert_eq!(
            screen.viewport(usize::MAX).plain_text(),
            "old0\nold1\nlive0\nlive1"
        );
        screen.alternate = true;
        assert_eq!(screen.viewport(2).plain_text(), latest.plain_text());
        assert!(screen.viewport(2).cursor.visible);
    }

    #[test]
    fn older_serialized_screens_default_to_empty_history() {
        let mut value = serde_json::to_value(Screen::default()).unwrap();
        value.as_object_mut().unwrap().remove("history");
        value.as_object_mut().unwrap().remove("history_dropped");
        let screen: Screen = serde_json::from_value(value).unwrap();
        assert!(screen.history.is_empty());
        assert_eq!(screen.history_dropped, 0);
    }
}

// FVPROV1 21196 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTRjNGRlYzE4NzIyYzMwNWNjMjMyNGU3OGRmM2EwYzkzMmZiMWIwNzVlYzU5ZjhjZWVkZmVjYjRhNmQ0YTVjYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzg2NTVkZWYxYTRkYjExYTIxYzAyMTczOTMzNDI5NzBjNGI1MGEwZGJkMTc0MTUxMmI3NGI5MTM4MGE1MWM0ZjZlZTMxMzAyZWZiOTczMzc4OGMwNDU1MGUzNzg3ZDQ3MTVlYTIzMGQ5ZDhmMTFmZDJiZjE5MmE0ZTgzN2U4MDEifV19XQ== FVPROV1-END
