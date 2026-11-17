// SPDX-License-Identifier: GPL-3.0-only
//! Run actual Vim, an isolated tmux server, and an optional btop in a real PTY.
use anyhow::{ensure, Context, Result};
use session_manager::{Backend, Session, SessionEvent, SessionOptions};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn wait_for(
    session: &Session,
    description: &str,
    condition: impl Fn(&terminal_core::Screen) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        for event in session.take_events() {
            if let SessionEvent::Error(message) = event {
                anyhow::bail!("{message}");
            }
        }
        if condition(&session.snapshot()) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("timed out waiting for {description}");
}

struct TmuxServer(String);
impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("/opt/homebrew/bin/tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn main() -> Result<()> {
    ensure!(cfg!(target_os = "macos"), "this smoke test targets macOS");
    let directory = std::env::args()
        .nth(1)
        .context("provide a new evidence directory")?;
    let directory = Path::new(&directory);
    fs::create_dir(directory).context("evidence directory must be new")?;
    let evidence_root = directory.canonicalize()?;
    let directory = evidence_root.as_path();
    let file = directory.join("vim-utf8.txt");
    fs::write(&file, "initial\n")?;
    let quoted_file = shell_quote(&file);
    let mut session = Session::spawn(
        SessionOptions {
            backend: Backend::Helper,
            shell: Some("/bin/sh".into()),
            ..SessionOptions::default()
        },
        Arc::new(|| {}),
    )?;
    session.input(b"stty -echo; printf '__READY__\\n'\r")?;
    wait_for(&session, "shell", |screen| {
        screen.plain_text().contains("__READY__")
    })?;

    session.input(format!("/usr/bin/vim -Nu NONE -n -i NONE -N -- {quoted_file}\r").as_bytes())?;
    wait_for(&session, "Vim alternate screen", |screen| screen.alternate)?;
    session.input("iMAC_VIM_中文_e\u{301}\x1b".as_bytes())?;
    wait_for(&session, "Vim UTF-8 edit", |screen| {
        screen.plain_text().contains("MAC_VIM_中文_e\u{301}")
    })?;
    fs::write(
        directory.join("vim-screen.json"),
        serde_json::to_vec_pretty(&session.snapshot())?,
    )?;
    session.input(b":wq\r")?;
    wait_for(&session, "Vim exit", |screen| !screen.alternate)?;
    ensure!(
        fs::read_to_string(&file)?.contains("MAC_VIM_中文_e\u{301}initial"),
        "Vim saved unexpected text"
    );

    let mut tmux_passed = false;
    if Path::new("/opt/homebrew/bin/tmux").is_file() {
        let server = TmuxServer(format!("glass-smoke-{}", std::process::id()));
        let command = format!(
            "/opt/homebrew/bin/tmux -u -L {} -f /dev/null new-session -s smoke 'exec /bin/sh'\r",
            server.0
        );
        session.input(command.as_bytes())?;
        wait_for(&session, "tmux alternate screen", |screen| screen.alternate)?;
        session.input("printf '__TMUX_%s__\\n' 中文\r".as_bytes())?;
        wait_for(&session, "tmux UTF-8 output", |screen| {
            screen.plain_text().contains("__TMUX_中文__")
        })?;
        fs::write(
            directory.join("tmux-screen.json"),
            serde_json::to_vec_pretty(&session.snapshot())?,
        )?;
        session.input(b"exit\r")?;
        wait_for(&session, "tmux exit", |screen| !screen.alternate)?;
        tmux_passed = true;
    }
    let btop = std::env::var_os("GLASS_BTOP_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/bin/btop"));
    let mut btop_passed = false;
    let mut btop_version = None;
    if btop.is_file() {
        let version = Command::new(&btop).arg("--version").output()?;
        ensure!(version.status.success(), "btop version query failed");
        btop_version = Some(String::from_utf8_lossy(&version.stdout).into_owned());
        let config_home = directory.join("btop-config");
        let state_home = directory.join("btop-state");
        fs::create_dir_all(config_home.join("btop"))?;
        fs::create_dir(&state_home)?;
        fs::write(config_home.join("btop/btop.conf"),
            "update_ms = 200\ncheck_temp = False\nshow_disks = False\nsave_config_on_exit = False\n")?;
        session.resize(120, 36)?;
        session.input(format!(
            "XDG_CONFIG_HOME={} XDG_STATE_HOME={} {} --force-utf --update 200 --filter btop; printf '__BTOP_EXIT_%s__\\n' \"$?\"\r",
            shell_quote(&config_home), shell_quote(&state_home), shell_quote(&btop)
        ).as_bytes())?;
        wait_for(&session, "btop live dashboard", |screen| {
            let text = screen.plain_text().to_ascii_lowercase();
            screen.alternate
                && text.contains("cpu")
                && text.contains("mem")
                && text.contains("proc")
        })?;
        let initial = session.snapshot();
        ensure!(
            initial
                .lines
                .iter()
                .flat_map(|line| &line.runs)
                .any(|run| run
                    .text
                    .chars()
                    .any(|character| ('\u{2500}'..='\u{259f}').contains(&character))),
            "btop did not render its Unicode border/graph grid"
        );
        fs::write(
            directory.join("btop-screen.json"),
            serde_json::to_vec_pretty(&initial)?,
        )?;
        session.resize(100, 30)?;
        wait_for(&session, "btop resize redraw", |screen| {
            let text = screen.plain_text().to_ascii_lowercase();
            screen.alternate
                && screen.cols == 100
                && screen.rows == 30
                && text.contains("cpu")
                && text.contains("proc")
        })?;
        fs::write(
            directory.join("btop-resize-screen.json"),
            serde_json::to_vec_pretty(&session.snapshot())?,
        )?;
        session.input(b"q")?;
        wait_for(
            &session,
            "btop clean quit and primary-screen restoration",
            |screen| !screen.alternate && screen.plain_text().contains("__BTOP_EXIT_0__"),
        )?;
        btop_passed = true;
    } else {
        ensure!(
            std::env::var_os("GLASS_BTOP_BIN").is_none(),
            "explicit btop executable does not exist"
        );
    }
    session.close()?;
    let result = serde_json::json!({
        "schema":"glass-terminal.macos-tui-smoke","passed":true,
        "platform":"macos","arch":std::env::consts::ARCH,
        "backend":"helper","native_pty":true,"native_gui":false,
        "vim_utf8_edit_save_and_alternate":true,"tmux_utf8_and_alternate":tmux_passed,
        "tmux_status":if tmux_passed {"Verified"} else {"Not tested - executable missing"},
        "btop":if btop_passed {"Verified"} else {"Not tested - executable missing"},
        "btop_resize_and_quit":btop_passed,
        "btop_executable":btop_passed.then(|| btop.to_string_lossy().into_owned()),
        "btop_version_output":btop_version,
        "acceptance_requires_actual_programs":true,
    });
    fs::write(
        directory.join("RESULT.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!("Vim smoke passed, tmux passed={tmux_passed}, btop passed={btop_passed}");
    Ok(())
}

// FVPROV1 7722 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNzA5MDU5ZjQzMDdmMmRmYTJkYTFjYmE2Y2M1NGRjMDBiNGI1ZDQ1MjEzOWQwZjMwNzhmYjViNWQ1M2U4MWUzNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNDMwZDkzODQyZTI5NWMzM2MyOTNmZDU1YTU1ZjhkM2JiNTIyYWI2ZTc0Y2Y0ZTYzNGM4ZDY0ODg3MTMwYTE3ZDY3ZDE5MmNjMjQ0MTczYWQwZTcxODk3M2Y2MmFmNTIxM2M5NzhjNmZjOWYwNTZhMjY0M2JiMzNlZDM1YjJlMGUifV19XQ== FVPROV1-END
