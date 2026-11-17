// SPDX-License-Identifier: GPL-3.0-only
//! Measure bounded native PTY ingestion with a numbered output fixture.
use anyhow::{ensure, Context, Result};
use session_manager::{Backend, Session, SessionEvent, SessionOptions};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let output = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("Expected a new results file")?,
    );
    ensure!(!output.exists(), "Results file already exists");
    let count = 20_000_u64;
    let program = format!(
        "BEGIN {{ for (i=0;i<{count};i++) printf(\"L%06d terminal throughput payload\\n\",i); print \"__THROUGHPUT_DONE__\"; }}"
    );
    let started = Instant::now();
    let mut session = Session::spawn(
        SessionOptions {
            backend: Backend::Helper,
            cols: 100,
            rows: 40,
            command: Some(vec!["/usr/bin/awk".into(), program]),
            ..SessionOptions::default()
        },
        Arc::new(|| {}),
    )?;
    let mut clean_exit = false;
    while started.elapsed() < Duration::from_secs(60) && !clean_exit {
        for event in session.take_events() {
            match event {
                SessionEvent::Exit(code) => {
                    ensure!(code == 0, "Output fixture exited {code}");
                    clean_exit = true;
                }
                SessionEvent::Error(error) => anyhow::bail!("{error}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    ensure!(clean_exit, "Output fixture timed out");
    let elapsed = started.elapsed();
    let screen = session.snapshot();
    session.close()?;
    ensure!(
        screen.plain_text().contains("__THROUGHPUT_DONE__"),
        "Final output marker missing"
    );
    let retained: Vec<u64> = screen
        .history
        .iter()
        .chain(&screen.lines)
        .map(|line| {
            line.runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
        })
        .filter_map(|line| {
            line.strip_prefix('L')
                .and_then(|value| value.get(..6))
                .and_then(|value| value.parse().ok())
        })
        .collect();
    ensure!(!retained.is_empty(), "No numbered output retained");
    ensure!(
        retained.last() == Some(&(count - 1)),
        "Final numbered line missing"
    );
    ensure!(
        retained.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "Retained output contains a gap or duplicate"
    );
    ensure!(
        screen.history.len() <= terminal_core::MAX_HISTORY_LINES,
        "History bound exceeded"
    );
    ensure!(
        screen.history_dropped > 0,
        "Bounded retention did not account for discarded history"
    );
    let result = serde_json::json!({
        "schema": "glass.terminal-throughput", "passed": true,
        "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "backend": "helper + native PTY + libvterm", "profile": "dev",
        "lines_emitted": count, "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "lines_per_second": count as f64 / elapsed.as_secs_f64(),
        "retained_numbered_lines": retained.len(), "first_retained": retained[0],
        "last_retained": retained.last(), "history_lines": screen.history.len(),
        "history_dropped": screen.history_dropped, "reader_updates": session.output_revision(),
        "coverage": "Software ingestion through final screen marker, bounded retained suffix checked for gaps and duplicates. Rendering excluded."
    });
    fs::write(&output, serde_json::to_vec_pretty(&result)?)?;
    println!("{result}");
    Ok(())
}

// FVPROV1 3776 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiODVmMzM1ZWZmNmE2MzIyNDgxY2M5YzFmYTdmMzNhMWQ3M2YzNTQ3MTJhNDhkYWI2ODM4MzE5OTBlNDE2N2YwNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNjI2YzBiYzVkYjRmNDE2NDNkYmVmYTdiNjRjY2JkNTUyOTJlM2EyMGMxOWFiOTY4YWVlM2Q1MmU0Y2Q2MzAwNzhhZGRiMTJiOTBjOTlmNzk2NDMyYmEzNjA3YTlkMzc3YTE4NjA3MmFhYjg3N2ViODdhNmMxMTQyYzU1NGIwMGUifV19XQ== FVPROV1-END
