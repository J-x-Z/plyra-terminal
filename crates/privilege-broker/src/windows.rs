// SPDX-License-Identifier: GPL-3.0-only
use anyhow::{ensure, Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufReader, Write},
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::Path,
    ptr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use system_api::{
    ipc::*,
    windows::{
        windows_identity::{self, Handle, PinnedImage, Process},
        windows_ipc::PipeStream,
        WindowsBootstrap, WindowsHello,
    },
    *,
};
use windows_sys::Win32::{
    Foundation::{
        DuplicateHandle, GetLastError, LocalFree, DUPLICATE_SAME_ACCESS, ERROR_IO_PENDING,
        WAIT_OBJECT_0,
    },
    Security::{
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        OWNER_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        GetFileInformationByHandle, GetFileType, ReadFile, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
    },
    System::{
        Threading::{
            CreateEventW, ExitProcess, GetCurrentProcess, OpenProcess, SetEvent, TerminateProcess,
            WaitForMultipleObjects, WaitForSingleObject, INFINITE, PROCESS_DUP_HANDLE,
            PROCESS_SYNCHRONIZE,
        },
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

pub fn run() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        arguments.len() == 7
            && arguments[0] == "--windows-pipe"
            && arguments[2] == "--ui-pid"
            && arguments[4] == "--ui-image-sha256",
        "invalid Windows broker startup"
    );
    let administrator = match arguments[6].as_str() {
        "--administrator" => true,
        "--ordinary" => false,
        _ => anyhow::bail!("invalid Windows broker authorization mode"),
    };
    let caller_pid: u32 = arguments[3].parse().context("invalid UI PID")?;
    ensure!(
        arguments[5].len() == 64 && arguments[5].bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid UI image hash"
    );
    let broker = windows_identity::current_principal()?;
    ensure!(
        broker.elevated == administrator,
        "Windows broker token elevation differs from its startup mode"
    );
    let pipe = PipeStream::connect(&arguments[1])?;
    ensure!(
        pipe.server_pid()? == caller_pid,
        "Windows pipe belongs to another UI process"
    );
    let lifetime = PipeStream::connect(&format!("{}.1", arguments[1]))?;
    ensure!(
        lifetime.server_pid()? == caller_pid,
        "Windows lifetime pipe belongs to another UI process"
    );
    let caller = Process::open(caller_pid, false)?;
    ensure!(
        !caller.principal.elevated && caller.principal.session_id == broker.session_id,
        "broker requires a normal UI in the same desktop session"
    );
    if !administrator {
        ensure!(
            broker.sid == caller.principal.sid,
            "ordinary broker belongs to another SID"
        );
    }
    let ui_image = PinnedImage::open(Path::new(&caller.identity.exe))?;
    ensure!(
        ui_image.sha256 == arguments[5],
        "running UI image differs from its startup identity"
    );
    let executable = std::env::current_exe()?;
    let _installed = if administrator {
        let package = system_api::windows::windows_trust::installed_package()?;
        let running_broker = PinnedImage::open(&executable)?;
        ensure!(
            running_broker.sha256 == package.broker.sha256 && ui_image.sha256 == package.ui.sha256,
            "administrator processes differ from the protected installed package"
        );
        Some(package)
    } else {
        let expected_ui = PinnedImage::open(&executable.with_file_name("glass-terminal.exe"))?;
        ensure!(
            ui_image.sha256 == expected_ui.sha256,
            "UI executable differs from the paired terminal package"
        );
        None
    };
    let _parent = ParentWatch::new(caller_pid, lifetime)?;
    let mut connection = BufReader::new(pipe);
    write_message(
        &WindowsHello {
            pid: std::process::id(),
        },
        connection.get_mut(),
    )?;
    let bootstrap: WindowsBootstrap =
        read_message(&mut connection)?.context("missing Windows broker bootstrap")?;
    ensure!(
        bootstrap.caller_pid == caller_pid
            && bootstrap.caller_sid == caller.principal.sid
            && bootstrap.caller_session == caller.principal.session_id
            && bootstrap.administrator == administrator
            && bootstrap.nonce.len() == 64
            && bootstrap.nonce.bytes().all(|b| b.is_ascii_hexdigit()),
        "Windows bootstrap differs from the kernel-authenticated UI"
    );
    let audit = duplicate_audit(caller_pid, bootstrap.audit_handle, &bootstrap.caller_sid)?;
    let mut state = State {
        caller_sid: bootstrap.caller_sid,
        caller_pid,
        broker_sid: broker.sid,
        administrator,
        audit,
        audit_bytes: 0,
        recent: VecDeque::new(),
        pending: HashMap::new(),
    };
    // An idle authenticated session waits in the kernel. Closing the UI pipe produces EOF.
    connection.get_mut().set_read_timeout(None);
    let mut sequence = 0_u64;
    while let Some(envelope) = read_message::<Envelope>(&mut connection)? {
        ensure!(
            envelope.nonce == bootstrap.nonce && Some(envelope.sequence) == sequence.checked_add(1),
            "Windows broker authentication or sequence check failed"
        );
        sequence = envelope.sequence;
        let revoke = administrator && matches!(&envelope.request, Request::Revoke);
        let response = state
            .handle(envelope.request)
            .unwrap_or_else(|error| Response::Error(error.to_string()));
        write_message(&response, connection.get_mut())?;
        if revoke {
            break;
        }
    }
    Ok(())
}

struct State {
    caller_sid: String,
    caller_pid: u32,
    broker_sid: String,
    administrator: bool,
    audit: File,
    audit_bytes: usize,
    recent: VecDeque<AuditRecord>,
    pending: HashMap<String, (PrivilegedAction, Instant)>,
}

impl State {
    fn handle(&mut self, request: Request) -> Result<Response> {
        self.pending
            .retain(|_, (_, expires)| *expires >= Instant::now());
        match request {
            Request::PrepareProcessStop { expected } => {
                self.record(
                    "prepare",
                    &target(&PrivilegedAction::StopProcess(expected)),
                    "denied unavailable native process identity completion",
                )?;
                anyhow::bail!("native process identity completion is unavailable on this platform")
            }
            Request::PowerSample { .. } => {
                anyhow::bail!("native SoC power sampling is unavailable on this platform")
            }
            Request::Identity => {
                self.record(
                    "identity",
                    "authenticated Windows SID session",
                    "read session identity",
                )?;
                Ok(Response::Identity(BrokerSessionIdentity {
                    broker_uid: u32::MAX,
                    broker_pid: std::process::id(),
                    caller_uid: u32::MAX,
                    caller_pid: self.caller_pid,
                    administrator: self.administrator,
                    broker_sid: Some(self.broker_sid.clone()),
                    caller_sid: Some(self.caller_sid.clone()),
                }))
            }
            Request::Prepare(action) => {
                ensure!(
                    self.pending.len() < 16,
                    "too many pending operation confirmations"
                );
                let summary = match self.validate(&action) {
                    Ok(summary) => summary,
                    Err(error) => {
                        self.record(
                            "prepare",
                            &target(&action),
                            "denied target identity or capability policy",
                        )?;
                        return Err(error);
                    }
                };
                self.record(
                    "prepare",
                    &target(&action),
                    "prepared; awaiting UI confirmation",
                )?;
                let ticket = random_token()?;
                self.pending.insert(
                    ticket.clone(),
                    (action, Instant::now() + Duration::from_secs(30)),
                );
                Ok(Response::Prepared(PreparedAction {
                    ticket,
                    summary,
                    expires_at_ms: now_ms().saturating_add(30_000),
                }))
            }
            Request::Confirm { ticket } => {
                ensure!(ticket.len() == 64, "invalid confirmation ticket");
                let (action, expires) = self
                    .pending
                    .remove(&ticket)
                    .context("authorization was expired, revoked, or consumed")?;
                ensure!(Instant::now() <= expires, "operation authorization expired");
                if let Err(error) = self.validate(&action) {
                    self.record(
                        "confirm",
                        &target(&action),
                        "denied changed target identity",
                    )?;
                    return Err(error);
                }
                self.record(
                    "confirm",
                    &target(&action),
                    "confirmed by authenticated UI; executing",
                )?;
                let result = self.execute(&action);
                self.record(
                    "execute",
                    &target(&action),
                    if result.is_ok() {
                        "completed"
                    } else {
                        "failed"
                    },
                )?;
                Ok(Response::Completed(result?))
            }
            Request::Revoke => {
                self.pending.clear();
                self.record(
                    "revoke",
                    "Windows UI session",
                    "all pending tickets revoked",
                )?;
                Ok(Response::Revoked)
            }
            Request::CancelPending => {
                self.pending.clear();
                self.record(
                    "cancel_pending",
                    "pending operation tickets",
                    "pending operation confirmations cancelled",
                )?;
                Ok(Response::Cancelled)
            }
            Request::Audit => Ok(Response::Audit(
                self.recent.iter().rev().take(32).cloned().collect(),
            )),
        }
    }

    fn validate(&self, action: &PrivilegedAction) -> Result<String> {
        match action {
            PrivilegedAction::StopProcess(expected) => {
                self.process(expected, false)?;
                Ok(format!(
                    "Terminate {} (PID {}, SID {})",
                    expected.exe, expected.pid, expected.uid
                ))
            }
            PrivilegedAction::RestartUserService(expected) => {
                if let Some(pid) = expected.pid {
                    ensure!(
                        pid != self.caller_pid && pid != std::process::id(),
                        "protected UI or broker service target"
                    );
                    Process::open(pid, false)?.ensure_not_critical()?;
                }
                system_api::windows::validate_service(
                    expected,
                    &self.caller_sid,
                    self.administrator,
                )
            }
        }
    }

    fn process(&self, expected: &ProcessIdentity, terminate: bool) -> Result<Process> {
        ensure!(
            expected.pid != self.caller_pid && expected.pid != std::process::id(),
            "protected UI or broker process target"
        );
        let process = Process::open(expected.pid, terminate)?;
        ensure!(
            self.administrator || process.principal.sid == self.caller_sid,
            "process belongs to another account SID"
        );
        ensure!(
            process.identity.uid == expected.uid
                && process.identity.start_time == expected.start_time
                && process.identity.exe == expected.exe,
            "Windows process target identity changed"
        );
        process.ensure_not_critical()?;
        Ok(process)
    }

    fn execute(&self, action: &PrivilegedAction) -> Result<OperationResult> {
        let message = match action {
            PrivilegedAction::StopProcess(expected) => {
                let process = self.process(expected, true)?;
                ensure!(
                    unsafe { TerminateProcess(process.raw(), 1) } != 0,
                    "Windows process termination failed ({})",
                    std::io::Error::last_os_error()
                );
                ensure!(
                    unsafe { WaitForSingleObject(process.raw(), 2000) } == WAIT_OBJECT_0,
                    "Windows process termination is still pending"
                );
                format!("Terminated PID {}", expected.pid)
            }
            PrivilegedAction::RestartUserService(expected) => {
                system_api::windows::restart_service(
                    expected,
                    &self.caller_sid,
                    self.administrator,
                )?;
                format!("Restarted Windows service {}", expected.label)
            }
        };
        Ok(OperationResult {
            success: true,
            message,
        })
    }

    fn record(&mut self, operation: &str, target: &str, result: &str) -> Result<()> {
        let record = AuditRecord {
            timestamp_ms: now_ms(),
            caller_uid: u32::MAX,
            caller_pid: self.caller_pid,
            caller_sid: Some(self.caller_sid.clone()),
            operation: operation.into(),
            target: target.chars().take(1024).collect(),
            result: result.into(),
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        ensure!(
            self.audit_bytes + bytes.len() <= 1024 * 1024,
            "Windows broker audit budget reached"
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

fn target(action: &PrivilegedAction) -> String {
    match action {
        PrivilegedAction::StopProcess(process) => format!(
            "PID {} SID {} start {} {}",
            process.pid, process.uid, process.start_time, process.exe
        ),
        PrivilegedAction::RestartUserService(service) => format!(
            "SCM {} SID {:?} configuration {:?}",
            service.label, service.owner_sid, service.configuration_hash
        ),
    }
}

fn duplicate_audit(pid: u32, source: u64, sid: &str) -> Result<File> {
    let source = usize::try_from(source).context("invalid audit handle width")?;
    ensure!(source > 0 && source != usize::MAX, "invalid audit handle");
    unsafe {
        let process = Handle::from_raw(OpenProcess(PROCESS_DUP_HANDLE, 0, pid))?;
        let mut handle = ptr::null_mut();
        ensure!(
            DuplicateHandle(
                process.raw(),
                source as _,
                GetCurrentProcess(),
                &mut handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS
            ) != 0,
            "cannot duplicate UI-owned audit capability ({})",
            std::io::Error::last_os_error()
        );
        let file = File::from_raw_handle(handle);
        ensure!(
            GetFileType(file.as_raw_handle()) == FILE_TYPE_DISK,
            "audit capability is not a disk file"
        );
        let mut metadata: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        ensure!(
            GetFileInformationByHandle(file.as_raw_handle(), &mut metadata) != 0
                && metadata.dwFileAttributes
                    & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
                    == 0
                && metadata.nNumberOfLinks == 1
                && metadata.nFileSizeHigh == 0
                && metadata.nFileSizeLow == 0,
            "audit capability must be a new regular non-reparse file"
        );
        let mut owner = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let error = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        ensure!(error == 0, "cannot query audit owner ({error})");
        let owner_sid = windows_identity::sid_string(owner);
        LocalFree(descriptor);
        ensure!(owner_sid? == sid, "audit capability belongs to another SID");
        Ok(file)
    }
}

struct ParentWatch {
    stop: Arc<Handle>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ParentWatch {
    fn new(pid: u32, lifetime: PipeStream) -> Result<Self> {
        let parent = unsafe { Handle::from_raw(OpenProcess(PROCESS_SYNCHRONIZE, 0, pid))? };
        let stop =
            Arc::new(unsafe { Handle::from_raw(CreateEventW(ptr::null(), 1, 0, ptr::null()))? });
        let complete = unsafe { Handle::from_raw(CreateEventW(ptr::null(), 1, 0, ptr::null()))? };
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
            operation.hEvent = complete.raw();
            let mut byte = [0_u8; 1];
            let mut count = 0;
            unsafe {
                if ReadFile(
                    lifetime.raw(),
                    byte.as_mut_ptr(),
                    1,
                    &mut count,
                    &mut operation,
                ) != 0
                    || GetLastError() != ERROR_IO_PENDING
                {
                    ExitProcess(0);
                }
                let handles = [parent.raw(), stopped.raw(), complete.raw()];
                let status = WaitForMultipleObjects(3, handles.as_ptr(), 0, INFINITE);
                if status == WAIT_OBJECT_0 + 1 {
                    CancelIoEx(lifetime.raw(), &operation);
                    GetOverlappedResult(lifetime.raw(), &operation, &mut count, 1);
                } else {
                    ExitProcess(0);
                }
            }
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for ParentWatch {
    fn drop(&mut self) {
        unsafe {
            SetEvent(self.stop.raw());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// FVPROV1 19237 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYzExMjZkNDFhOWE3ZTQ3MjJlNzExYjhhOTY2MDVlZDIzNzQ5Nzk0MTJhOGY0ZjhkZmQ2NmM3MWE2YjEwZjE4NiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNWNmZmFjMDZjM2U5N2M1ZjljOWNlMjNkZmM4ODA0YzgxMDYxYmJiZWViMzczMTkxZDZiZDliZDk5YTEyZDI1NDQyZjgyNGExNWRjZGIzMzU1Mzc5Y2UxZjA4ZTg0YTU0MjJiMGY5MzJmM2ExMzczZjg3MzY3ZWUyZTcyZjgzMDcifV19XQ== FVPROV1-END
