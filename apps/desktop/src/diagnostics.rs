use serde_json::{json, Value};
use std::{collections::VecDeque, time::Instant};

const LIMIT: usize = 256;
struct Pending {
    pane: u64,
    revision: u64,
    started: Instant,
    ready: bool,
}

#[derive(Default)]
pub struct Diagnostics {
    render: VecDeque<f64>,
    latency: VecDeque<f64>,
    pending: VecDeque<Pending>,
    discarded: usize,
}

impl Diagnostics {
    pub fn input(&mut self, pane: u64, revision: u64) {
        if self.pending.len() == LIMIT {
            self.pending.pop_front();
            self.discarded += 1;
        }
        self.pending.push_back(Pending {
            pane,
            revision,
            started: Instant::now(),
            ready: false,
        });
    }
    pub fn output(&mut self, pane: u64, revision: u64) {
        for pending in &mut self.pending {
            if pending.pane == pane && revision > pending.revision {
                pending.ready = true;
            }
        }
    }
    pub fn render(&mut self, milliseconds: f64) {
        push(&mut self.render, milliseconds);
    }
    pub fn presented(&mut self, panes: &[u64]) {
        let now = Instant::now();
        let mut retained = VecDeque::new();
        while let Some(pending) = self.pending.pop_front() {
            if pending.ready && panes.contains(&pending.pane) {
                push(
                    &mut self.latency,
                    now.duration_since(pending.started).as_secs_f64() * 1000.0,
                );
            } else {
                retained.push_back(pending);
            }
        }
        self.pending = retained;
    }
    pub fn close(&mut self, pane: u64) {
        let before = self.pending.len();
        self.pending.retain(|pending| pending.pane != pane);
        self.discarded += before - self.pending.len();
    }
    pub fn report(&self) -> Value {
        json!({"retained_sample_limit":LIMIT,"render_callback_wall_ms":summary(&self.render),
            "software_input_to_next_pty_update_presented_ms":summary(&self.latency),"pending_input_samples":self.pending.len(),"discarded_input_samples":self.discarded,
            "percentile_method":"nearest-rank: zero-based index ceil(n * percentile / 100) - 1; empty samples return null",
            "timing_source":"Monotonic software timestamps. Input samples wait for a later PTY screen revision in the same pane and that pane's next surface.present call.",
            "input_output_correspondence":"next update only","physical_latency_measured":false})
    }
}
fn push(samples: &mut VecDeque<f64>, value: f64) {
    samples.push_back(value);
    while samples.len() > LIMIT {
        samples.pop_front();
    }
}
fn summary(samples: &VecDeque<f64>) -> Value {
    if samples.is_empty() {
        return json!({"count":0,"p50":null,"p95":null});
    }
    let mut values: Vec<_> = samples.iter().copied().collect();
    values.sort_by(f64::total_cmp);
    let p50_index = (values.len() * 50).div_ceil(100) - 1;
    let p95_index = (values.len() * 95).div_ceil(100) - 1;
    json!({"count":values.len(),"p50":values[p50_index],"p95":values[p95_index]})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latency_requires_output_from_the_same_visible_pane() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.input(1, 4);
        diagnostics.output(2, 9);
        diagnostics.presented(&[1]);
        assert_eq!(
            diagnostics.report()["software_input_to_next_pty_update_presented_ms"]["count"],
            0
        );
        diagnostics.output(1, 5);
        diagnostics.presented(&[2]);
        assert_eq!(diagnostics.pending.len(), 1);
        diagnostics.presented(&[1]);
        assert_eq!(
            diagnostics.report()["software_input_to_next_pty_update_presented_ms"]["count"],
            1
        );
    }
    #[test]
    fn numeric_samples_have_a_fixed_memory_bound() {
        let mut diagnostics = Diagnostics::default();
        for value in 0..1000 {
            diagnostics.render(f64::from(value));
        }
        assert_eq!(diagnostics.render.len(), LIMIT);
    }
    #[test]
    fn percentile_summary_uses_nearest_rank_for_small_and_empty_samples() {
        assert_eq!(
            summary(&VecDeque::new()),
            json!({"count":0,"p50":null,"p95":null})
        );
        assert_eq!(
            summary(&VecDeque::from([37.302])),
            json!({"count":1,"p50":37.302,"p95":37.302})
        );
        assert_eq!(
            summary(&VecDeque::from([81.0, 37.302])),
            json!({"count":2,"p50":37.302,"p95":81.0})
        );
        assert_eq!(
            Diagnostics::default().report()["percentile_method"],
            "nearest-rank: zero-based index ceil(n * percentile / 100) - 1; empty samples return null"
        );
    }
}

// FVPROV1 4772 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMzZmMGRlY2ZmMzU4OWI2YjIzNjQwMjhmYTVhMTFiMTQzZDkzOTYxMzQ1OTUwNDg2OGVkN2FmNzdlZDcwNzgxZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjU5MDhjY2RhMDIwOGEwMTY2MTlmOTE3ZTc5MjVkMDg3YzJkZjM3NTE5MzY3NDU0OTRlMGQ0ZDU1YjVmOTdiMzQ3NjcyYjRlYjgwMjY1NTgwODUzZWNmN2RkNDYzMzBmMmVjYTBiMWYwZTNiYjEzODI5ODMxYTJlODVhNzdiMGIifV19XQ== FVPROV1-END
