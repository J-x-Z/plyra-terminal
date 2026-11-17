// SPDX-License-Identifier: GPL-3.0-only
#[cfg(target_os = "android")]
mod android_root;

#[cfg(target_os = "macos")]
mod macos_admin;
#[cfg(windows)]
mod windows;
use anyhow::Result;
#[cfg(unix)]
use anyhow::{ensure, Context};
#[cfg(unix)]
use privilege_policy::Caller;
#[cfg(unix)]
use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{BufReader, Write},
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use system_api::{ipc::*, *};

pub fn run() -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixListener,
        };
        let arguments: Vec<_> = std::env::args().skip(1).collect();
        #[cfg(target_os = "macos")]
        if let Some(result) = macos_admin::dispatch(&arguments) {
            return result;
        }
        #[cfg(not(target_os = "linux"))]
        let expected_ui_start_time = None;
        #[cfg(target_os = "linux")]
        let mut expected_ui_start_time = None;
        let (bootstrap, expected_ui_hash): (Bootstrap, Option<String>) = if arguments.is_empty() {
            (
                read_message(&mut BufReader::new(std::io::stdin().lock()))?
                    .context("missing broker bootstrap")?,
                None,
            )
        } else {
            #[cfg(target_os = "macos")]
            let valid_arguments = arguments.len() == 4
                && arguments[0] == "--admin-bootstrap"
                && arguments[2] == "--expected-ui-cdhash";
            #[cfg(target_os = "linux")]
            let valid_arguments = arguments.len() == 6
                && arguments[0] == "--polkit-bootstrap"
                && arguments[2] == "--expected-ui-sha256"
                && arguments[4] == "--expected-ui-start-time";
            #[cfg(target_os = "android")]
            let valid_arguments = android_root::valid_arguments(&arguments);
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
            let valid_arguments = false;
            ensure!(
                valid_arguments && current_uid() == 0,
                "invalid administrator startup"
            );
            #[cfg(target_os = "linux")]
            {
                expected_ui_start_time = Some(arguments[5].parse::<u64>()?);
                ensure!(
                    arguments[3].len() == 64
                        && arguments[3].bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid UI executable identity"
                );
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&arguments[1])?;
            ensure!(
                file.metadata()?.is_file()
                    && file.metadata()?.len() <= MAX_MESSAGE as u64
                    && file.metadata()?.permissions().mode() & 0o077 == 0,
                "administrator bootstrap is not private"
            );
            let owner = file.metadata()?.uid();
            let bootstrap: Bootstrap = read_message(&mut BufReader::new(file))?
                .context("missing administrator bootstrap")?;
            ensure!(
                owner == bootstrap.parent_uid && bootstrap.administrator,
                "administrator bootstrap ownership mismatch"
            );
            (bootstrap, Some(arguments[3].clone()))
        };
        ensure!(
            bootstrap.parent_uid != 0 && bootstrap.parent_pid > 1,
            "invalid normal-user UI identity"
        );
        if bootstrap.administrator {
            ensure!(
                current_uid() == 0 && expected_ui_hash.is_some(),
                "administrator mode requires native platform authorization"
            );
            #[cfg(target_os = "android")]
            android_root::validate_startup(&arguments, &bootstrap)?;
            let executable = std::env::current_exe()?;
            #[cfg(target_os = "macos")]
            {
                let metadata = executable.symlink_metadata()?;
                let parent_metadata = executable
                    .parent()
                    .context("administrator stage unavailable")?
                    .symlink_metadata()?;
                ensure!(
                    metadata.uid() == 0
                        && metadata.is_file()
                        && metadata.permissions().mode() & 0o022 == 0
                        && parent_metadata.uid() == 0
                        && parent_metadata.is_dir()
                        && parent_metadata.permissions().mode() & 0o022 == 0,
                    "administrator broker stage is not root-owned and protected from writes"
                );
            }
            #[cfg(target_os = "linux")]
            {
                ensure!(
                    executable == linux_admin::verify_installation()?,
                    "administrator broker is outside its installed path"
                );
                let invoking_uid = std::env::var("PKEXEC_UID")?.parse::<u32>()?;
                ensure!(
                    invoking_uid == bootstrap.parent_uid
                        && unsafe { libc::getppid() } as u32 == bootstrap.parent_pid,
                    "polkit caller does not match the registered UI"
                );
                ensure!(
                    Some(linux_admin::process_start_time(bootstrap.parent_pid)?)
                        == expected_ui_start_time,
                    "authenticated UI process lifetime changed"
                );
            }
            ensure!(
                running_code_hash(std::process::id())? == code_hash_for_path(&executable)?,
                "administrator broker executable identity changed"
            );
        } else {
            ensure!(
                bootstrap.parent_uid == current_uid()
                    && bootstrap.parent_pid == unsafe { libc::getppid() } as u32,
                "ordinary broker parent identity mismatch"
            );
        }
        ensure!(
            bootstrap.nonce.len() == 64
                && bootstrap.nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid broker nonce"
        );
        let socket = std::path::PathBuf::from(&bootstrap.socket_path);
        let parent = socket.parent().context("missing private IPC directory")?;
        let metadata = std::fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == bootstrap.parent_uid
                && metadata.permissions().mode() & 0o077 == 0,
            "broker IPC directory is not private"
        );
        ensure!(
            socket.file_name().and_then(|name| name.to_str()) == Some("ipc.sock"),
            "invalid broker socket path"
        );
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        if bootstrap.administrator {
            let path = std::ffi::CString::new(bootstrap.socket_path.as_str())?;
            ensure!(
                unsafe { libc::chown(path.as_ptr(), bootstrap.parent_uid, u32::MAX) } == 0,
                "cannot grant the authenticated UI access to its private socket"
            );
        }
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(
                        Instant::now() < deadline,
                        "broker client connection timed out"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_nonblocking(false)?;
        let result = serve_session(
            BufReader::new(stream),
            bootstrap,
            expected_ui_hash,
            expected_ui_start_time,
            || Ok(()),
        );
        std::fs::remove_file(socket).ok();
        result
    }
    #[cfg(windows)]
    {
        windows::run()
    }
    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!("kernel-authenticated broker backend is implemented for Unix")
    }
}

#[cfg(unix)]
fn serve_session(
    mut reader: BufReader<std::os::unix::net::UnixStream>,
    bootstrap: Bootstrap,
    expected_ui_hash: Option<String>,
    expected_ui_start_time: Option<u64>,
    mut on_revoke: impl FnMut() -> Result<()>,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    #[cfg(not(target_os = "linux"))]
    let _ = expected_ui_start_time;
    let (uid, pid) = peer_identity(reader.get_ref())?;
    reader
        .get_mut()
        .set_write_timeout(Some(Duration::from_secs(5)))?;
    let audit_path = std::path::Path::new(&bootstrap.audit_path);
    let audit_directory_path = audit_path.parent().context("audit directory missing")?;
    let audit_directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(audit_directory_path)?;
    ensure!(
        audit_path.is_absolute()
            && audit_directory.metadata()?.uid() == bootstrap.parent_uid
            && audit_directory.metadata()?.permissions().mode() & 0o077 == 0,
        "invalid broker audit location"
    );
    if bootstrap.administrator {
        #[cfg(target_os = "macos")]
        {
            let account = unsafe { libc::getpwuid(bootstrap.parent_uid) };
            ensure!(
                !account.is_null(),
                "authenticated UI account is unavailable"
            );
            let home = unsafe { std::ffi::CStr::from_ptr((*account).pw_dir) }.to_str()?;
            let expected_directory =
                std::path::Path::new(home).join("Library/Application Support/GlassTerminal/audit");
            ensure!(
                audit_path.parent().unwrap().canonicalize()?
                    == expected_directory.canonicalize()?,
                "administrator audit path is outside the caller audit directory"
            );
        }
        #[cfg(target_os = "linux")]
        {
            // The opened directory is already caller-owned, private and
            // O_NOFOLLOW-checked. Keep XDG_STATE_HOME support without using
            // the elevated process' HOME or other environment values.
            ensure!(
                audit_directory_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some("audit")
                    && audit_directory_path
                        .parent()
                        .and_then(|path| path.file_name())
                        .and_then(|name| name.to_str())
                        == Some("glass-terminal"),
                "administrator audit path is outside the application state directory"
            );
        }
    }
    use std::os::fd::{AsRawFd, FromRawFd};
    let audit_name = std::ffi::CString::new(
        audit_path
            .file_name()
            .context("audit file name missing")?
            .to_str()
            .context("audit name is not UTF-8")?,
    )?;
    let fd = unsafe {
        libc::openat(
            audit_directory.as_raw_fd(),
            audit_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    ensure!(
        fd >= 0,
        "cannot create private audit record: {}",
        std::io::Error::last_os_error()
    );
    let audit = unsafe { File::from_raw_fd(fd) };
    if bootstrap.administrator {
        // The caller owns the enclosing 0700 directory. Keep the log
        // root-owned and grant read access through that private directory.
        audit.set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    let mut state = BrokerState {
        caller: Caller {
            uid,
            pid,
            broker_pid: std::process::id(),
            administrator: bootstrap.administrator,
        },
        pending: HashMap::new(),
        audit,
        recent: VecDeque::new(),
        audit_bytes: 0,
    };
    if uid != bootstrap.parent_uid || pid != bootstrap.parent_pid {
        state.record_control(
            "authentication",
            "kernel peer UID/PID",
            "denied caller outside the registered UI process",
        )?;
        anyhow::bail!("broker rejected a caller outside its authenticated UI process");
    }
    if let Some(expected) = expected_ui_hash {
        if running_code_hash(pid).ok().as_deref() != Some(&expected) {
            #[cfg(target_os = "macos")]
            let identity_source = "running UI code signature";
            #[cfg(not(target_os = "macos"))]
            let identity_source = "running UI executable identity";
            state.record_control(
                "authentication",
                identity_source,
                "denied different signed UI identity",
            )?;
            anyhow::bail!("administrator broker rejected a different UI code identity");
        }
        #[cfg(target_os = "linux")]
        ensure!(
            Some(linux_admin::process_start_time(pid)?) == expected_ui_start_time,
            "authenticated UI process lifetime changed during the handshake"
        );
    }
    let mut expected_sequence = 1;
    loop {
        let envelope = match read_message::<Envelope>(&mut reader) {
            Ok(Some(envelope)) => envelope,
            Ok(None) => break,
            Err(error) => {
                state.record_control(
                    "schema",
                    "UI IPC session",
                    "denied malformed or oversized request",
                )?;
                return Err(error);
            }
        };
        if envelope.nonce != bootstrap.nonce || envelope.sequence != expected_sequence {
            state.record_control(
                "authentication",
                "UI IPC session",
                "denied invalid nonce or replayed sequence",
            )?;
            anyhow::bail!("broker authentication or sequence check failed");
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .context("broker sequence exhausted")?;
        let revoke_admin = bootstrap.administrator && matches!(&envelope.request, Request::Revoke);
        let mut response = match state.handle(envelope.request) {
            Ok(response) => response,
            Err(error) => Response::Error(format!("{error:#}").chars().take(2048).collect()),
        };
        if revoke_admin {
            if let Err(error) = on_revoke() {
                let _ = state.record_control(
                    "persistent_revoke",
                    "system administrator helper",
                    "revoked session; persistent installation cleanup failed",
                );
                response = Response::Error(
                    format!("administrator authorization cleanup failed: {error:#}")
                        .chars()
                        .take(2048)
                        .collect(),
                );
            }
        }
        write_message(&response, reader.get_mut())?;
        if revoke_admin {
            break;
        }
    }
    state.pending.clear();
    Ok(())
}

#[cfg(unix)]
struct BrokerState {
    caller: Caller,
    pending: HashMap<String, (PrivilegedAction, Instant)>,
    audit: File,
    recent: VecDeque<AuditRecord>,
    audit_bytes: usize,
}

#[cfg(unix)]
fn verify_process_observation(
    expected: &ProcessIdentity,
    resolved: &ProcessIdentity,
) -> Result<()> {
    ensure!(
        expected.pid > 1 && expected.start_time > 0 && expected.uid.parse::<u32>().is_ok(),
        "original process PID, birth identity and UID are required"
    );
    ensure!(
        resolved.pid == expected.pid
            && resolved.start_time == expected.start_time
            && resolved.uid == expected.uid,
        "process target identity changed while completing its executable identity"
    );
    ensure!(
        !resolved.exe.is_empty() && (expected.exe.is_empty() || expected.exe == resolved.exe),
        "process executable identity is unavailable or changed"
    );
    Ok(())
}

#[cfg(unix)]
impl BrokerState {
    fn handle(&mut self, request: Request) -> Result<Response> {
        self.pending
            .retain(|_, (_, deadline)| *deadline >= Instant::now());
        match request {
            Request::Identity => {
                self.record_control(
                    "identity",
                    "authenticated broker session",
                    "read session identity",
                )?;
                Ok(Response::Identity(BrokerSessionIdentity {
                    broker_uid: current_uid(),
                    broker_pid: std::process::id(),
                    caller_uid: self.caller.uid,
                    caller_pid: self.caller.pid,
                    administrator: self.caller.administrator,
                    broker_sid: None,
                    caller_sid: None,
                }))
            }
            Request::Prepare(action) => self.prepare_action(action),
            Request::PrepareProcessStop { expected } => {
                let resolved = (|| -> Result<ProcessIdentity> {
                    ensure!(
                        self.caller.administrator && current_uid() == 0,
                        "process identity completion requires the active administrator session"
                    );
                    let resolved = process_identity(expected.pid)?;
                    verify_process_observation(&expected, &resolved)?;
                    Ok(resolved)
                })();
                let resolved = match resolved {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        self.record(
                            &PrivilegedAction::StopProcess(expected),
                            "denied incomplete process identity or administrator capability",
                        )?;
                        return Err(error);
                    }
                };
                self.prepare_action(PrivilegedAction::StopProcess(resolved))
            }
            Request::Confirm { ticket } => {
                ensure!(ticket.len() == 64, "invalid operation ticket");
                let Some((action, expires)) = self.pending.remove(&ticket) else {
                    self.record_control(
                        "confirm",
                        "operation authorization",
                        "denied expired, revoked, or consumed ticket",
                    )?;
                    anyhow::bail!(
                        "operation authorization expired, was revoked, or was already consumed"
                    );
                };
                ensure!(Instant::now() <= expires, "operation authorization expired");
                if let Err(error) = privilege_policy::validate(&action, self.caller) {
                    self.record(&action, "denied because target identity changed")?;
                    return Err(error);
                }
                self.record(&action, "confirmed by authenticated UI; executing")?;
                let result = privilege_policy::execute(&action, self.caller);
                self.record(
                    &action,
                    match &result {
                        Ok(_) => "completed",
                        Err(_) => "failed",
                    },
                )?;
                Ok(Response::Completed(result?))
            }
            Request::Revoke => {
                self.pending.clear();
                self.record_control(
                    "revoke",
                    "UI session authorizations",
                    "all pending operation tickets revoked",
                )?;
                Ok(Response::Revoked)
            }
            Request::CancelPending => {
                self.pending.clear();
                self.record_control(
                    "cancel_pending",
                    "pending operation tickets",
                    "pending operation confirmations cancelled",
                )?;
                Ok(Response::Cancelled)
            }
            Request::Audit => Ok(Response::Audit(
                self.recent.iter().rev().take(16).cloned().collect(),
            )),
            Request::PowerSample { interval_ms } => {
                ensure!(
                    self.caller.administrator && current_uid() == 0,
                    "power sampling requires the active administrator session"
                );
                self.record_control(
                    "power_sample",
                    "whole-SoC estimated power",
                    "requested bounded read-only sample",
                )?;
                let sample = sample_power(interval_ms)?;
                self.record_control("power_sample", "whole-SoC estimated power", "completed")?;
                Ok(Response::PowerSample(sample))
            }
        }
    }

    fn prepare_action(&mut self, action: PrivilegedAction) -> Result<Response> {
        ensure!(
            self.pending.len() < 16,
            "too many pending operation confirmations"
        );
        let summary = match privilege_policy::validate(&action, self.caller) {
            Ok(summary) => summary,
            Err(error) => {
                self.record(&action, "denied by target identity or capability policy")?;
                return Err(error);
            }
        };
        self.record(&action, "prepared; awaiting UI confirmation")?;
        let ticket = random_token()?;
        let expires_at_ms = now_ms().saturating_add(30_000);
        self.pending.insert(
            ticket.clone(),
            (action, Instant::now() + Duration::from_secs(30)),
        );
        Ok(Response::Prepared(PreparedAction {
            ticket,
            summary,
            expires_at_ms,
        }))
    }

    fn record(&mut self, action: &PrivilegedAction, result: &str) -> Result<()> {
        let (operation, target) = match action {
            PrivilegedAction::StopProcess(target) => (
                "stop_process",
                format!(
                    "PID {} start {} {}",
                    target.pid, target.start_time, target.exe
                ),
            ),
            PrivilegedAction::RestartUserService(target) => (
                "restart_user_service",
                format!(
                    "{}/{} PID {:?} start {:?}",
                    target.domain, target.label, target.pid, target.process_start_time
                ),
            ),
        };
        self.record_control(operation, &target, result)
    }

    fn record_control(&mut self, operation: &str, target: &str, result: &str) -> Result<()> {
        let record = AuditRecord {
            timestamp_ms: now_ms(),
            caller_uid: self.caller.uid,
            caller_pid: self.caller.pid,
            caller_sid: None,
            operation: operation.into(),
            target: target.chars().take(1024).collect(),
            result: result.into(),
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        ensure!(
            self.audit_bytes + bytes.len() <= 1024 * 1024,
            "broker audit budget reached; further writes are denied"
        );
        self.audit.write_all(&bytes)?;
        self.audit.sync_data()?;
        self.audit_bytes += bytes.len();
        self.recent.push_back(record);
        if self.recent.len() > 32 {
            self.recent.pop_front();
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn partial_process_stop_requires_matching_kernel_identity() {
        let resolved = ProcessIdentity {
            pid: 42,
            start_time: 7,
            exe: "/usr/bin/sleep".into(),
            uid: "0".into(),
        };
        let expected = ProcessIdentity {
            exe: String::new(),
            ..resolved.clone()
        };
        verify_process_observation(&expected, &resolved).unwrap();
        for invalid in [
            ProcessIdentity {
                pid: 43,
                ..expected.clone()
            },
            ProcessIdentity {
                start_time: 0,
                ..expected.clone()
            },
            ProcessIdentity {
                start_time: 8,
                ..expected.clone()
            },
            ProcessIdentity {
                uid: "501".into(),
                ..expected.clone()
            },
            ProcessIdentity {
                uid: String::new(),
                ..expected.clone()
            },
            ProcessIdentity {
                exe: "/forged/executable".into(),
                ..expected.clone()
            },
        ] {
            assert!(verify_process_observation(&invalid, &resolved).is_err());
        }
        assert!(verify_process_observation(&expected, &expected).is_err());
    }

    #[test]
    fn partial_process_stop_denies_ordinary_clients_and_audits_it() {
        let mut state = BrokerState {
            caller: Caller {
                uid: 501,
                pid: 40,
                broker_pid: 41,
                administrator: false,
            },
            pending: HashMap::new(),
            audit: tempfile::tempfile().unwrap(),
            recent: VecDeque::new(),
            audit_bytes: 0,
        };
        let error = state
            .handle(Request::PrepareProcessStop {
                expected: ProcessIdentity {
                    pid: 42,
                    start_time: 7,
                    exe: String::new(),
                    uid: "0".into(),
                },
            })
            .err()
            .unwrap();
        assert!(error.to_string().contains("administrator session"));
        assert!(state.pending.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].operation, "stop_process");
        assert!(state.recent[0].result.starts_with("denied"));
    }

    #[test]
    fn cancelling_pending_tickets_keeps_the_administrator_session() {
        let ticket = "a".repeat(64);
        let action = PrivilegedAction::StopProcess(ProcessIdentity {
            pid: 42,
            start_time: 1,
            exe: "/bin/sleep".into(),
            uid: "501".into(),
        });
        let mut state = BrokerState {
            caller: Caller {
                uid: 501,
                pid: 40,
                broker_pid: 41,
                administrator: true,
            },
            pending: HashMap::from([(
                ticket.clone(),
                (action, Instant::now() + Duration::from_secs(30)),
            )]),
            audit: tempfile::tempfile().unwrap(),
            recent: VecDeque::new(),
            audit_bytes: 0,
        };
        assert!(matches!(
            state.handle(Request::CancelPending).unwrap(),
            Response::Cancelled
        ));
        assert!(state.pending.is_empty());
        assert!(state.handle(Request::Confirm { ticket }).is_err());
        let Response::Identity(identity) = state.handle(Request::Identity).unwrap() else {
            panic!("expected session identity");
        };
        assert!(identity.administrator);
        assert_eq!(state.recent.front().unwrap().operation, "cancel_pending");
    }
}

// FVPROV1 27766 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNmU0NDdjOTBiNDc3MTYxYTZhZTVhNTYyNzA5ODAzNTJlNDhhNzNjYWY4ODdjYTEzNTJkYjJlMWY1MTdhZDhkOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzk0OGIwYTIxZDVhYWQwYzM3MTFkZjk3OTRjNTRiNTZjZmU4MGQ4MmQ2ODBiNjk4NDFlN2E3OTZlYWE2NWMwZTE4ODhkMDFjZjg3OTc2ZGI4MDg1MDg1NDE5ZjMwNTM4NDUzNjBmNDg3NzEyZTAzZTJkMGZiYzk5NWQ0ZTU0MGEifV19XQ== FVPROV1-END
