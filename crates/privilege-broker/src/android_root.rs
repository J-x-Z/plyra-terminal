// SPDX-License-Identifier: GPL-3.0-only
//! Startup identity checks for the fixed APK broker launched by Magisk or KernelSU.
use anyhow::{ensure, Context, Result};
use std::{io::Read, path::PathBuf};
use system_api::{android_root::*, current_uid, ipc::Bootstrap};

pub(super) fn valid_arguments(arguments: &[String]) -> bool {
    arguments.len() == 12
        && arguments[0] == "--android-bootstrap"
        && arguments[2] == "--expected-ui-sha256"
        && arguments[4] == "--expected-ui-start-time"
        && arguments[6] == "--expected-broker-sha256"
        && arguments[8] == "--expected-ui-library"
        && arguments[10] == "--expected-ui-library-sha256"
}

pub(super) fn validate_startup(arguments: &[String], bootstrap: &Bootstrap) -> Result<()> {
    ensure!(
        valid_arguments(arguments) && current_uid() == 0 && bootstrap.administrator,
        "Android administrator startup requires an authorized root manager"
    );
    for index in [3, 7, 11] {
        ensure!(
            arguments[index].len() == 64
                && arguments[index]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Android executable identity is malformed"
        );
    }
    let expected_start = arguments[5].parse::<u64>()?;
    ensure!(
        expected_start > 0
            && process_start_time(bootstrap.parent_pid)? == expected_start
            && process_uid(bootstrap.parent_pid)? == bootstrap.parent_uid,
        "Android UI process lifetime or UID changed"
    );
    let mut command_line = Vec::new();
    std::fs::File::open(format!("/proc/{}/cmdline", bootstrap.parent_pid))?
        .take(4097)
        .read_to_end(&mut command_line)?;
    ensure!(
        command_line.len() <= 4096
            && command_line.split(|byte| *byte == 0).next() == Some(PACKAGE_NAME.as_bytes()),
        "Android broker caller is not the declared Activity process"
    );
    let executable = std::env::current_exe()?.canonicalize()?;
    let directory = executable
        .parent()
        .context("Android package native directory unavailable")?;
    ensure!(
        executable.file_name().and_then(|name| name.to_str()) == Some(BROKER_LIBRARY),
        "Android root agent must execute from its packaged library name"
    );
    verify_packaged_binary(&executable, directory)?;
    ensure!(
        file_hash(&executable)? == arguments[7],
        "Android packaged broker identity changed"
    );
    let library = PathBuf::from(&arguments[9]);
    ensure!(
        library.file_name().and_then(|name| name.to_str()) == Some(UI_LIBRARY),
        "Android UI native library name is invalid"
    );
    verify_packaged_binary(&library, directory)?;
    ensure!(
        file_hash(&library)? == arguments[11],
        "Android UI native library identity changed"
    );
    let mut maps = String::new();
    std::fs::File::open(format!("/proc/{}/maps", bootstrap.parent_pid))?
        .take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut maps)?;
    let library = library
        .to_str()
        .context("Android UI library path is not UTF-8")?;
    ensure!(
        maps.len() <= 16 * 1024 * 1024 && maps.lines().any(|line| line.ends_with(library)),
        "Android caller has not loaded the fixed UI native library"
    );
    let user = bootstrap.parent_uid / 100_000;
    ensure!(
        bootstrap.parent_uid % 100_000 >= 10_000,
        "Android caller is outside the ordinary application UID range"
    );
    let files = PathBuf::from(format!("/data/user/{user}/{PACKAGE_NAME}/files")).canonicalize()?;
    let socket = PathBuf::from(&bootstrap.socket_path);
    ensure!(
        socket
            .parent()
            .and_then(|path| path.parent())
            .map(|path| path.canonicalize())
            .transpose()?
            .as_deref()
            == Some(files.as_path())
            && PathBuf::from(&arguments[1]).parent() == socket.parent(),
        "Android bootstrap and IPC directory leave the app's private files directory"
    );
    let expected_audit = files.join("state/audit").canonicalize()?;
    ensure!(
        PathBuf::from(&bootstrap.audit_path)
            .parent()
            .map(|path| path.canonicalize())
            .transpose()?
            .as_deref()
            == Some(expected_audit.as_path()),
        "Android administrator audit directory leaves the application state directory"
    );
    ensure!(
        process_start_time(bootstrap.parent_pid)? == expected_start,
        "Android UI process changed during startup authentication"
    );
    Ok(())
}

// FVPROV1 4583 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZjhlYmNiMTY3ODIxZTVmMGExYzJmODU2YTBmOTRkMmU1ZWMzNDgwZGU4NDA1Nzg2Y2IyYjkwMGQzMDFlN2YxNiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMzdhMjdlNDg1NDI3OGUxMTQzOTNkZWE0MGI3NjNkNDBjNzQ3ZmZhZjk1NGRjMjM2ZWE5YjNkYWEwNmE1MzdhZTNiNzQ2ZDQ5OGVhMTljNGNjMjU4ODQ5MGMxZDEzNDAyNDE0ZWRmNmQwNjFmZTI0MzE4OTk3MmRhMWI0MDFmMGYifV19XQ== FVPROV1-END
