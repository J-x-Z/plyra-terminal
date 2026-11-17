// SPDX-License-Identifier: GPL-3.0-only
use anyhow::{Context, Result};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Source the user's normal startup files and add display-only OSC 133 events.
/// Arguments and terminal input are excluded from the command timeline.
pub fn configure(command: &mut Command, shell: Option<&str>) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let selected = shell
        .map(str::to_owned)
        .or_else(|| {
            command_environment(command, "SHELL").and_then(|value| value.into_string().ok())
        })
        .unwrap_or_else(|| "/bin/zsh".into());
    if !selected.ends_with("/zsh") {
        return Ok(());
    }
    let home =
        command_environment(command, "HOME").context("HOME is required for shell integration")?;
    let original = command_environment(command, "ZDOTDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home));
    let directory = PathBuf::from(home).join("Library/Application Support/GlassTerminal/shell/zsh");
    fs::create_dir_all(&directory)?;
    write_if_changed(
        directory.join(".zshenv"),
        r#"export ZDOTDIR="$GLASS_SHELL_ORIGINAL"
if [[ -r "$GLASS_SHELL_ORIGINAL/.zshenv" ]]; then
  source "$GLASS_SHELL_ORIGINAL/.zshenv"
fi
if [[ "$ZDOTDIR" != "$GLASS_SHELL_WRAPPER" ]]; then
  export GLASS_SHELL_ORIGINAL="$ZDOTDIR"
fi
export ZDOTDIR="$GLASS_SHELL_WRAPPER"
"#,
    )?;
    write_if_changed(
        directory.join(".zshrc"),
        r#"export ZDOTDIR="$GLASS_SHELL_ORIGINAL"
if [[ -r "$GLASS_SHELL_ORIGINAL/.zshrc" ]]; then
  source "$GLASS_SHELL_ORIGINAL/.zshrc"
fi
function __glass_command_start() {
  local program="${1%% *}"
  case "$program" in
    (*[^a-zA-Z0-9_./-]*|'') program=shell ;;
  esac
  if (( ${#program} > 128 )); then program=shell; fi
  printf '\033]133;C;%s\007' "$program"
}
function __glass_command_finish() {
  local command_status=$?
  printf '\033]133;D;%s\007\033]133;A\007' "$command_status"
}
typeset -ga preexec_functions precmd_functions
preexec_functions=(__glass_command_start ${preexec_functions:#__glass_command_start})
precmd_functions=(__glass_command_finish ${precmd_functions:#__glass_command_finish})
"#,
    )?;
    write_if_changed(
        directory.join(".zprofile"),
        r#"export ZDOTDIR="$GLASS_SHELL_ORIGINAL"
if [[ -r "$GLASS_SHELL_ORIGINAL/.zprofile" ]]; then
  source "$GLASS_SHELL_ORIGINAL/.zprofile"
fi
export GLASS_SHELL_ORIGINAL="$ZDOTDIR"
export ZDOTDIR="$GLASS_SHELL_WRAPPER"
"#,
    )?;
    command
        .env("GLASS_SHELL_ORIGINAL", original)
        .env("GLASS_SHELL_WRAPPER", &directory)
        .env("ZDOTDIR", &directory);
    Ok(())
}

fn command_environment(command: &Command, name: &str) -> Option<OsString> {
    match command.get_envs().find(|(key, _)| *key == OsStr::new(name)) {
        Some((_, value)) => value.map(OsStr::to_os_string),
        None => env::var_os(name),
    }
}

fn write_if_changed(path: PathBuf, contents: &str) -> Result<()> {
    if fs::read(&path).ok().as_deref() == Some(contents.as_bytes()) {
        return Ok(());
    }
    let sequence = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

// FVPROV1 3964 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMTFjZDk4ZWMxZGM1MDEyMDlmOTQ4OGFhNTIxMTZmZmM1ZWVjNmM2Zjc5MmVjYzdmYWJiZDdjNjcyYjhkMTJhNyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOTM2OTlhODc5MWU1NjU4OTI5YjdiMTFjMWE2ZWJkZTE1MDBiNmQyZDc1NTUyZDZhNDI3NDhjYTk3NTI5YzQwMDU4MWE1YTJjNjgzNzExZThjZjg2N2ZlZTBmZGFkZjg3YTJhYjEwYTAwOGUyNmFhOWI5NGQ4NzEyMGVjOTc1MDYifV19XQ== FVPROV1-END
