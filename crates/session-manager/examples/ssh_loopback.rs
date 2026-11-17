// SPDX-License-Identifier: GPL-3.0-only
//! Exercise OpenSSH through the normal structured PTY command entry point.
use anyhow::{ensure, Context, Result};
use session_manager::{Backend, Session, SessionEvent, SessionOptions};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn wait_for(
    session: &Session,
    description: &str,
    predicate: impl Fn(&terminal_core::Screen) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        for event in session.take_events() {
            if let SessionEvent::Error(error) = event {
                anyhow::bail!("{error}");
            }
        }
        if predicate(&session.snapshot()) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("Timed out waiting for {description}")
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 5,
        "Expected PORT USER IDENTITY KNOWN_HOSTS NEW_EVIDENCE_DIRECTORY"
    );
    let port: u16 = args[0].parse()?;
    ensure!(port > 0, "Port must be nonzero");
    let evidence = PathBuf::from(&args[4]);
    fs::create_dir(&evidence).context("Evidence directory must be new")?;
    let mut session = Session::spawn(
        SessionOptions {
            backend: Backend::Helper,
            command: Some(vec![
                "/usr/bin/ssh".into(),
                "-tt".into(),
                "-F".into(),
                "/dev/null".into(),
                "-i".into(),
                args[2].clone(),
                "-o".into(),
                "IdentitiesOnly=yes".into(),
                "-o".into(),
                "StrictHostKeyChecking=yes".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", args[3]),
                "-p".into(),
                port.to_string(),
                "-l".into(),
                args[1].clone(),
                "--".into(),
                "127.0.0.1".into(),
            ]),
            ..SessionOptions::default()
        },
        Arc::new(|| {}),
    )?;
    session.input("stty -echo; printf '\\n__SSH_中文_e\u{301}__\\n'\r".as_bytes())?;
    wait_for(&session, "remote UTF-8 output", |screen| {
        screen
            .plain_text()
            .lines()
            .any(|line| line.trim() == "__SSH_中文_e\u{301}__")
    })?;
    session.resize(92, 28)?;
    thread::sleep(Duration::from_millis(200));
    session.input(b"printf '\\n__SSH_SIZE_'; stty size\r")?;
    wait_for(&session, "remote resize", |screen| {
        screen.plain_text().contains("__SSH_SIZE_28 92")
    })?;
    session.input(b"sleep 30\r")?;
    thread::sleep(Duration::from_millis(100));
    session.input(b"\x03")?;
    session.input(b"printf '\\n__SSH_INTERRUPTED__\\n'\r")?;
    wait_for(&session, "remote Ctrl+C", |screen| {
        screen
            .plain_text()
            .lines()
            .any(|line| line.trim() == "__SSH_INTERRUPTED__")
    })?;
    fs::write(
        evidence.join("screen.json"),
        serde_json::to_vec_pretty(&session.snapshot())?,
    )?;
    session.input(b"exit\r")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < deadline && !exited {
        exited = session
            .take_events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Exit(0)));
        thread::sleep(Duration::from_millis(10));
    }
    session.close()?;
    ensure!(exited, "SSH did not exit cleanly");
    fs::write(
        evidence.join("RESULT.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "glass.ssh-loopback-smoke", "passed": true,
            "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "backend": "native PTY + /usr/bin/ssh + user-owned loopback sshd",
            "utf8": true, "resize": true, "ctrl_c": true, "clean_exit": true,
            "server_binding": "127.0.0.1 only", "native_gui": false
        }))?,
    )?;
    println!("SSH UTF-8, resize, Ctrl+C and clean exit passed");
    Ok(())
}

// FVPROV1 4209 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYzgwODViZmFiOTQxMjY2ZmQyMWU3YjZlYjZmMDZmOTYwZTA0MjQ2ZmU3YTZhZWY4YTk3OGMxZGU2NzBmYWM2YSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNGJhZjA2MjhiOWQwM2RmMTJmODU2NDI1YWQxNzFiNzZhYjAwMzI4ZjA1MzNmNjYwOGVjMzExMzM1NGIwNWIwZDFlZWE1ZWE2YTA5ZDAzOTU3YmY4Njc2MTg0ZWIwZmQ3MmJhOTc4MzZmMTY0NjBiOWQ1N2I1ZTYxMjIyM2YxMDMifV19XQ== FVPROV1-END
