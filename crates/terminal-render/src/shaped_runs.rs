// SPDX-License-Identifier: GPL-3.0-only
use std::num::NonZeroUsize;
use std::sync::Arc;

use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, Weight, Wrap};
use lru::LruCache;
use terminal_core::Run;

const MAX_ENTRIES: usize = 4096;
const MAX_ESTIMATED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Eq, PartialEq, Hash, Clone)]
struct ShapeKey {
    text: String,
    columns: u16,
    bold: bool,
    italic: bool,
    font_size: u32,
    line_height: u32,
}

struct CachedShape {
    buffer: Arc<Buffer>,
    width: f32,
    estimated_bytes: usize,
}

pub(super) struct ShapeCache {
    entries: LruCache<ShapeKey, CachedShape>,
    estimated_bytes: usize,
    frame_live_estimated_bytes: usize,
    frame_peak_estimated_bytes: usize,
    peak_estimated_bytes: usize,
    hits: u64,
    misses: u64,
}

impl Default for ShapeCache {
    fn default() -> Self {
        Self {
            entries: LruCache::new(
                NonZeroUsize::new(MAX_ENTRIES).expect("positive cache capacity"),
            ),
            estimated_bytes: 0,
            frame_live_estimated_bytes: 0,
            frame_peak_estimated_bytes: 0,
            peak_estimated_bytes: 0,
            hits: 0,
            misses: 0,
        }
    }
}

impl ShapeCache {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.estimated_bytes = 0;
        self.frame_live_estimated_bytes = 0;
    }

    pub fn begin_frame(&mut self) {
        // The previous frame's TextAreas have retired. Resident buffers remain.
        self.frame_live_estimated_bytes = self.estimated_bytes;
        self.frame_peak_estimated_bytes = self.estimated_bytes;
    }

    pub fn diagnostics(&self) -> String {
        format!(
            "shape_cache_entries={} shape_cache_estimated_bytes={} shape_frame_peak_estimated_bytes={} shape_peak_estimated_bytes={} shape_cache_hits={} shape_cache_misses={}",
            self.entries.len(), self.estimated_bytes, self.frame_peak_estimated_bytes,
            self.peak_estimated_bytes, self.hits, self.misses,
        )
    }

    pub fn shape(
        &mut self,
        font_system: &mut FontSystem,
        run: &Run,
        metrics: Metrics,
    ) -> (Arc<Buffer>, f32) {
        let key = ShapeKey {
            text: run.text.clone(),
            columns: run.width,
            bold: run.bold,
            italic: run.italic,
            font_size: metrics.font_size.to_bits(),
            line_height: metrics.line_height.to_bits(),
        };
        if let Some(entry) = self.entries.get(&key) {
            self.hits = self.hits.saturating_add(1);
            return (Arc::clone(&entry.buffer), entry.width);
        }
        self.misses = self.misses.saturating_add(1);
        let mut buffer = Buffer::new(font_system, metrics);
        buffer.set_wrap(font_system, Wrap::None);
        // Layout is unwrapped. Extent, clipping, position and terminal-cell fit
        // are recomputed by the renderer for each placement, including RTL text.
        buffer.set_size(font_system, None, Some(metrics.line_height));
        let attributes = Attrs::new()
            .family(Family::Monospace)
            .weight(if run.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            })
            .style(if run.italic {
                Style::Italic
            } else {
                Style::Normal
            });
        buffer.set_text(font_system, &run.text, attributes, Shaping::Advanced);
        buffer.shape_until_scroll(font_system, false);
        let width = buffer
            .layout_runs()
            .map(|line| line.line_w)
            .fold(0.0_f32, f32::max);
        let glyphs: usize = buffer.layout_runs().map(|line| line.glyphs.len()).sum();
        // Include key/text copies, shaped and layout glyphs, and per-buffer
        // scratch/allocator overhead. This is a conservative storage estimate.
        let glyph_storage = glyphs.max(run.text.chars().count());
        let estimated_bytes = 2048_usize
            .saturating_add(run.text.len().saturating_mul(3))
            .saturating_add(glyph_storage.saturating_mul(512));
        let buffer = Arc::new(buffer);
        // Buffers evicted from the LRU may still be held by this frame's
        // TextAreas. Track that temporary overlap separately from the resident
        // cache limit. These figures estimate shape storage, not process RSS.
        self.frame_live_estimated_bytes = self
            .frame_live_estimated_bytes
            .saturating_add(estimated_bytes);
        self.frame_peak_estimated_bytes = self
            .frame_peak_estimated_bytes
            .max(self.frame_live_estimated_bytes);
        self.peak_estimated_bytes = self
            .peak_estimated_bytes
            .max(self.frame_peak_estimated_bytes);
        if estimated_bytes <= MAX_ESTIMATED_BYTES {
            while self.entries.len() >= MAX_ENTRIES
                || self.estimated_bytes.saturating_add(estimated_bytes) > MAX_ESTIMATED_BYTES
            {
                let Some((_, removed)) = self.entries.pop_lru() else {
                    break;
                };
                self.estimated_bytes -= removed.estimated_bytes;
                if Arc::strong_count(&removed.buffer) == 1 {
                    self.frame_live_estimated_bytes = self
                        .frame_live_estimated_bytes
                        .saturating_sub(removed.estimated_bytes);
                }
            }
            self.estimated_bytes += estimated_bytes;
            self.entries.put(
                key,
                CachedShape {
                    buffer: Arc::clone(&buffer),
                    width,
                    estimated_bytes,
                },
            );
        }
        (buffer, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_tui_runs_reuse_layout_and_invalidate_style_metrics_and_width() {
        let mut fonts = FontSystem::new_with_fonts([glyphon::fontdb::Source::Binary(Arc::new(
            super::super::FONT.to_vec(),
        ))]);
        fonts.db_mut().set_monospace_family("DejaVu Sans Mono");
        let mut cache = ShapeCache::default();
        let mut run = Run {
            text: "CPU 99.9% ── 中文 e\u{301}".to_owned(),
            width: 25,
            ..Run::default()
        };
        let metrics = Metrics::new(30.0, 46.0);
        let (first, width) = cache.shape(&mut fonts, &run, metrics);
        for _ in 0..100 {
            let (next, next_width) = cache.shape(&mut fonts, &run, metrics);
            assert!(Arc::ptr_eq(&first, &next));
            assert_eq!(width, next_width);
        }
        assert_eq!(cache.hits, 100);
        assert_eq!(cache.misses, 1);
        run.width += 1;
        assert!(!Arc::ptr_eq(
            &first,
            &cache.shape(&mut fonts, &run, metrics).0
        ));
        run.width -= 1;
        run.bold = true;
        assert!(!Arc::ptr_eq(
            &first,
            &cache.shape(&mut fonts, &run, metrics).0
        ));
        run.bold = false;
        assert!(!Arc::ptr_eq(
            &first,
            &cache.shape(&mut fonts, &run, Metrics::new(28.0, 44.0)).0
        ));
        cache.clear();
        assert_eq!(cache.estimated_bytes, 0);
        assert!(!Arc::ptr_eq(
            &first,
            &cache.shape(&mut fonts, &run, metrics).0
        ));
    }
}

// FVPROV1 7411 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiODgxNTJjZTYwYzBjMzYwMWYxZTRmMTQ1YzJlOTg1NTE4ZjdlN2MzODA1NzVhZTU2ZDk0Y2Y1ZWVhMDVhZGM0OSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZjllZjY0ZTI2NTcyOWU1M2JlMTY1NTk1MzVkNDU5YWI0NmQ0M2EyYjhkYWY0NjAxMGM4ZjcxMWIyYzFmODU3MzI1ZjdkNzMzOTA4YzM3YjRjMmEzOWNmNzI4NDY4NjdjNzRiYzdhNGQ0ZGFlOTRhNmUxMzNmNDNmOWQ0ZmU4MGUifV19XQ== FVPROV1-END
