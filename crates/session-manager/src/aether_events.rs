// SPDX-License-Identifier: GPL-3.0-only
//! Per-session Aether Shell events, separate from PTY output and system authority.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub const MAX_SHELL_EVENT_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_TASKS: usize = 4096;
const MAX_PENDING_OPERATIONS: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellIntegration {
    #[default]
    None,
    AetherV1,
}

#[derive(Clone, Debug)]
pub struct ShellEvent {
    pub sequence: u64,
    pub event_type: String,
    pub task_id: Option<u64>,
    pub data: Value,
}

impl ShellEvent {
    pub fn summary(&self) -> String {
        let task = self
            .task_id
            .map(|id| format!("task {id} · "))
            .unwrap_or_default();
        let mut detail = match self.event_type.as_str() {
            "hello" => "session connected".into(),
            "task.started" => format!("{task}started · PGID {}", self.data["pgid"]),
            "task.state" => format!(
                "{task}{} · status {}",
                self.data["state"].as_str().unwrap_or("unknown"),
                self.data["status"]
            ),
            "prompt.ready" => format!("prompt · status {}", self.data["status"]),
            "operation.result" => format!(
                "operation {} · status {}",
                self.data["request_id"], self.data["status"]
            ),
            "session.exiting" => format!("session exiting · status {}", self.data["status"]),
            _ => self.event_type.clone(),
        };
        if self.event_type == "operation.result" {
            let result = self
                .data
                .get("error")
                .filter(|error| error.is_string())
                .or_else(|| self.data.get("result"));
            if let Some(result) = result {
                let text = serde_json::to_string(result).unwrap_or_default();
                detail.push_str(" · ");
                detail.extend(text.chars().filter(|ch| !ch.is_control()).take(4096));
            }
        }
        if let Some(program) = self.data["program"].as_str() {
            let name = std::path::Path::new(program)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("program");
            let safe: String = name
                .chars()
                .filter(|ch| !ch.is_control())
                .take(128)
                .collect();
            detail.push_str(" · ");
            detail.push_str(&safe);
        }
        detail
    }
}

struct EventProtocol {
    session_id: String,
    next_sequence: u64,
    hello: bool,
    last_task: u64,
    active: BTreeMap<u64, String>,
    pending_operations: HashSet<u64>,
    pending_host_operations: HashSet<u64>,
    last_host_request: u64,
}

impl EventProtocol {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            next_sequence: 1,
            hello: false,
            last_task: 0,
            active: BTreeMap::new(),
            pending_operations: HashSet::new(),
            pending_host_operations: HashSet::new(),
            last_host_request: 0,
        }
    }

    fn operation(&mut self, request_id: u64, operation: &str, args: &[String]) -> Result<Vec<u8>> {
        ensure!(
            request_id > 0 && !self.pending_operations.contains(&request_id),
            "duplicate or invalid operation request ID"
        );
        ensure!(
            self.pending_operations.len() < MAX_PENDING_OPERATIONS,
            "too many pending Shell operations"
        );
        ensure!(
            !operation.is_empty()
                && operation.len() <= 128
                && operation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid operation name"
        );
        ensure!(
            args.len() <= 128
                && args
                    .iter()
                    .all(|arg| arg.len() <= 8192 && !arg.contains('\0')),
            "invalid operation arguments"
        );
        let mut message = serde_json::to_vec(
            &serde_json::json!({"schema":"aether.shell.v1", "session_id":self.session_id, "type":"operation.request", "request_id":request_id, "operation":operation, "args":args}),
        )?;
        ensure!(
            message.len() < MAX_SHELL_EVENT_BYTES,
            "Shell operation request exceeds limit"
        );
        message.push(b'\n');
        self.pending_operations.insert(request_id);
        Ok(message)
    }

    fn host_result(
        &mut self,
        request_id: u64,
        status: i32,
        result: Value,
        error: Option<String>,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.pending_host_operations.contains(&request_id),
            "unknown Shell host request"
        );
        ensure!((0..=255).contains(&status), "invalid Shell host status");
        let mut message = serde_json::json!({"schema":"aether.shell.v1","session_id":self.session_id,
            "type":"host.operation.result","request_id":request_id,"status":status,"result":result});
        if let Some(error) = error {
            message["error"] = error.into();
        }
        let mut encoded = serde_json::to_vec(&message)?;
        if encoded.len() >= MAX_SHELL_EVENT_BYTES {
            encoded = serde_json::to_vec(
                &serde_json::json!({"schema":"aether.shell.v1","session_id":self.session_id,
                "type":"host.operation.result","request_id":request_id,"status":1,"error":"system response exceeds the session frame limit"}),
            )?;
        }
        encoded.push(b'\n');
        Ok(encoded)
    }

    fn apply(&mut self, value: Value, shell_pid: u32) -> Result<ShellEvent> {
        ensure!(value.is_object(), "Shell event must be an object");
        ensure!(
            value["schema"] == "aether.shell.v1",
            "unsupported Shell event schema"
        );
        ensure!(
            value["session_id"].as_str() == Some(self.session_id.as_str()),
            "Shell session identity mismatch"
        );
        let sequence = value["sequence"]
            .as_u64()
            .context("invalid Shell event sequence")?;
        ensure!(
            sequence == self.next_sequence,
            "Shell event sequence mismatch"
        );
        let kind = value["type"]
            .as_str()
            .context("missing Shell event type")?
            .to_owned();
        let task_id = value["task_id"].as_u64();
        if !self.hello {
            ensure!(kind == "hello", "Shell hello must be the first event");
        }
        match kind.as_str() {
            "hello" => {
                ensure!(
                    !self.hello
                        && value["protocol"] == 1
                        && value["shell_pid"].as_u64() == Some(u64::from(shell_pid)),
                    "invalid Shell hello identity"
                );
                self.hello = true;
            }
            "task.started" => {
                let task = task_id
                    .filter(|id| *id > self.last_task)
                    .context("invalid or reused Shell task identity")?;
                ensure!(
                    self.active.len() < MAX_ACTIVE_TASKS,
                    "too many active Shell tasks"
                );
                positive_pid(&value["pgid"])?;
                ensure!(
                    value["background"].is_boolean(),
                    "invalid Shell task background state"
                );
                bounded_text(&value["display"], 16 * 1024, "invalid Shell task display")?;
                self.validate_task_string(&value, task)?;
                self.last_task = task;
                self.active.insert(task, "running".into());
            }
            "task.state" => {
                let task = task_id.context("missing Shell task identity")?;
                self.validate_task_string(&value, task)?;
                ensure!(
                    self.active.contains_key(&task),
                    "unknown or completed Shell task"
                );
                let state = value["state"]
                    .as_str()
                    .context("invalid Shell task state")?;
                ensure!(
                    ["running", "stopped", "completed"].contains(&state),
                    "unknown Shell task state"
                );
                status(&value["status"])?;
                if state == "completed" {
                    self.active.remove(&task);
                } else {
                    self.active.insert(task, state.into());
                }
            }
            "prompt.ready" => {
                status(&value["status"])?;
                let cwd = bounded_text(&value["cwd"], 4096, "invalid Shell cwd")?;
                ensure!(
                    std::path::Path::new(cwd).is_absolute(),
                    "Shell cwd must be absolute"
                );
            }
            "session.exiting" => {
                status(&value["status"])?;
            }
            "completion.snapshot" => {
                ensure!(
                    value["revision"].as_u64().is_some_and(|id| id > 0),
                    "invalid editor revision"
                );
                ensure!(
                    value["generation"].as_u64().is_some(),
                    "invalid completion generation"
                );
                let start = value["start"]
                    .as_u64()
                    .context("invalid completion start")?;
                let end = value["end"].as_u64().context("invalid completion end")?;
                ensure!(
                    start <= end
                        && end <= 65536
                        && value["cursor"]
                            .as_u64()
                            .is_some_and(|cursor| cursor <= 65536),
                    "invalid completion range"
                );
                ensure!(value["active"].is_boolean(), "invalid completion state");
                let candidates = value["candidates"]
                    .as_array()
                    .context("missing completion candidates")?;
                ensure!(candidates.len() <= 64, "too many completion candidates");
                let mut ids = HashSet::new();
                for candidate in candidates {
                    let id = candidate["id"]
                        .as_u64()
                        .filter(|id| (1..=128).contains(id))
                        .context("invalid candidate ID")?;
                    ensure!(ids.insert(id), "duplicate completion candidate");
                    bounded_text(&candidate["value"], 8192, "invalid candidate insertion")?;
                    bounded_text(&candidate["label"], 8192, "invalid candidate label")?;
                    bounded_text(
                        &candidate["description"],
                        8192,
                        "invalid candidate description",
                    )?;
                }
            }
            "values.record" => {
                ensure!(
                    task_id.is_some_and(|id| id > 0 && id <= self.last_task),
                    "unknown presentation task"
                );
                ensure!(
                    value["row"].as_u64().is_some_and(|row| row > 0)
                        && value["dropped"].as_u64().is_some(),
                    "invalid presentation sequence"
                );
                ensure!(
                    value["truncated"].is_boolean() && value.get("value").is_some(),
                    "invalid presentation record"
                );
                ensure!(
                    serde_json::to_vec(&value["value"])?.len() <= 4096,
                    "presentation record exceeds limit"
                );
            }
            "host.operation.request" => {
                let request = value["request_id"]
                    .as_u64()
                    .filter(|id| *id > self.last_host_request)
                    .context("invalid host operation request ID")?;
                ensure!(
                    self.pending_host_operations.len() < 32,
                    "too many pending host operations"
                );
                let name = value["operation"]
                    .as_str()
                    .context("missing host operation")?;
                ensure!(
                    [
                        "sys.prepare",
                        "sys.confirm",
                        "sys.cancel",
                        "sys.revoke",
                        "sys.audit",
                        "sys.identity",
                        "sys.operations",
                        "sys.power"
                    ]
                    .contains(&name),
                    "unsupported host operation"
                );
                let args = value["args"].as_array().context("invalid host arguments")?;
                ensure!(args.len() <= 128, "too many host arguments");
                for arg in args {
                    bounded_text(arg, 8192, "invalid host argument")?;
                }
                self.pending_host_operations.insert(request);
                self.last_host_request = request;
            }
            "operation.result" => {
                let request = value["request_id"]
                    .as_u64()
                    .context("invalid operation result ID")?;
                ensure!(
                    self.pending_operations.remove(&request),
                    "unsolicited Shell operation result"
                );
                status(&value["status"])?;
                ensure!(
                    value.get("result").is_some() || value["error"].is_string(),
                    "Shell operation result has no value or error"
                );
            }
            _ => bail!("unsupported Shell event type"),
        }
        self.next_sequence = sequence
            .checked_add(1)
            .context("Shell sequence exhausted")?;
        Ok(ShellEvent {
            sequence,
            event_type: kind,
            task_id,
            data: value,
        })
    }

    fn validate_task_string(&self, value: &Value, task: u64) -> Result<()> {
        if let Some(id) = value.get("id") {
            ensure!(
                id.as_str() == Some(format!("{}:{task}", self.session_id).as_str()),
                "Shell task string identity mismatch"
            );
        }
        Ok(())
    }
}

fn status(value: &Value) -> Result<i32> {
    value
        .as_i64()
        .filter(|value| (0..=255).contains(value))
        .map(|value| value as i32)
        .context("invalid Shell status")
}
fn positive_pid(value: &Value) -> Result<u32> {
    value
        .as_u64()
        .filter(|value| *value > 0 && *value <= i32::MAX as u64)
        .map(|value| value as u32)
        .context("invalid Shell process group")
}
fn bounded_text<'a>(value: &'a Value, max: usize, message: &str) -> Result<&'a str> {
    value
        .as_str()
        .filter(|value| value.len() <= max && !value.contains('\0'))
        .context(message.to_owned())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::{SessionEvent, Shared};
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::Shutdown,
        os::{
            fd::AsRawFd,
            unix::{
                fs::{DirBuilderExt, PermissionsExt},
                net::{UnixListener, UnixStream},
            },
        },
        path::{Path, PathBuf},
        sync::{
            mpsc::{self, SyncSender},
            Arc, Mutex,
        },
        thread,
    };

    pub(crate) struct ShellChannel {
        directory: PathBuf,
        socket: PathBuf,
        session_id: String,
        listener: Option<UnixListener>,
        cancel: UnixStream,
        cancel_reader: Option<UnixStream>,
        connected: Arc<Mutex<Option<UnixStream>>>,
        protocol: Arc<Mutex<EventProtocol>>,
        operations: Option<SyncSender<Vec<u8>>>,
    }

    impl ShellChannel {
        pub(crate) fn prepare(base: Option<&Path>) -> Result<Self> {
            let mut random = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
            let session_id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let base = match base {
                Some(path) => {
                    ensure!(path.is_absolute(), "Shell event directory must be absolute");
                    path.to_path_buf()
                }
                None if cfg!(target_os = "macos") => PathBuf::from("/tmp"),
                None => std::env::temp_dir(),
            };
            let directory = base.join(format!("gt-aether-{}", &session_id[..16]));
            fs::DirBuilder::new().mode(0o700).create(&directory)?;
            let socket = directory.join("events");
            let prepared = (|| -> Result<Self> {
                let listener = UnixListener::bind(&socket)?;
                fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
                listener.set_nonblocking(true)?;
                let (cancel, cancel_reader) = UnixStream::pair()?;
                Ok(Self {
                    directory: directory.clone(),
                    socket,
                    session_id: session_id.clone(),
                    listener: Some(listener),
                    cancel,
                    cancel_reader: Some(cancel_reader),
                    connected: Arc::new(Mutex::new(None)),
                    protocol: Arc::new(Mutex::new(EventProtocol::new(session_id))),
                    operations: None,
                })
            })();
            if prepared.is_err() {
                let _ = fs::remove_file(directory.join("events"));
                let _ = fs::remove_dir(&directory);
            }
            prepared
        }
        pub(crate) fn environment(&self) -> [(&str, String); 2] {
            [
                (
                    "AETHER_EVENT_SOCKET",
                    self.socket.to_string_lossy().into_owned(),
                ),
                ("AETHER_SESSION_ID", self.session_id.clone()),
            ]
        }
        pub(crate) fn start(&mut self, shared: Arc<Shared>) {
            let listener = self.listener.take().unwrap();
            let cancel = self.cancel_reader.take().unwrap();
            let connected = self.connected.clone();
            let protocol = self.protocol.clone();
            let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(MAX_PENDING_OPERATIONS);
            self.operations = Some(tx.clone());
            thread::spawn(move || {
                let result = read_channel(listener, cancel, &connected, &protocol, &shared, rx);
                if let Some(stream) = connected.lock().unwrap().take() {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                let _ = tx.try_send(Vec::new());
                if let Err(error) = result {
                    if !shared.stopping.load(std::sync::atomic::Ordering::Acquire) {
                        shared.event(SessionEvent::ShellWarning(format!(
                            "Aether Shell events unavailable: {error:#}"
                        )));
                    }
                }
            });
        }
        pub(crate) fn is_connected(&self) -> bool {
            self.connected.lock().unwrap().is_some() && self.protocol.lock().unwrap().hello
        }
        pub(crate) fn host_operation_result(
            &self,
            request_id: u64,
            status: i32,
            result: Value,
            error: Option<String>,
        ) -> Result<()> {
            ensure!(self.is_connected(), "Shell host channel disconnected");
            let message = self
                .protocol
                .lock()
                .unwrap()
                .host_result(request_id, status, result, error)?;
            self.operations
                .as_ref()
                .context("Shell host channel unavailable")?
                .try_send(message)
                .context("Shell host reply queue is full or closed")?;
            self.protocol
                .lock()
                .unwrap()
                .pending_host_operations
                .remove(&request_id);
            Ok(())
        }
        pub(crate) fn operation(
            &self,
            request_id: u64,
            operation: &str,
            args: &[String],
        ) -> Result<()> {
            ensure!(
                self.connected.lock().unwrap().is_some(),
                "Aether Shell event channel is not connected"
            );
            let message = self
                .protocol
                .lock()
                .unwrap()
                .operation(request_id, operation, args)?;
            let sent = self
                .operations
                .as_ref()
                .context("Shell event channel unavailable")?
                .try_send(message);
            if sent.is_err() {
                self.protocol
                    .lock()
                    .unwrap()
                    .pending_operations
                    .remove(&request_id);
                bail!("Shell operation queue is full or closed");
            }
            Ok(())
        }
    }

    impl Drop for ShellChannel {
        fn drop(&mut self) {
            self.operations.take();
            let _ = self.cancel.write_all(&[1]);
            if let Some(stream) = self.connected.lock().unwrap().take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
            let _ = fs::remove_file(&self.socket);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    fn read_channel(
        listener: UnixListener,
        cancel: UnixStream,
        connected: &Arc<Mutex<Option<UnixStream>>>,
        protocol: &Arc<Mutex<EventProtocol>>,
        shared: &Arc<Shared>,
        requests: mpsc::Receiver<Vec<u8>>,
    ) -> Result<()> {
        let stream = loop {
            let mut descriptors = [
                libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cancel.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(std::io::Error::last_os_error().into());
            }
            if descriptors[1].revents != 0
                || shared.stopping.load(std::sync::atomic::Ordering::Acquire)
            {
                return Ok(());
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // Darwin accepts inherit the listener's nonblocking flag.
                    stream.set_nonblocking(false)?;
                    let peer = peer_identity(&stream)?;
                    if peer.0 != unsafe { libc::getuid() } {
                        continue;
                    }
                    let mut pid = shared.shell_identity.lock().unwrap();
                    while pid.is_none()
                        && !shared.stopping.load(std::sync::atomic::Ordering::Acquire)
                    {
                        pid = shared.shell_ready.wait(pid).unwrap();
                    }
                    if shared.stopping.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(());
                    }
                    if *pid != Some(peer.1) {
                        continue;
                    }
                    break stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let shell_pid =
            (*shared.shell_identity.lock().unwrap()).context("missing Shell identity")?;
        {
            let mut slot = connected.lock().unwrap();
            if shared.stopping.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(());
            }
            *slot = Some(stream.try_clone()?);
        }
        let mut writer = stream.try_clone()?;
        thread::spawn(move || {
            for message in requests {
                if message.is_empty() || writer.write_all(&message).is_err() {
                    break;
                }
            }
        });
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = Read::by_ref(&mut reader)
                .take((MAX_SHELL_EVENT_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if count == 0 {
                break;
            }
            ensure!(
                line.len() <= MAX_SHELL_EVENT_BYTES && line.ends_with(b"\n"),
                "Shell event exceeds limit or is truncated"
            );
            let value = serde_json::from_slice(&line).context("invalid Shell event JSON")?;
            let event = protocol.lock().unwrap().apply(value, shell_pid)?;
            shared.event(SessionEvent::Shell {
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                event,
            });
        }
        connected.lock().unwrap().take();
        Ok(())
    }

    #[cfg(test)]
    mod transport_tests {
        use super::*;
        use crate::State;
        use std::collections::VecDeque;
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize},
            Condvar,
        };
        use std::time::Duration;
        use terminal_core::{Screen, TerminalPalette};

        #[test]
        fn native_peer_roundtrip_and_channel_cleanup() {
            let (wake, awakened) = mpsc::channel();
            let shared = Arc::new(Shared {
                state: Mutex::new(State {
                    screen: Screen::new(10, 4),
                    palette: TerminalPalette::default(),
                    events: VecDeque::new(),
                    exit_seen: false,
                }),
                pending: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                writer_stop: Mutex::new(None),
                reader_done: AtomicBool::new(false),
                queued_bytes: AtomicUsize::new(0),
                output_revision: AtomicUsize::new(0),
                shell_identity: Mutex::new(Some(std::process::id())),
                shell_ready: Condvar::new(),
                wake: Arc::new(move || {
                    let _ = wake.send(());
                }),
            });
            let mut channel = ShellChannel::prepare(None).unwrap();
            let socket = channel.socket.clone();
            let directory = channel.directory.clone();
            let session_id = channel.session_id.clone();
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
                0o600
            );
            channel.start(shared.clone());
            let mut client = UnixStream::connect(&socket).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            writeln!(
                client,
                "{}",
                serde_json::json!({"schema":"aether.shell.v1", "session_id":session_id,
                "sequence":1,"type":"hello","protocol":1,"shell_pid":std::process::id()})
            )
            .unwrap();
            awakened.recv_timeout(Duration::from_secs(2)).unwrap();
            let events = shared.state.lock().unwrap().events.clone();
            assert!(
                matches!(events.front(), Some(SessionEvent::Shell { event, .. }) if event.event_type == "hello"),
                "{events:?}"
            );
            assert!(channel.is_connected());
            channel.operation(7, "task.list", &[]).unwrap();
            let mut input = BufReader::new(client);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["request_id"], 7);
            assert_eq!(request["operation"], "task.list");
            assert_eq!(request["args"], serde_json::json!([]));
            drop(channel);
            assert!(!socket.exists());
            assert!(!directory.exists());
            line.clear();
            assert_eq!(input.read_line(&mut line).unwrap(), 0);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_identity(stream: &UnixStream) -> Result<(u32, u32)> {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        ensure!(
            result == 0 && credentials.pid > 0,
            "cannot read Shell event peer credentials"
        );
        Ok((credentials.uid, credentials.pid as u32))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    fn peer_identity(_: &UnixStream) -> Result<(u32, u32)> {
        bail!("Shell event peer identity is unavailable on this Unix platform")
    }
    #[cfg(target_os = "macos")]
    fn peer_identity(stream: &UnixStream) -> Result<(u32, u32)> {
        let mut uid = 0;
        let mut gid = 0;
        ensure!(
            unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0,
            "cannot read Shell event peer UID"
        );
        let mut pid: libc::pid_t = 0;
        let mut size = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut size,
                )
            } == 0
                && pid > 0,
            "cannot read Shell event peer PID"
        );
        Ok((uid, pid as u32))
    }
}
#[cfg(unix)]
pub(crate) use unix::ShellChannel;

#[cfg(test)]
mod tests {
    use super::*;
    fn event(sequence: u64, kind: &str) -> Value {
        serde_json::json!({"schema":"aether.shell.v1","session_id":"test-session","sequence":sequence,"type":kind})
    }
    fn protocol() -> EventProtocol {
        let mut protocol = EventProtocol::new("test-session".into());
        let mut hello = event(1, "hello");
        hello["protocol"] = 1.into();
        hello["shell_pid"] = 42.into();
        protocol.apply(hello, 42).unwrap();
        protocol
    }
    #[test]
    fn rejects_stale_foreign_or_prehello_events() {
        let mut p = protocol();
        assert!(p.apply(event(1, "prompt.ready"), 42).is_err());
        let mut v = event(2, "prompt.ready");
        v["session_id"] = "other".into();
        assert!(p.apply(v, 42).is_err());
        let mut fresh = EventProtocol::new("test-session".into());
        assert!(fresh.apply(event(1, "session.exiting"), 42).is_err());
    }
    #[test]
    fn task_identity_is_single_use_and_states_require_live_task() {
        let mut p = protocol();
        let mut start = event(2, "task.started");
        start["task_id"] = 1.into();
        start["pgid"] = 100.into();
        start["background"] = false.into();
        start["display"] = "echo sensitive argument".into();
        p.apply(start.clone(), 42).unwrap();
        let mut complete = event(3, "task.state");
        complete["task_id"] = 1.into();
        complete["state"] = "completed".into();
        complete["status"] = 0.into();
        p.apply(complete, 42).unwrap();
        start["sequence"] = 4.into();
        assert!(p.apply(start, 42).is_err());
    }
    #[test]
    fn native_presentations_are_bounded_and_host_replies_are_correlated() {
        let mut p = protocol();
        let mut snapshot = event(2, "completion.snapshot");
        snapshot.as_object_mut().unwrap().extend(
            serde_json::json!({"revision":1,"generation":3,
            "start":0,"end":2,"cursor":2,"active":true,"candidates":[
                {"id":1,"value":"echo","label":"echo","description":"command"}]})
            .as_object()
            .unwrap()
            .clone(),
        );
        p.apply(snapshot, 42).unwrap();
        let mut request = event(3, "host.operation.request");
        request["request_id"] = 9.into();
        request["operation"] = "sys.identity".into();
        request["args"] = serde_json::json!([]);
        p.apply(request.clone(), 42).unwrap();
        let reply: Value = serde_json::from_slice(
            &p.host_result(9, 0, serde_json::json!({"uid":501}), None)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["type"], "host.operation.result");
        assert_eq!(reply["result"]["uid"], 501);
        assert!(p.host_result(10, 0, Value::Null, None).is_err());
        request["sequence"] = 4.into();
        assert!(p.apply(request, 42).is_err());
        let mut invalid = event(4, "completion.snapshot");
        invalid["revision"] = 1.into();
        invalid["generation"] = 1.into();
        invalid["start"] = 3.into();
        invalid["end"] = 1.into();
        assert!(p.apply(invalid, 42).is_err());
    }
    #[test]
    fn completed_tasks_can_deliver_buffered_value_previews() {
        let mut p = protocol();
        let mut start = event(2, "task.started");
        start["task_id"] = 1.into();
        start["pgid"] = 99.into();
        start["background"] = false.into();
        start["display"] = "display".into();
        p.apply(start, 42).unwrap();
        let mut complete = event(3, "task.state");
        complete["task_id"] = 1.into();
        complete["state"] = "completed".into();
        complete["status"] = 0.into();
        p.apply(complete, 42).unwrap();
        let mut preview = event(4, "values.record");
        preview["task_id"] = 1.into();
        preview["row"] = 1.into();
        preview["dropped"] = 0.into();
        preview["truncated"] = false.into();
        preview["value"] = serde_json::json!({"name":"中文"});
        p.apply(preview, 42).unwrap();
    }
    #[test]
    fn operation_results_must_match_pending_request() {
        let mut p = protocol();
        let mut result = event(2, "operation.result");
        result["request_id"] = 3.into();
        result["status"] = 0.into();
        result["result"] = serde_json::json!({});
        assert!(p.apply(result.clone(), 42).is_err());
        p.operation(3, "jobs", &[]).unwrap();
        assert!(p.apply(result, 42).is_ok());
        assert!(p.operation(4, "echo; bad", &[]).is_err());
    }
}

// FVPROV1 35904 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTY0NzY5MDdmY2EzZGFiMTc4NGY1NzliZWFkZjQ2ODQxNWJmYjUzMDU0YzNlNTkxOGMwN2VmZTNlY2U5YTJlOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiODUxY2UwN2Y4NzQwMzA5MTZlNTU2N2Q1ZGQ5MDczZWI5ZTVjMjI1NTQxZmZiMWY1ZjVlMTRkMWQ0MWJhMTM0ZjliYjk1OGNlZTlmZTBmOTE0ZDQyMDBkYzExODI0Mzg4Y2JjN2U1YmNkNjI2MTJlODZhYmU4ZTA2ZjY1M2FmMGEifV19XQ== FVPROV1-END
