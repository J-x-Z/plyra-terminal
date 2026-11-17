// SPDX-License-Identifier: GPL-3.0-only
//! Explicit acceptance entry inside the paired production UI executable.
#![cfg(windows)]

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Read,
    os::windows::{fs::MetadataExt, fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
use system_api::{
    windows::windows_identity::{current_principal, Process},
    AuditRecord, BrokerClient, PrivilegedAction,
};
use windows_sys::Win32::{
    Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT},
    System::Threading::{WaitForSingleObject, CREATE_NO_WINDOW},
};

const SLEEPER_ARGUMENT: &str = "--system-probe-sleeper";
const SLEEPER_SECONDS: u64 = 90;
const AUDIT_LIMIT: u64 = 1024 * 1024;

/// Called only by the fixed private CLI branch, before constructing the GUI.
/// There are deliberately no duration, command, or script arguments.
pub fn run_sleeper() {
    thread::sleep(Duration::from_secs(SLEEPER_SECONDS));
}

struct OwnedSleeper {
    child: Child,
}

impl OwnedSleeper {
    fn spawn(executable: &Path) -> Result<Self> {
        let child = Command::new(executable)
            .arg(SLEEPER_ARGUMENT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("starting the fixed native sleeper")?;
        Ok(Self { child })
    }

    fn alive(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    fn cleanup(&mut self) -> Result<Value> {
        let was_running = self.alive()?;
        if was_running {
            // Child::kill uses this child's retained kernel handle, never a PID lookup.
            if let Err(error) = self.child.kill() {
                ensure!(
                    self.child.try_wait()?.is_some(),
                    "cannot stop the owned sleeper: {error}"
                );
            }
        }
        use std::os::windows::io::AsRawHandle;
        wait_exited(self.child.as_raw_handle(), 3000)?;
        let status = self.child.wait().context("reaping the owned sleeper")?;
        Ok(json!({
            "pid": self.child.id(),
            "was_running": was_running,
            "exited": true,
            "exit_code": status.code(),
        }))
    }
}

impl Drop for OwnedSleeper {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn wait_exited(handle: windows_sys::Win32::Foundation::HANDLE, milliseconds: u32) -> Result<()> {
    match unsafe { WaitForSingleObject(handle, milliseconds) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => anyhow::bail!("owned process did not exit within {milliseconds} ms"),
        WAIT_FAILED => anyhow::bail!("process wait failed: {}", std::io::Error::last_os_error()),
        status => anyhow::bail!("unexpected process wait result {status:#x}"),
    }
}

fn check(receipt: &mut Value, name: &str, passed: bool, details: Value) -> Result<()> {
    receipt["checks"].as_array_mut().unwrap().push(json!({
        "name": name,
        "passed": passed,
        "details": details,
    }));
    ensure!(passed, "Windows Broker acceptance failed: {name}");
    Ok(())
}

fn audit_names(directory: &Path) -> Result<BTreeSet<PathBuf>> {
    if !directory.exists() {
        return Ok(BTreeSet::new());
    }
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(directory)?.take(4097) {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            names.insert(entry.path());
        }
        ensure!(
            names.len() <= 4096,
            "too many Broker audit files to inspect"
        );
    }
    Ok(names)
}

fn persistent_audit(
    directory: &Path,
    before: &BTreeSet<PathBuf>,
    caller_sid: &str,
    caller_pid: u32,
) -> Result<Vec<(PathBuf, Vec<AuditRecord>)>> {
    let mut matched = Vec::new();
    for path in audit_names(directory)?.difference(before) {
        // Inspect only newly created, bounded regular files. Do not follow reparse points.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && metadata.len() <= AUDIT_LIMIT,
            "invalid new Broker audit file"
        );
        let mut bytes = Vec::new();
        file.take(AUDIT_LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= AUDIT_LIMIT,
            "Broker audit exceeded its limit"
        );
        let mut records = Vec::<AuditRecord>::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            records.push(serde_json::from_slice(line).context("reading persisted Broker audit")?);
        }
        if !records.is_empty()
            && records.iter().all(|record| {
                record.caller_pid == caller_pid && record.caller_sid.as_deref() == Some(caller_sid)
            })
        {
            matched.push((path.clone(), records));
        }
    }
    Ok(matched)
}

/// Runs from the ordinary, paired glass-terminal.exe, not a separate test executable.
/// Administrator mode is requested only by the explicit CLI argument and may display UAC.
pub fn run(output: &Path, administrator: bool) -> Result<()> {
    fs::create_dir(output).context("system probe output directory must be new")?;
    let receipt_path = output.join(if administrator {
        "administrator-broker.json"
    } else {
        "ordinary-broker.json"
    });
    let mut receipt = json!({
        "schema": "glass-windows-native-broker.v1",
        "administrator_requested": administrator,
        "pid": std::process::id(),
        "started_at_ms": system_api::now_ms(),
        "scope": "Paired production UI; authenticated named-pipe peer and real Windows token identity; only fixed, owned 90-second native sleepers; no service or unrelated process operations.",
        "numeric_uid_semantics": "Legacy u32::MAX display placeholder. Windows authorization uses complete SID, token elevation, desktop session and authenticated pipe PID.",
        "checks": [],
        "passed": false,
    });
    let mut broker = None::<BrokerClient>;
    let mut broker_process = None::<Process>;
    let mut sleepers = Vec::<OwnedSleeper>::new();
    let mut audit_directory = None::<PathBuf>;
    let mut previous_audits = BTreeSet::new();
    let mut caller_sid = String::new();
    let mut executed_target = String::new();

    let outcome: Result<()> = (|| {
        let caller = current_principal()?;
        caller_sid = caller.sid.clone();
        receipt["caller"] = json!({
            "sid": caller.sid,
            "elevated": caller.elevated,
            "session_id": caller.session_id,
        });
        check(
            &mut receipt,
            "ordinary_desktop_ui_token",
            !caller.elevated && caller.session_id != 0,
            json!({"elevated": caller.elevated, "session_id": caller.session_id}),
        )?;
        let executable = std::env::current_exe()?;
        receipt["ui_executable"] = json!({
            "path": executable,
            "sha256": system_api::code_hash_for_path(&executable)?,
        });
        let directory = system_api::default_data_dir()?.join("audit");
        previous_audits = audit_names(&directory)?;
        audit_directory = Some(directory);

        broker = Some(if administrator {
            BrokerClient::spawn_admin()
                .context("explicit UAC administrator Broker authorization")?
        } else {
            BrokerClient::spawn().context("starting the paired ordinary Broker")?
        });
        let client = broker.as_ref().unwrap();
        let identity = client.session_identity()?;
        receipt["identity"] = serde_json::to_value(&identity)?;
        let peer = client.peer_identity();
        receipt["authenticated_pipe_peer"] = json!({"pid": peer.1, "numeric_uid_placeholder": peer.0, "principal": client.peer_principal_label()});
        check(
            &mut receipt,
            "authenticated_pipe_identity",
            identity.broker_pid > 4
                && identity.broker_pid == peer.1
                && identity.caller_pid == std::process::id()
                && identity.caller_sid.as_deref() == Some(caller.sid.as_str())
                && identity
                    .broker_sid
                    .as_ref()
                    .is_some_and(|sid| !sid.is_empty())
                && identity.administrator == administrator
                && client.is_connected()
                && client.is_admin() == administrator,
            json!({"pipe_pid": peer.1, "administrator": identity.administrator}),
        )?;

        // Keep this exact process handle through Revoke and client destruction.
        // A later process with a recycled numeric PID cannot satisfy the exit check.
        broker_process = Some(
            Process::open(identity.broker_pid, false)
                .context("pinning the Broker kernel process and token")?,
        );
        let pinned = broker_process.as_ref().unwrap();
        receipt["kernel_broker"] = json!({
            "process": pinned.identity,
            "sid": pinned.principal.sid,
            "elevated": pinned.principal.elevated,
            "session_id": pinned.principal.session_id,
            "sha256": system_api::code_hash_for_path(Path::new(&pinned.identity.exe))?,
        });
        check(
            &mut receipt,
            "kernel_sid_elevation_session",
            identity.broker_sid.as_deref() == Some(pinned.principal.sid.as_str())
                && pinned.principal.elevated == administrator
                && pinned.principal.session_id == caller.session_id
                && (administrator || pinned.principal.sid == caller.sid),
            json!({"session_id": pinned.principal.session_id, "elevated": pinned.principal.elevated, "sid": pinned.principal.sid}),
        )?;

        sleepers.push(OwnedSleeper::spawn(&executable)?);
        let target = Process::open(sleepers[0].child.id(), false)?;
        let ui = system_api::process_identity(std::process::id())?;
        receipt["stop_target"] = serde_json::to_value(&target.identity)?;
        check(
            &mut receipt,
            "owned_bounded_native_target",
            target.identity.pid != identity.broker_pid
                && target.identity.pid != std::process::id()
                && target.identity.start_time > 0
                && target.identity.exe == ui.exe
                && target.identity.uid == caller.sid
                && !target.principal.elevated
                && target.principal.session_id == caller.session_id
                && sleepers[0].alive()?,
            json!({"lifetime_seconds": SLEEPER_SECONDS, "pid": target.identity.pid}),
        )?;
        executed_target = format!(
            "PID {} SID {} start {} {}",
            target.identity.pid,
            target.identity.uid,
            target.identity.start_time,
            target.identity.exe
        );
        let action = PrivilegedAction::StopProcess(target.identity.clone());
        let cancelled = client.prepare(action.clone())?;
        receipt["cancelled_preparation"] = serde_json::to_value(&cancelled)?;
        client.cancel_pending()?;
        let cancelled_error = client
            .confirm(&cancelled.ticket)
            .err()
            .map(|error| format!("{error:#}"));
        receipt["cancelled_ticket_error"] = json!(cancelled_error);
        let after_cancel = client.session_identity()?;
        receipt["identity_after_cancel"] = serde_json::to_value(&after_cancel)?;
        check(
            &mut receipt,
            "cancel_retains_authorization_and_target",
            cancelled_error.is_some()
                && sleepers[0].alive()?
                && client.is_connected()
                && client.is_admin() == administrator
                && serde_json::to_value(&identity)? == serde_json::to_value(&after_cancel)?,
            json!({"same_broker_pid": after_cancel.broker_pid, "target_alive": true}),
        )?;

        let prepared = client.prepare(action)?;
        receipt["confirmed_preparation"] = serde_json::to_value(&prepared)?;
        let operation = client.confirm(&prepared.ticket)?;
        receipt["operation"] = serde_json::to_value(&operation)?;
        wait_exited(target.raw(), 3000)?;
        let status = sleepers[0]
            .child
            .try_wait()?
            .context("Broker reported success before target exit")?;
        receipt["target_exit_code"] = json!(status.code());
        check(
            &mut receipt,
            "confirmed_kernel_termination",
            operation.success && status.code() == Some(1),
            json!({"exit_code": status.code(), "kernel_process_signalled": true}),
        )?;
        let consumed_error = client
            .confirm(&prepared.ticket)
            .err()
            .map(|error| format!("{error:#}"));
        receipt["consumed_ticket_error"] = json!(consumed_error);
        check(
            &mut receipt,
            "confirmation_is_single_use",
            consumed_error.is_some(),
            Value::Null,
        )?;

        let audit = client.audit()?;
        receipt["audit_before_revoke"] = serde_json::to_value(&audit)?;
        check(
            &mut receipt,
            "audit_matches_owned_operation",
            audit.iter().all(|record| {
                record.caller_pid == std::process::id()
                    && record.caller_sid.as_deref() == Some(caller.sid.as_str())
            }) && audit
                .iter()
                .any(|record| record.operation == "cancel_pending")
                && audit.iter().any(|record| {
                    record.operation == "confirm" && record.target == executed_target
                })
                && audit.iter().any(|record| {
                    record.operation == "execute"
                        && record.target == executed_target
                        && record.result == "completed"
                }),
            json!({"record_count": audit.len()}),
        )?;

        // Revoke a real pending ticket without executing it. This second owned
        // sleeper is killed by our guard after its survival has been checked.
        sleepers.push(OwnedSleeper::spawn(&executable)?);
        let revoke_target = system_api::process_identity(sleepers[1].child.id())?;
        receipt["revoke_target"] = serde_json::to_value(&revoke_target)?;
        let pending = client.prepare(PrivilegedAction::StopProcess(revoke_target))?;
        receipt["revoked_preparation"] = serde_json::to_value(&pending)?;
        client.revoke()?;
        receipt["revoke_completed"] = json!(true);
        let revoked_error = client
            .confirm(&pending.ticket)
            .err()
            .map(|error| format!("{error:#}"));
        receipt["revoked_ticket_error"] = json!(revoked_error);
        let connected = client.is_connected();
        let after_revoke = client.session_identity();
        receipt["after_revoke"] = match &after_revoke {
            Ok(value) => json!({"connected": connected, "identity": value}),
            Err(error) => json!({"connected": connected, "identity_error": format!("{error:#}")}),
        };
        check(
            &mut receipt,
            "revoke_invalidates_ticket_without_executing",
            revoked_error.is_some() && sleepers[1].alive()?,
            json!({"target_alive": true}),
        )?;
        check(
            &mut receipt,
            "revoke_transport_semantics",
            if administrator {
                !connected && after_revoke.is_err()
            } else {
                connected
                    && after_revoke.as_ref().is_ok_and(|value| {
                        value.broker_pid == identity.broker_pid
                            && value.caller_sid == identity.caller_sid
                            && !value.administrator
                    })
            },
            json!({"connected": connected, "administrator": administrator}),
        )?;
        Ok(())
    })();

    // Cleanup precedes all receipt serialization. File I/O failures cannot leave
    // our sleepers running or keep an authenticated Broker transport alive.
    drop(broker.take());
    let mut cleanup_errors = Vec::<String>::new();
    if let Some(process) = broker_process.as_ref() {
        let exited = wait_exited(process.raw(), 3000);
        if let Err(error) = &exited {
            cleanup_errors.push(format!("Broker cleanup: {error:#}"));
        }
        let _ = check(
            &mut receipt,
            "broker_kernel_process_exited",
            exited.is_ok(),
            json!({"pid": process.identity.pid, "error": exited.err().map(|error| format!("{error:#}"))}),
        );
    }
    let mut fixture_cleanup = Vec::new();
    for sleeper in &mut sleepers {
        match sleeper.cleanup() {
            Ok(value) => fixture_cleanup.push(value),
            Err(error) => {
                cleanup_errors.push(format!("sleeper {} cleanup: {error:#}", sleeper.child.id()));
                fixture_cleanup.push(json!({"pid": sleeper.child.id(), "exited": false, "error": format!("{error:#}")}));
            }
        }
    }
    receipt["fixture_cleanup"] = json!(fixture_cleanup);
    // RAII also covers partial initialization and unwinding paths.
    drop(sleepers);

    if let Some(directory) = audit_directory.as_ref() {
        match persistent_audit(directory, &previous_audits, &caller_sid, std::process::id()) {
            Ok(files) => {
                let valid = files.len() == 1
                    && files[0].1.iter().any(|record| {
                        record.operation == "revoke"
                            && record.result == "all pending tickets revoked"
                    })
                    && files[0].1.iter().any(|record| {
                        record.operation == "execute"
                            && record.target == executed_target
                            && record.result == "completed"
                    });
                receipt["persistent_audit"] = json!(files
                    .iter()
                    .map(|(path, records)| json!({"path": path, "records": records}))
                    .collect::<Vec<_>>());
                if let Err(error) = check(
                    &mut receipt,
                    "persistent_audit_includes_revoke_and_execution",
                    valid,
                    json!({"matching_files": files.len()}),
                ) {
                    cleanup_errors.push(format!("{error:#}"));
                }
            }
            Err(error) => cleanup_errors.push(format!("persistent audit: {error:#}")),
        }
    }
    receipt["error"] = json!(outcome.as_ref().err().map(|error| format!("{error:#}")));
    receipt["cleanup_errors"] = json!(cleanup_errors);
    receipt["finished_at_ms"] = json!(system_api::now_ms());
    let passed = outcome.is_ok() && cleanup_errors.is_empty();
    receipt["passed"] = json!(passed);
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    fs::write(&receipt_path, bytes).context("writing the Windows Broker acceptance receipt")?;
    outcome?;
    ensure!(
        passed,
        "Windows Broker cleanup or persistent audit failed; see {}",
        receipt_path.display()
    );
    Ok(())
}

// FVPROV1 19842 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYzRlMTExYzk5YmU2MTFiMTcyYjJiNTdkZTk1OTQ5NWIwOTdjNTlhZTgyYzYyZTM5MDM4NjBkZjYzOWE4MzE3ZSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiODIwOWY1NjZhNzRlYTg1ZDA2Y2ZkNzMxYzVmYWZmY2Y0YTUzY2M0ZjBlZDZhYjgxOGUzZGFjNTZmOTVjMTIxZjZjM2FlY2E3MGE0ODdiODBkYzU4YmRkMTk0OGE1NzkzY2U0NDcyYTkxNDUzYjJmNTY0YmRhY2RkNmYwM2VmMDgifV19XQ== FVPROV1-END
