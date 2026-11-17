use crate::{Line, Run};
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn same_style(left: &Run, right: &Run) -> bool {
    left.fg == right.fg
        && left.bg == right.bg
        && left.fg_default == right.fg_default
        && left.bg_default == right.bg_default
        && left.fg_index == right.fg_index
        && left.bg_index == right.bg_index
        && left.bold == right.bold
        && left.italic == right.italic
        && left.underline == right.underline
        && left.inverse == right.inverse
        && left.strike == right.strike
        && left.hyperlink == right.hyperlink
}

fn used_columns(line: &Line) -> usize {
    if let Some(columns) = line.used_columns {
        return usize::from(columns);
    }
    let mut columns = line.cell_count();
    for run in line.runs.iter().rev() {
        if !run.bg_default || run.inverse || run.underline || run.strike || run.hyperlink.is_some()
        {
            break;
        }
        let spaces = run.text.len() - run.text.trim_end_matches(' ').len();
        columns = columns.saturating_sub(spaces.min(usize::from(run.width)));
        if spaces != run.text.len() {
            break;
        }
    }
    columns
}

fn each_glyph(line: &Line, mut visit: impl FnMut(&Run, &str, usize)) {
    let mut remaining = used_columns(line);
    for run in &line.runs {
        let mut graphemes = run.text.graphemes(true).peekable();
        while let Some(text) = graphemes.next() {
            let width = if text.len() == run.text.len() {
                usize::from(run.width)
            } else {
                UnicodeWidthStr::width(text)
            };
            if width > remaining {
                return;
            }
            visit(run, text, width);
            remaining = remaining.saturating_sub(width);
            if remaining == 0
                && graphemes
                    .peek()
                    .is_some_and(|text| UnicodeWidthStr::width(*text) > 0)
            {
                return;
            }
        }
    }
}

fn append(row: &mut Line, style: &Run, text: &str, width: usize) {
    if let Some(last) = row.runs.last_mut().filter(|last| same_style(last, style)) {
        last.text.push_str(text);
        last.width += width as u16;
        return;
    }
    let mut run = style.clone();
    run.text = text.to_owned();
    run.width = width as u16;
    row.runs.push(run);
}

pub(super) fn reflow(lines: &VecDeque<Line>, columns: u16) -> VecDeque<Line> {
    let mut result = VecDeque::new();
    let mut row = Line::default();
    let mut width = 0usize;
    for source in lines {
        each_glyph(source, |style, text, glyph_width| {
            if width > 0 && width + glyph_width > usize::from(columns) {
                row.wrapped = true;
                row.used_columns = Some(width as u16);
                result.push_back(std::mem::take(&mut row));
                width = 0;
            }
            append(&mut row, style, text, glyph_width);
            width += glyph_width;
        });
        if !source.wrapped {
            row.used_columns = Some(width as u16);
            result.push_back(std::mem::take(&mut row));
            width = 0;
        }
    }
    if !row.runs.is_empty() {
        row.wrapped = lines.back().is_some_and(|line| line.wrapped);
        row.used_columns = Some(width as u16);
        result.push_back(row);
    }
    result
}

fn text_bytes(line: &Line) -> usize {
    let mut bytes = 0;
    each_glyph(line, |_, text, _| bytes += text.len());
    bytes
}

pub(super) fn reflowed_offset(
    previous: &VecDeque<Line>,
    next: &VecDeque<Line>,
    offset: usize,
) -> usize {
    if offset == 0 {
        return 0;
    }
    let first = previous.len().saturating_sub(offset);
    let mut paragraph = 0;
    let mut byte_offset = 0;
    for line in previous.iter().take(first) {
        byte_offset += text_bytes(line);
        if !line.wrapped {
            paragraph += 1;
            byte_offset = 0;
        }
    }
    let mut current_paragraph = 0;
    let mut bytes = 0;
    for (index, line) in next.iter().enumerate() {
        let count = text_bytes(line);
        if current_paragraph == paragraph && (byte_offset == bytes || byte_offset < bytes + count) {
            return next.len() - index;
        }
        bytes += count;
        if !line.wrapped {
            current_paragraph += 1;
            bytes = 0;
        }
    }
    next.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, wrapped: bool) -> Line {
        Line {
            runs: vec![Run {
                text: text.into(),
                width: UnicodeWidthStr::width(text) as u16,
                fg_default: true,
                bg_default: true,
                ..Run::default()
            }],
            wrapped,
            used_columns: Some(UnicodeWidthStr::width(text) as u16),
        }
    }

    fn text(lines: &VecDeque<Line>) -> String {
        lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.as_str())
            .collect()
    }

    #[test]
    fn narrow_then_widen_preserves_soft_breaks_hard_breaks_and_spaces() {
        let source = VecDeque::from([
            line("abcdefghij", true),
            line("klmnop    ", false),
            line("second", false),
        ]);
        let narrow = reflow(&source, 5);
        assert_eq!(text(&source), text(&narrow));
        assert_eq!(narrow.len(), 6);
        let wider = reflow(&narrow, 40);
        assert_eq!(wider.len(), 2);
        assert_eq!(text(&source), text(&wider));
        assert!(!wider[0].wrapped);
        assert!(!wider[1].wrapped);
    }

    #[test]
    fn unicode_graphemes_and_hyperlinks_keep_their_styles() {
        let mut source = line("A中e\u{301}😀BC", false);
        source.runs[0].hyperlink = Some("https://example.com".into());
        source.runs[0].bold = true;
        let source = VecDeque::from([source]);
        let narrow = reflow(&source, 4);
        assert_eq!(text(&narrow), text(&source));
        assert!(narrow.iter().all(|row| row.cell_count() <= 4));
        assert!(narrow
            .iter()
            .flat_map(|row| &row.runs)
            .all(|run| run.bold && run.hyperlink.as_deref() == Some("https://example.com")));
    }

    #[test]
    fn native_used_columns_distinguishes_padding_from_written_spaces() {
        let mut source = line("A中     ", false);
        source.used_columns = Some(3);
        let rows = reflow(&VecDeque::from([source]), 10);
        assert_eq!(text(&rows), "A中");
        assert_eq!(rows[0].used_columns, Some(3));
        let blank = line("", false);
        assert_eq!(reflow(&VecDeque::from([blank]), 10).len(), 1);
    }

    #[test]
    fn resize_anchor_follows_the_same_logical_text() {
        let source = VecDeque::from([
            line("abcdefghij", true),
            line("klmnopqrst", false),
            line("last", false),
        ]);
        let narrow = reflow(&source, 5);
        assert_eq!(reflowed_offset(&source, &narrow, 2), 3);
        assert_eq!(reflowed_offset(&source, &narrow, 0), 0);
    }
}

// FVPROV1 7194 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYWM2OWU4OGFjZTZiZTAyMmU1ZDM2NjI0YThhNGVlYTFiNjNlY2I5NzQxODRkZmY5ZDQxOTEwMTkxOWNiODQxOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjI2YTMyNDRiNmM0MjJhOTZiZTkwNjQ4NmVmYjE1OTNjMjAxN2YzNzllYjdhMzE5ZTI1MDMzNDMzZjM0MmQyZDE1ZDA3ZDM2MmEzNTQ2MDkyMDY3YWVkZTg2NzA0N2QzNTkxM2JlMjE5ZDBhYzEzNjVlOWJjZDkwNGNhZTE5MDAifV19XQ== FVPROV1-END
