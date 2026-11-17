// SPDX-License-Identifier: GPL-3.0-only
//! Exercise a real Aether Shell through the existing PTY and typed event paths.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use session_manager::{Backend, Session, SessionEvent, SessionOptions, ShellIntegration};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

fn wait_for(
    session: &Session,
    wake: &mpsc::Receiver<()>,
    events: &mut Vec<Value>,
    description: &str,
    condition: impl Fn(&[Value]) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        for event in session.take_events() {
            match event {
                SessionEvent::Shell { event, .. } => events.push(event.data),
                SessionEvent::ShellWarning(message) | SessionEvent::Error(message) => {
                    bail!("{message}")
                }
                SessionEvent::Ready {
                    helper_pid,
                    shell_pid,
                    shell,
                } => events.push(json!({
                    "type":"terminal.ready", "helper_pid":helper_pid,
                    "shell_pid":shell_pid, "shell":shell,
                })),
                SessionEvent::Exit(status) => {
                    events.push(json!({"type":"terminal.exit", "status":status}))
                }
                SessionEvent::Command { .. } => {}
            }
        }
        if condition(events) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "timed out waiting for {description}");
        let _ = wake.recv_timeout(remaining);
    }
}

fn result(events: &[Value], request_id: u64) -> Option<&Value> {
    events
        .iter()
        .find(|event| event["type"] == "operation.result" && event["request_id"] == request_id)
}

fn exercise(
    session: &mut Session,
    wake: &mpsc::Receiver<()>,
    events: &mut Vec<Value>,
) -> Result<Value> {
    wait_for(session, wake, events, "authenticated prompt", |events| {
        events.iter().any(|event| event["type"] == "prompt.ready")
    })?;
    ensure!(
        session.has_shell_operations(),
        "Shell operation channel unavailable"
    );
    session.input(b"/bin/sleep 20 &\r")?;
    wait_for(session, wake, events, "owned background task", |events| {
        events
            .iter()
            .any(|event| event["type"] == "task.started" && event["background"] == true)
    })?;
    let task = events
        .iter()
        .find(|event| event["type"] == "task.started" && event["background"] == true)
        .unwrap();
    let task_id = task["task_id"].as_u64().context("missing owned task ID")?;
    let pgid = task["pgid"]
        .as_u64()
        .context("missing owned process group")?;
    session.shell_operation(1, "task.list", &[])?;
    wait_for(session, wake, events, "typed task list", |events| {
        result(events, 1).is_some()
    })?;
    let listed = result(events, 1).unwrap();
    ensure!(listed["status"] == 0, "task.list failed: {listed}");
    let jobs = listed["result"]["jobs"]
        .as_array()
        .context("task.list result has no jobs array")?;
    ensure!(
        jobs.iter().any(|job| {
            (job["id"] == task_id || job["task_id"] == task_id) && job["pgid"] == pgid
        }),
        "typed task list lost task identity or process group"
    );
    session.shell_operation(2, "task.signal", &[task_id.to_string(), "TERM".into()])?;
    wait_for(session, wake, events, "owned task completion", |events| {
        result(events, 2).is_some()
            && events.iter().any(|event| {
                event["type"] == "task.state"
                    && event["task_id"] == task_id
                    && event["state"] == "completed"
            })
    })?;
    ensure!(
        result(events, 2).unwrap()["status"] == 0,
        "typed task.signal failed"
    );
    #[cfg(unix)]
    ensure!(
        unsafe { libc::kill(-(pgid as i32), 0) } != 0,
        "completed task group is still alive"
    );
    session.input(b"exit 0\r")?;
    wait_for(session, wake, events, "normal Shell exit", |events| {
        events
            .iter()
            .any(|event| event["type"] == "terminal.exit" && event["status"] == 0)
    })?;
    Ok(
        json!({"task_id":task_id,"pgid":pgid,"typed_list":true,"typed_signal":true,"completed_group_gone":true}),
    )
}

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let shell = PathBuf::from(
        arguments
            .next()
            .context("provide the Aether Shell executable path")?,
    );
    ensure!(
        shell.is_absolute() && shell.is_file(),
        "Shell must be an absolute executable file"
    );
    let backend_name = arguments
        .next()
        .context("provide helper or portable backend")?;
    let backend = match backend_name.as_str() {
        "helper" => Backend::Helper,
        "portable" => Backend::Portable,
        _ => bail!("backend must be helper or portable"),
    };
    let directory = arguments
        .next()
        .context("provide a new evidence directory")?;
    let directory = Path::new(&directory);
    fs::create_dir(directory).context("evidence directory must be new")?;
    ensure!(arguments.next().is_none(), "unexpected extra arguments");
    let (notify, wake) = mpsc::sync_channel(1);
    let mut session = Session::spawn(
        SessionOptions {
            backend,
            shell: Some(shell.to_string_lossy().into_owned()),
            login: Some(false),
            shell_integration: ShellIntegration::AetherV1,
            ..SessionOptions::default()
        },
        Arc::new(move || {
            let _ = notify.try_send(());
        }),
    )?;
    let mut events = Vec::new();
    let started = Instant::now();
    let outcome = exercise(&mut session, &wake, &mut events);
    let closed = session.close();
    let receipt = json!({
        "schema":"glass.aether-shell.native-pty-acceptance.v1",
        "passed":outcome.is_ok() && closed.is_ok(),
        "os":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "backend":backend_name,"shell":shell,
        "elapsed_ms":started.elapsed().as_millis(),
        "checks":outcome.as_ref().ok(),
        "error":outcome.as_ref().err().map(|error| format!("{error:#}")),
        "close_error":closed.as_ref().err().map(|error| format!("{error:#}")),
        "events":events,
    });
    fs::write(
        directory.join("acceptance.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    outcome?;
    closed
}

// FVPROV1 6752 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTMyYWE3YmJiOWMzYjFhZGM4NjhmYWIzNTFhZDE4NjczN2E1OTlkNzI5OGZmMGRhOTNjOTQzNzQ2MThhNTI0YyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYmY4NjI1MDJmYzUyZjFjZmMwMzljZDNlYzViZDZjYzk5YTIzMTE4NmExMjNkYmI1MjVlZWNkYTE3YTIwOTE1MGY4ZTg5MjU3YTVjOWRlZjM5ZWRmNGQ4M2Q5ZDc0NTY1MzJhYzNhY2M5NmMyNjFhMjM5OTU0YWUwZjVkYTBlMGIifV19XQ== FVPROV1-END
