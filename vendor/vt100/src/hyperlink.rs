use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const MAX_TARGET_BYTES: usize = 4096;
const MAX_SHARED_BYTES: usize = 1024 * 1024;
const TARGET_OVERHEAD: usize = 64;

#[derive(Debug)]
pub(crate) struct Hyperlink {
    pub(crate) uri: String,
    budget: Arc<AtomicUsize>,
}

impl Hyperlink {
    pub(crate) fn new(parts: &[&[u8]], budget: &Arc<AtomicUsize>) -> Option<Arc<Self>> {
        let bytes = parts
            .iter()
            .try_fold(0_usize, |total, part| total.checked_add(part.len() + 1))?
            .saturating_sub(1);
        if bytes == 0 || bytes > MAX_TARGET_BYTES {
            return None;
        }
        let uri = String::from_utf8(parts.join(&b';')).ok()?;
        if uri.chars().any(char::is_control) {
            return None;
        }
        let charge = uri.len() + TARGET_OVERHEAD;
        let mut current = budget.load(Ordering::Acquire);
        loop {
            let next = current
                .checked_add(charge)
                .filter(|value| *value <= MAX_SHARED_BYTES)?;
            match budget.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        Some(Arc::new(Self {
            uri,
            budget: Arc::clone(budget),
        }))
    }
}

impl Drop for Hyperlink {
    fn drop(&mut self) {
        self.budget
            .fetch_sub(self.uri.len() + TARGET_OVERHEAD, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn osc8_chunked_bel_st_close_and_semicolon() {
        let mut parser = crate::Parser::new(3, 40, 4);
        parser.process(b"\x1b]8;id=owned;https://example.org/a;");
        parser.process(b"b\x07LI");
        parser.process(b"NK\x1b[0mX\x1b]8;;\x1b");
        parser.process(b"\\P");
        for col in 0..5 {
            assert_eq!(
                parser.screen().cell(0, col).unwrap().hyperlink(),
                Some("https://example.org/a;b")
            );
        }
        assert_eq!(parser.screen().cell(0, 5).unwrap().hyperlink(), None);
    }

    #[test]
    fn osc8_survives_resize_scroll_and_alternate_grid_then_erases() {
        let mut parser = crate::Parser::new(3, 20, 10);
        parser.process(b"\x1b]8;;https://main\x07LINK\x1b]8;;\x07");
        parser.screen_mut().set_size(4, 21);
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://main")
        );
        parser.process(b"\x1b[?1049h\x1b]8;;https://alt\x1b\\ALT\x1b]8;;\x1b\\");
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://alt")
        );
        parser.process(b"\x1b[?1049l");
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://main")
        );
        parser.process(b"\x1b[H\x1b[2K");
        assert_eq!(parser.screen().cell(0, 0).unwrap().hyperlink(), None);

        let mut scroll = crate::Parser::new(2, 20, 10);
        scroll.process(b"\x1b]8;;https://history\x07LINK\x1b]8;;\x07\r\none\r\ntwo\r\nthree");
        scroll.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            scroll.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://history")
        );
    }

    #[test]
    fn osc8_wide_combining_cells_and_overwrite() {
        let mut parser = crate::Parser::new(2, 20, 0);
        parser.process("\x1b]8;;https://wide\x07中e\u{301}\x1b]8;;\x07".as_bytes());
        for col in 0..3 {
            assert_eq!(
                parser.screen().cell(0, col).unwrap().hyperlink(),
                Some("https://wide")
            );
        }
        parser.process(b"\x1b[Hxx");
        assert_eq!(parser.screen().cell(0, 0).unwrap().hyperlink(), None);
        assert_eq!(parser.screen().cell(0, 1).unwrap().hyperlink(), None);
    }

    #[test]
    fn osc8_truncated_parameter_lists_clear_previous_target() {
        let mut parser = crate::Parser::new(2, 20, 0);
        parser.process(b"\x1b]8;;https://before\x07A");
        let uri = format!("https://owned/{}", "segment;".repeat(20));
        parser.process(format!("\x1b]8;;{uri}\x07B").as_bytes());
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://before")
        );
        assert_eq!(parser.screen().cell(0, 1).unwrap().hyperlink(), None);
        assert_eq!(parser.screen().cell(0, 1).unwrap().contents(), "B");
    }

    #[test]
    fn osc8_budget_drops_only_metadata_and_releases_erased_targets() {
        let mut parser = crate::Parser::new(1, 400, 0);
        for index in 0..300 {
            let uri = format!("https://owned/{index:04}{}", "x".repeat(3980));
            parser.process(format!("\x1b]8;;{uri}\x07x\x1b]8;;\x07").as_bytes());
        }
        assert!(parser.screen().cell(0, 0).unwrap().hyperlink().is_some());
        assert_eq!(parser.screen().cell(0, 299).unwrap().hyperlink(), None);
        assert_eq!(parser.screen().cell(0, 299).unwrap().contents(), "x");
        parser.process(b"\x1b[2J\x1b[H\x1b]8;;https://reused\x07R\x1b]8;;\x07");
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://reused")
        );
        parser.process(b"\x1bcN");
        assert_eq!(parser.screen().cell(0, 0).unwrap().hyperlink(), None);
        let budget = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(super::Hyperlink::new(&[b"bad\x01uri"], &budget).is_none());
        assert!(super::Hyperlink::new(&[&vec![b'x'; 4097]], &budget).is_none());
    }
}
