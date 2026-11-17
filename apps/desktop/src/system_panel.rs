use crate::{now_ms, Wake};
use egui::{Color32, RichText, Ui};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::mpsc::{self, Receiver, Sender},
};
use system_api::{
    BrokerClient, LogEntry, LogQuery, PreparedAction, PrivilegedAction, SystemClient,
    SystemSnapshot,
};
use winit::event_loop::EventLoopProxy;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    Processes,
    Services,
    Network,
    Devices,
    Logs,
    Timeline,
    Xray,
    Recovery,
    Plugins,
    Audit,
    ShellValues,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::Processes => "Processes",
            Self::Services => "Services",
            Self::Network => "Network",
            Self::Devices => "Devices",
            Self::Logs => "Logs",
            Self::Timeline => "Timeline",
            Self::Xray => "X-Ray",
            Self::Recovery => "Recovery",
            Self::Plugins => "Plugins",
            Self::Audit => "Audit",
            Self::ShellValues => "Shell values",
        }
    }
}

#[derive(Clone)]
pub struct TraceEvent {
    pub at_ms: u64,
    pub source: String,
    pub message: String,
    socket: Option<SocketEvent>,
}

#[derive(Clone, Debug)]
pub struct ShellHostReply {
    pub pane_id: u64,
    pub session_id: String,
    pub request_id: u64,
    pub status: i32,
    pub result: serde_json::Value,
    pub error: Option<String>,
}

#[derive(Clone)]
struct ShellHostRequest {
    pane_id: u64,
    session_id: String,
    request_id: u64,
    name: String,
    args: Vec<String>,
}

enum Update {
    Snapshot(Result<Captured, String>),
    Logs(Result<Vec<LogEntry>, String>),
    Audit(Result<Vec<system_api::AuditRecord>, String>),
    Power(Result<system_api::PowerSample, String>),
    Prepared(Result<(BrokerClient, PreparedAction), String>),
    Operation(Result<String, String>),
    Administrator(Result<BrokerClient, String>),
    Observation(Result<system_api::ProcessObservation, String>),
    Resources(Result<system_api::ProcessResources, String>),
    ShellHost {
        request: ShellHostRequest,
        result: Result<(BrokerClient, serde_json::Value), String>,
    },
}

struct Captured {
    client: SystemClient,
    snapshot: SystemSnapshot,
    file_events: Vec<system_api::FileEvent>,
    connections: Option<Result<String, String>>,
    socket_sample: Option<Result<SocketSample, String>>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
struct SocketIdentity {
    pid: u32,
    start_time: u64,
    uid: String,
    exe: String,
    descriptor: String,
    protocol: String,
    local_endpoint: String,
    remote_endpoint: Option<String>,
}

struct SocketSample {
    at_ms: u64,
    sockets: BTreeMap<SocketIdentity, Option<String>>,
}

impl SocketSample {
    fn capture(at_ms: u64, processes: &[system_api::ProcessResources]) -> Result<Self, String> {
        let mut sockets = BTreeMap::new();
        for process in processes {
            if process.truncated {
                return Err("Socket sample is truncated; change detection was skipped".into());
            }
            for resource in &process.resources {
                if !matches!(resource.kind.as_str(), "IPv4" | "IPv6") {
                    continue;
                }
                let (local, remote) = match resource.name.split_once("->") {
                    Some((local, remote)) => (local, Some(remote.to_owned())),
                    None => (resource.name.as_str(), None),
                };
                sockets.insert(
                    SocketIdentity {
                        pid: process.identity.pid,
                        start_time: process.identity.start_time,
                        uid: process.identity.uid.clone(),
                        exe: process.identity.exe.clone(),
                        descriptor: resource.descriptor.clone(),
                        protocol: resource
                            .protocol
                            .clone()
                            .unwrap_or_else(|| resource.kind.clone()),
                        local_endpoint: local.into(),
                        remote_endpoint: remote,
                    },
                    resource.state.clone(),
                );
            }
        }
        Ok(Self { at_ms, sockets })
    }

    #[cfg(target_os = "macos")]
    fn table(&self) -> String {
        let mut rows = vec!["PID · START · PROTOCOL · LOCAL → REMOTE · STATE".into()];
        for (connection, state) in &self.sockets {
            rows.push(format!(
                "{} · {} · {} · {} → {} · {}",
                connection.pid,
                connection.start_time,
                connection.protocol,
                connection.local_endpoint,
                connection.remote_endpoint.as_deref().unwrap_or("*"),
                state.as_deref().unwrap_or("unknown"),
            ));
        }
        rows.join("\n")
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SocketChange {
    Baseline {
        connections: usize,
    },
    Appeared {
        connection: SocketIdentity,
        state: Option<String>,
    },
    Disappeared {
        connection: SocketIdentity,
        state: Option<String>,
    },
    StateChanged {
        connection: SocketIdentity,
        state: Option<String>,
        previous_state: Option<String>,
    },
}

#[derive(Clone, serde::Serialize)]
struct SocketEvent {
    sampled_at_ms: u64,
    previous_sampled_at_ms: Option<u64>,
    change: SocketChange,
}

impl SocketEvent {
    fn message(&self) -> String {
        let (kind, connection, state, previous_state) = match &self.change {
            SocketChange::Baseline { connections } => {
                return format!("Socket baseline · {connections} IPv4/IPv6 descriptors · short-lived connections between samples are not captured");
            }
            SocketChange::Appeared { connection, state } => ("appeared", connection, state, None),
            SocketChange::Disappeared { connection, state } => {
                ("disappeared", connection, state, None)
            }
            SocketChange::StateChanged {
                connection,
                state,
                previous_state,
            } => (
                "state changed",
                connection,
                state,
                previous_state.as_deref(),
            ),
        };
        format!(
            "Observed between samples · {kind} · PID {} · start {} · {} · {} → {} · {}{} · interval {} ms",
            connection.pid,
            connection.start_time,
            connection.protocol,
            connection.local_endpoint,
            connection.remote_endpoint.as_deref().unwrap_or("*"),
            previous_state.map(|value| format!("{value} → ")).unwrap_or_default(),
            state.as_deref().unwrap_or("unknown state"),
            self.sampled_at_ms.saturating_sub(self.previous_sampled_at_ms.unwrap_or(self.sampled_at_ms)),
        )
    }
}

fn socket_events(previous: Option<&SocketSample>, current: &SocketSample) -> Vec<SocketEvent> {
    let event = |change| SocketEvent {
        sampled_at_ms: current.at_ms,
        previous_sampled_at_ms: previous.map(|sample| sample.at_ms),
        change,
    };
    let Some(previous) = previous else {
        return vec![event(SocketChange::Baseline {
            connections: current.sockets.len(),
        })];
    };
    let mut events = Vec::new();
    for (connection, state) in &current.sockets {
        let change = match previous.sockets.get(connection) {
            None => Some(SocketChange::Appeared {
                connection: connection.clone(),
                state: state.clone(),
            }),
            Some(old) if old != state => Some(SocketChange::StateChanged {
                connection: connection.clone(),
                state: state.clone(),
                previous_state: old.clone(),
            }),
            Some(_) => None,
        };
        if let Some(change) = change {
            events.push(event(change));
        }
    }
    for (connection, state) in &previous.sockets {
        if !current.sockets.contains_key(connection) {
            events.push(event(SocketChange::Disappeared {
                connection: connection.clone(),
                state: state.clone(),
            }));
        }
    }
    events
}

fn resource_socket_events(
    previous: Option<&system_api::ProcessResources>,
    current: &system_api::ProcessResources,
) -> Result<Vec<SocketEvent>, String> {
    let previous = previous.filter(|old| {
        !old.truncated
            && old.identity.pid == current.identity.pid
            && old.identity.start_time == current.identity.start_time
            && old.identity.uid == current.identity.uid
            && old.identity.exe == current.identity.exe
    });
    let old_sample = previous
        .map(|sample| SocketSample::capture(sample.at_ms, std::slice::from_ref(sample)))
        .transpose()?;
    let new_sample = SocketSample::capture(current.at_ms, std::slice::from_ref(current))?;
    Ok(socket_events(old_sample.as_ref(), &new_sample))
}

pub struct SystemPanel {
    pub page: Page,
    pub trace: VecDeque<TraceEvent>,
    pub snapshot: Option<SystemSnapshot>,
    pub busy: bool,
    filter: String,
    logs_process: String,
    logs: Vec<LogEntry>,
    audit: Vec<system_api::AuditRecord>,
    detail: Option<String>,
    status: Option<String>,
    status_success: bool,
    broker: Option<BrokerClient>,
    prepared: Option<PreparedAction>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
    proxy: EventLoopProxy<Wake>,
    tree: bool,
    administrator: bool,
    #[cfg(target_os = "macos")]
    persistent_administrator: bool,
    extensions: crate::extensions::Extensions,
    discard_authorization: bool,
    discard_prepared: bool,
    client: Option<SystemClient>,
    file_events_seen: HashSet<String>,
    connections: Option<Result<String, String>>,
    socket_sample: Option<SocketSample>,
    inspect_pid: Option<u32>,
    observation: Option<system_api::ProcessObservation>,
    observation_pid: String,
    observation_directory: String,
    observation_dropped: u64,
    resources: Option<system_api::ProcessResources>,
    shell_host_queue: VecDeque<ShellHostRequest>,
    shell_host_replies: Vec<ShellHostReply>,
    active_shell_request: Option<(u64, String, u64)>,
    shell_request_cancelled: bool,
}

impl SystemPanel {
    pub fn prepare_catalog_action(
        &mut self,
        name: &str,
        object: &serde_json::Value,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.busy, "another system operation is active");
        self.prepare(system_api::operations::action_from_object(name, object)?);
        Ok(())
    }

    pub fn invoke_shell_operation(
        &mut self,
        pane_id: u64,
        session_id: String,
        request_id: u64,
        name: String,
        args: Vec<String>,
    ) {
        let request = ShellHostRequest {
            pane_id,
            session_id,
            request_id,
            name,
            args,
        };
        if !system_api::operations::is_broker_operation(&request.name)
            || request.args.len() > 2
            || request
                .args
                .iter()
                .any(|argument| argument.len() > 8192 || argument.contains('\0'))
        {
            self.shell_host_error(request, 64, "invalid shared Broker operation");
            return;
        }
        if matches!(request.name.as_str(), "sys.cancel" | "sys.revoke") {
            let mut cancelled = Vec::new();
            self.shell_host_queue.retain(|queued| {
                let selected =
                    queued.pane_id == request.pane_id && queued.session_id == request.session_id;
                if selected {
                    cancelled.push(queued.clone());
                }
                !selected
            });
            for queued in cancelled {
                self.shell_host_error(queued, 130, "pending Shell operation cancelled");
            }
            if self.shell_host_queue.len() >= 32 {
                if let Some(queued) = self.shell_host_queue.pop_back() {
                    self.shell_host_error(
                        queued,
                        75,
                        "queued operation superseded by cancellation",
                    );
                }
            }
            self.shell_host_queue.push_front(request);
        } else if self.shell_host_queue.len() >= 32 {
            self.shell_host_error(request, 75, "shared Broker operation queue is full");
            return;
        } else {
            self.shell_host_queue.push_back(request);
        }
        self.start_shell_operation();
    }

    pub fn cancel_shell_operations(&mut self, pane_id: u64, session_id: &str) {
        self.shell_host_queue
            .retain(|request| request.pane_id != pane_id || request.session_id != session_id);
        if self
            .active_shell_request
            .as_ref()
            .is_some_and(|(pane, session, _)| *pane == pane_id && session == session_id)
        {
            self.shell_request_cancelled = true;
        }
    }

    pub fn take_shell_replies(&mut self) -> Vec<ShellHostReply> {
        std::mem::take(&mut self.shell_host_replies)
    }

    fn shell_host_error(&mut self, request: ShellHostRequest, status: i32, error: &str) {
        self.shell_host_replies.push(ShellHostReply {
            pane_id: request.pane_id,
            session_id: request.session_id,
            request_id: request.request_id,
            status,
            result: serde_json::Value::Null,
            error: Some(error.into()),
        });
        let _ = self.proxy.send_event(Wake::System);
    }

    fn start_shell_operation(&mut self) {
        if self.busy {
            return;
        }
        let Some(request) = self.shell_host_queue.pop_front() else {
            return;
        };
        let existing = self.broker.clone().filter(BrokerClient::is_connected);
        self.discard_authorization = false;
        self.active_shell_request = Some((
            request.pane_id,
            request.session_id.clone(),
            request.request_id,
        ));
        self.shell_request_cancelled = false;
        self.worker(move || {
            let result = (|| -> anyhow::Result<_> {
                let broker = match existing {
                    Some(broker) => broker,
                    None => BrokerClient::spawn()?,
                };
                let result =
                    system_api::operations::broker_records(&broker, &request.name, &request.args)?;
                Ok((broker, result))
            })()
            .map_err(|error| error.to_string());
            Update::ShellHost { request, result }
        });
    }

    fn receive_shell_operation(
        &mut self,
        request: ShellHostRequest,
        result: Result<(BrokerClient, serde_json::Value), String>,
    ) {
        self.active_shell_request = None;
        let cancelled = std::mem::take(&mut self.shell_request_cancelled);
        match result {
            Ok((broker, value)) => {
                if cancelled && request.name == "sys.prepare" {
                    self.administrator = broker.is_admin();
                    self.broker = Some(broker);
                    self.cancel_pending();
                    self.shell_host_error(
                        request,
                        130,
                        "Shell session closed before preparation completed",
                    );
                    return;
                }
                if self.discard_authorization {
                    self.revoke_discarded_authorization(broker);
                    self.shell_host_error(
                        request,
                        130,
                        "Broker authorization was revoked while the request was active",
                    );
                    return;
                }
                if request.name == "sys.revoke" {
                    self.broker = None;
                    self.prepared = None;
                    self.administrator = false;
                } else {
                    self.administrator = broker.is_admin();
                    self.broker = Some(broker);
                    if request.name == "sys.cancel" {
                        self.prepared = None;
                    }
                }
                self.record(
                    "Shell Broker operation",
                    format!(
                        "{} · pane {} · {}",
                        request.name, request.pane_id, request.session_id
                    ),
                );
                self.shell_host_replies.push(ShellHostReply {
                    pane_id: request.pane_id,
                    session_id: request.session_id,
                    request_id: request.request_id,
                    status: 0,
                    result: value,
                    error: None,
                });
            }
            Err(error) => {
                if self
                    .broker
                    .as_ref()
                    .is_some_and(|broker| !broker.is_connected())
                {
                    self.broker = None;
                    self.administrator = false;
                    self.prepared = None;
                }
                self.shell_host_error(request, 1, &error);
            }
        }
    }

    pub fn dismiss_dialogs(&mut self) {
        self.discard_prepared = true;
        if self.prepared.is_some() {
            self.cancel_pending();
        }
        self.detail = None;
        self.extensions.dismiss_dialogs();
    }
    pub fn new(proxy: EventLoopProxy<Wake>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            page: Page::Processes,
            trace: VecDeque::new(),
            snapshot: None,
            busy: false,
            filter: String::new(),
            logs_process: LogQuery::default()
                .process
                .unwrap_or_else(|| "glass-terminal".into()),
            logs: Vec::new(),
            audit: Vec::new(),
            detail: None,
            status: None,
            status_success: false,
            broker: None,
            prepared: None,
            tx,
            rx,
            extensions: crate::extensions::Extensions::new(proxy.clone()),
            proxy,
            tree: false,
            administrator: false,
            #[cfg(target_os = "macos")]
            persistent_administrator: false,
            discard_authorization: false,
            discard_prepared: false,
            client: None,
            file_events_seen: HashSet::new(),
            connections: None,
            socket_sample: None,
            inspect_pid: None,
            observation: None,
            observation_pid: std::process::id().to_string(),
            observation_directory: String::new(),
            observation_dropped: 0,
            resources: None,
            shell_host_queue: VecDeque::new(),
            shell_host_replies: Vec::new(),
            active_shell_request: None,
            shell_request_cancelled: false,
        }
    }
    #[cfg(target_os = "macos")]
    pub fn configure_administrator(&mut self, persistent: bool, reconnect: bool) {
        let changed = self.persistent_administrator != persistent;
        self.persistent_administrator = persistent;
        if persistent
            && (reconnect || changed)
            && !self.administrator
            && self.broker.is_none()
            && !self.busy
        {
            self.discard_authorization = false;
            self.worker(|| {
                Update::Administrator(
                    BrokerClient::connect_persistent_admin().map_err(|error| error.to_string()),
                )
            });
        }
    }
    pub fn record(&mut self, source: &str, message: impl Into<String>) {
        self.record_at(now_ms(), source, message);
    }
    pub fn inspect_pid(&mut self, pid: u32) {
        self.page = Page::Processes;
        self.filter = pid.to_string();
        self.inspect_pid = Some(pid);
        self.refresh();
    }
    pub fn record_at(&mut self, at_ms: u64, source: &str, message: impl Into<String>) {
        self.trace.push_back(TraceEvent {
            at_ms,
            source: source.into(),
            message: message.into(),
            socket: None,
        });
        while self.trace.len() > 2000 {
            self.trace.pop_front();
        }
    }
    fn record_sockets(&mut self, events: Vec<SocketEvent>) {
        for event in events {
            self.trace.push_back(TraceEvent {
                at_ms: event.sampled_at_ms,
                source: "macOS socket observation between samples".into(),
                message: event.message(),
                socket: Some(event),
            });
        }
        while self.trace.len() > 2000 {
            self.trace.pop_front();
        }
    }
    fn worker(&mut self, job: impl FnOnce() -> Update + Send + 'static) {
        self.busy = true;
        let tx = self.tx.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            let _ = proxy.send_event(Wake::System);
        });
    }
    pub fn refresh(&mut self) {
        if !self.busy {
            self.status = None;
            self.status_success = false;
        }
        if self.page == Page::ShellValues {
            return;
        }
        if matches!(self.page, Page::Recovery | Page::Plugins) {
            self.extensions.refresh();
            return;
        }
        if self.busy {
            return;
        }
        if self.page == Page::Audit {
            let client = self.client.clone();
            self.worker(move || {
                Update::Audit(
                    (|| -> anyhow::Result<_> {
                        let client = match client {
                            Some(client) => client,
                            None => SystemClient::new()?,
                        };
                        client.audit_history(500)
                    })()
                    .map_err(|error| error.to_string()),
                )
            });
        } else if self.page == Page::Logs {
            let query = LogQuery {
                process: (!self.logs_process.trim().is_empty())
                    .then(|| self.logs_process.trim().to_owned()),
                contains: (!self.filter.is_empty()).then(|| self.filter.clone()),
                seconds: 120,
                limit: 200,
            };
            self.worker(move || {
                Update::Logs(
                    SystemClient::new()
                        .and_then(|client| system_api::operations::logs(&client, query))
                        .map_err(|error| error.to_string()),
                )
            });
        } else if self.page != Page::Timeline {
            let client = self.client.clone();
            let connections = matches!(self.page, Page::Network | Page::Xray);
            self.worker(move || {
                Update::Snapshot(
                    (|| -> anyhow::Result<Captured> {
                        let client = match client {
                            Some(client) => client,
                            None => SystemClient::new()?,
                        };
                        let snapshot = system_api::operations::snapshot(&client)?;
                        let file_events = client.file_events()?;
                        #[cfg(target_os = "macos")]
                        let (connections, socket_sample) = if connections {
                            let sample = client
                                .connection_resources(&snapshot)
                                .map_err(|error| error.to_string())
                                .and_then(|resources| SocketSample::capture(now_ms(), &resources));
                            let text = sample
                                .as_ref()
                                .map(SocketSample::table)
                                .map_err(Clone::clone);
                            (Some(text), Some(sample))
                        } else {
                            (None, None)
                        };
                        #[cfg(not(target_os = "macos"))]
                        let (connections, socket_sample) = (
                            connections
                                .then(|| client.connections().map_err(|error| error.to_string())),
                            None,
                        );
                        Ok(Captured {
                            client,
                            snapshot,
                            file_events,
                            connections,
                            socket_sample,
                        })
                    })()
                    .map_err(|error| error.to_string()),
                )
            });
        }
    }
    fn observe(&mut self) {
        let Some(identity) = self.observation_identity() else {
            self.status = Some("Choose a PID from the current snapshot".into());
            return;
        };
        let directory = std::path::PathBuf::from(&self.observation_directory);
        let proxy = self.proxy.clone();
        self.observation = None;
        self.observation_dropped = 0;
        self.worker(move || {
            Update::Observation(
                system_api::ProcessObservation::start(identity, &directory, move || {
                    let _ = proxy.send_event(Wake::System);
                })
                .map_err(|error| error.to_string()),
            )
        });
    }
    fn observation_identity(&self) -> Option<system_api::ProcessIdentity> {
        let pid = self.observation_pid.trim().parse::<u32>().ok()?;
        self.snapshot
            .as_ref()?
            .processes
            .iter()
            .find(|process| process.identity.pid == pid)
            .map(|process| process.identity.clone())
    }
    fn inspect_resources(&mut self) {
        let Some(identity) = self.observation_identity() else {
            self.status = Some("Choose a PID from the current snapshot".into());
            return;
        };
        self.worker(move || {
            Update::Resources(
                system_api::process_resources(&identity).map_err(|error| error.to_string()),
            )
        });
    }
    pub fn receive(&mut self) {
        for message in self.extensions.receive() {
            self.record("managed workspace API", message);
        }
        while let Ok(update) = self.rx.try_recv() {
            self.busy = false;
            match update {
                Update::ShellHost { request, result } => {
                    self.receive_shell_operation(request, result)
                }
                Update::Observation(Ok(observation)) => {
                    self.observation = Some(observation);
                }
                Update::Resources(Ok(resources)) => {
                    match resource_socket_events(self.resources.as_ref(), &resources) {
                        Ok(events) => self.record_sockets(events),
                        Err(error) => self.record("socket observation scope", error),
                    }
                    self.record_at(
                        resources.at_ms,
                        "process descriptor observation",
                        format!(
                            "Collected {} descriptors · PID {} · start {}",
                            resources.resources.len(),
                            resources.identity.pid,
                            resources.identity.start_time
                        ),
                    );
                    self.resources = Some(resources);
                }
                Update::Observation(Err(error)) | Update::Resources(Err(error)) => {
                    self.status = Some(error);
                    self.status_success = false;
                }
                Update::Snapshot(Ok(captured)) => {
                    let snapshot = captured.snapshot;
                    if self.observation_directory.is_empty() {
                        self.observation_directory = captured
                            .client
                            .managed_root()
                            .to_string_lossy()
                            .into_owned();
                    }
                    self.client = Some(captured.client);
                    match captured.socket_sample {
                        Some(Ok(sample)) => {
                            self.record_sockets(socket_events(
                                self.socket_sample.as_ref(),
                                &sample,
                            ));
                            self.socket_sample = Some(sample);
                        }
                        Some(Err(error)) => self.record("socket observation error", error),
                        None => {}
                    }
                    if captured.connections.is_some() {
                        self.connections = captured.connections;
                    }
                    let mut observed = HashSet::new();
                    for event in captured.file_events {
                        let key = serde_json::to_string(&event).unwrap_or_default();
                        if !self.file_events_seen.contains(&key) {
                            self.record_at(
                                event.sampled_at_ms,
                                &event.source,
                                format!("{} · {}", event.kind, event.paths.join(", ")),
                            );
                        }
                        observed.insert(key);
                    }
                    self.file_events_seen = observed;
                    if let Some(previous) = &self.snapshot {
                        let mut changes = Vec::new();
                        for network in &snapshot.network {
                            if let Some(old) =
                                previous.network.iter().find(|old| old.name == network.name)
                            {
                                let received =
                                    network.received_bytes.saturating_sub(old.received_bytes);
                                let sent = network
                                    .transmitted_bytes
                                    .saturating_sub(old.transmitted_bytes);
                                if received > 0 || sent > 0 {
                                    changes.push(format!(
                                        "{} · RX +{} bytes · TX +{} bytes · observed over {} ms",
                                        network.name,
                                        received,
                                        sent,
                                        snapshot
                                            .sampled_at_ms
                                            .saturating_sub(previous.sampled_at_ms)
                                    ));
                                }
                            }
                        }
                        changes.extend(process_snapshot_changes(
                            &previous.processes,
                            &snapshot.processes,
                        ));
                        for change in changes {
                            self.record("OS snapshot delta", change);
                        }
                    }
                    self.record(
                        &snapshot.source,
                        format!(
                            "System sampled · {} processes · {} services",
                            snapshot.processes.len(),
                            snapshot.services.len()
                        ),
                    );
                    self.snapshot = Some(snapshot);
                    if !self.status_success {
                        self.status = None;
                    }
                    if let Some(pid) = self.inspect_pid.take() {
                        if let Some(process) = self.snapshot.as_ref().and_then(|snapshot| {
                            snapshot
                                .processes
                                .iter()
                                .find(|process| process.identity.pid == pid)
                        }) {
                            self.detail = serde_json::to_string_pretty(process).ok();
                            self.record("system process API", format!("Inspected PID {pid}"));
                        } else {
                            self.status = Some(format!(
                                "PID {pid} is unavailable in the current system snapshot"
                            ));
                            self.status_success = false;
                        }
                    }
                }
                Update::Power(Ok(sample)) => {
                    self.record_at(sample.sampled_at_ms, &sample.source, sample.summary.clone());
                    self.detail = serde_json::to_string_pretty(&sample).ok();
                    self.status = Some(sample.summary);
                    self.status_success = true;
                }
                Update::Logs(Ok(logs)) => {
                    self.record("system log API", format!("Read {} log entries", logs.len()));
                    self.logs = logs;
                    self.status = None;
                }
                Update::Audit(Ok(audit)) => {
                    self.record(
                        "persistent broker audit",
                        format!("Read {} audit records", audit.len()),
                    );
                    self.audit = audit;
                    self.status = None;
                }
                Update::Prepared(Ok((broker, prepared))) => {
                    if let Some(broker) =
                        retain_authorization(self.discard_authorization, broker, |broker| {
                            self.revoke_discarded_authorization(broker)
                        })
                    {
                        if self.discard_prepared {
                            self.broker = Some(broker);
                            self.cancel_pending();
                            continue;
                        }
                        self.broker = Some(broker);
                        self.prepared = Some(prepared);
                    }
                }
                Update::Operation(Ok(message)) => {
                    self.record("authenticated broker", message.clone());
                    self.refresh();
                    self.status = Some(format!("Completed · {message}"));
                    self.status_success = true;
                }
                Update::Administrator(Ok(broker)) => {
                    if let Some(broker) =
                        retain_authorization(self.discard_authorization, broker, |broker| {
                            self.revoke_discarded_authorization(broker)
                        })
                    {
                        let (_, pid) = broker.peer_identity();
                        let principal = broker.peer_principal_label();
                        self.broker = Some(broker);
                        self.administrator = true;
                        self.status = Some(format!(
                            "Administrator authorization active · {principal} · PID {pid}"
                        ));
                        self.status_success = true;
                        self.record("authenticated broker", "Administrator broker connected");
                    }
                }
                Update::Snapshot(Err(error))
                | Update::Logs(Err(error))
                | Update::Audit(Err(error))
                | Update::Power(Err(error))
                | Update::Prepared(Err(error))
                | Update::Operation(Err(error))
                | Update::Administrator(Err(error)) => {
                    if self
                        .broker
                        .as_ref()
                        .is_some_and(|broker| !broker.is_connected())
                    {
                        self.broker = None;
                        self.administrator = false;
                        self.prepared = None;
                    }
                    self.record("system API error", error.clone());
                    self.status = Some(error);
                    self.status_success = false;
                }
            }
        }
        self.start_shell_operation();
        if let Some(observation) = &self.observation {
            let (events, dropped) = observation.take_events();
            self.observation_dropped = dropped;
            for event in events {
                self.record_at(event.at_ms, &event.source, event.message);
            }
        }
    }
    fn revoke_discarded_authorization(&mut self, broker: BrokerClient) {
        self.prepared = None;
        self.administrator = false;
        self.broker = None;
        self.worker(move || {
            Update::Operation(
                broker
                    .revoke()
                    .map(|_| "Authorization cancelled".into())
                    .map_err(|error| error.to_string()),
            )
        });
    }
    fn prepare(&mut self, action: PrivilegedAction) {
        if self.busy {
            return;
        }
        self.discard_authorization = false;
        self.discard_prepared = false;
        let existing = self.broker.clone();
        self.worker(move || {
            Update::Prepared(
                (|| {
                    let broker = match existing {
                        Some(broker) => broker,
                        None => BrokerClient::spawn()?,
                    };
                    let prepared = broker.prepare(action)?;
                    Ok((broker, prepared))
                })()
                .map_err(|error: anyhow::Error| error.to_string()),
            )
        });
    }
    fn cancel_pending(&mut self) {
        self.discard_prepared = true;
        self.prepared = None;
        if let Some(broker) = self.broker.clone() {
            self.worker(move || {
                Update::Operation(
                    broker
                        .cancel_pending()
                        .map(|_| "Pending operation cancelled".into())
                        .map_err(|error| error.to_string()),
                )
            });
        }
    }
    pub fn revoke(&mut self) {
        self.discard_authorization = true;
        self.discard_prepared = true;
        self.prepared = None;
        self.administrator = false;
        if let Some(broker) = self.broker.take() {
            self.worker(move || {
                Update::Operation(
                    broker
                        .revoke()
                        .map(|_| "Pending authorizations revoked".into())
                        .map_err(|error| error.to_string()),
                )
            });
        }
    }
    pub fn show(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
        let secondary_color = ui.visuals().text_color().gamma_multiply(0.8);
        let previous_page = self.page;
        ui.horizontal(|ui| {
            let page_width = (ui.available_width() - 116.0).clamp(90.0, 150.0);
            egui::ComboBox::from_id_salt("system_page")
                .selected_text(self.page.label())
                .width(page_width)
                .show_ui(ui, |ui| {
                    for page in [
                        Page::Processes,
                        Page::Services,
                        Page::Network,
                        Page::Devices,
                        Page::Logs,
                        Page::Audit,
                        Page::Timeline,
                        Page::Xray,
                        Page::Recovery,
                        Page::Plugins,
                        Page::ShellValues,
                    ] {
                        if page == Page::Timeline {
                            ui.separator();
                        }
                        ui.selectable_value(&mut self.page, page, page.label());
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.administrator {
                    ui.menu_button("Administrator", |ui| {
                        if cfg!(target_os = "macos")
                            && ui
                                .add_enabled(!self.busy, egui::Button::new("Power sample"))
                                .clicked()
                        {
                            ui.close_menu();
                            if let Some(broker) = self.broker.clone() {
                                self.worker(move || {
                                    Update::Power(
                                        broker
                                            .power_sample(1000)
                                            .map_err(|error| error.to_string()),
                                    )
                                });
                            }
                        }
                        if ui.button("Revoke").clicked() {
                            ui.close_menu();
                            self.revoke();
                        }
                    });
                } else {
                    #[cfg(target_os = "macos")]
                    let authorize_hint = if self.persistent_administrator {
                        "Authorize and keep administrator access across launches"
                    } else {
                        "Authorize administrator access for this session"
                    };
                    #[cfg(not(target_os = "macos"))]
                    let authorize_hint = "Authorize administrator access for this session";
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Authorize…"))
                        .on_hover_text(authorize_hint)
                        .clicked()
                    {
                        self.discard_authorization = false;
                        self.discard_prepared = false;
                        #[cfg(target_os = "macos")]
                        let persistent = self.persistent_administrator;
                        self.worker(move || {
                            Update::Administrator(
                                (|| {
                                    #[cfg(target_os = "macos")]
                                    if persistent {
                                        let status = system_api::macos_admin::persistent_status()?;
                                        return if status.compatible {
                                            BrokerClient::connect_persistent_admin()
                                        } else {
                                            BrokerClient::install_persistent_admin()
                                        };
                                    }
                                    BrokerClient::spawn_admin()
                                })()
                                .map_err(|error: anyhow::Error| error.to_string()),
                            )
                        });
                    }
                }
            });
        });
        if previous_page != self.page {
            self.refresh();
        }
        if matches!(self.page, Page::Recovery | Page::Plugins) {
            self.extensions.plugin_page = self.page == Page::Plugins;
            self.extensions.show(ui);
            return;
        }
        if self.page != Page::ShellValues {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let actions_width = if self.page == Page::Processes {
                    120.0
                } else {
                    58.0
                };
                let filter_width = (ui.available_width() - actions_width - 16.0).max(60.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .id_salt("system_filter")
                        .hint_text("Filter")
                        .desired_width(filter_width)
                        .char_limit(1024),
                );
                if self.page == Page::Processes {
                    ui.checkbox(&mut self.tree, "Tree")
                        .on_hover_text("Arrange processes by parent PID");
                }
                if self.page != Page::Timeline
                    && ui
                        .add_enabled(
                            !self.busy,
                            egui::Button::new("Refresh").small().frame(false),
                        )
                        .clicked()
                {
                    self.refresh();
                }
            });
        }
        if self.busy {
            ui.label(RichText::new("Working…").color(secondary_color));
        } else if let Some(status) = self
            .status
            .as_ref()
            .filter(|_| self.page != Page::ShellValues || !self.status_success)
        {
            let color = if self.status_success {
                secondary_color
            } else {
                ui.visuals().warn_fg_color
            };
            ui.add(egui::Label::new(RichText::new(status).small().color(color)).truncate())
                .on_hover_text(status);
        }
        if self.page == Page::ShellValues {
            return;
        }
        if let Some(snapshot) = self.snapshot.as_ref().filter(|_| {
            matches!(
                self.page,
                Page::Processes | Page::Services | Page::Network | Page::Devices | Page::Xray
            )
        }) {
            egui::CollapsingHeader::new(
                RichText::new(format!("Snapshot · {} UTC", clock(snapshot.sampled_at_ms)))
                    .small()
                    .color(secondary_color),
            )
            .id_salt("system_snapshot_source")
            .show(ui, |ui| {
                ui.small(&snapshot.source);
                for limitation in &snapshot.limitations {
                    ui.small(limitation);
                }
            });
        }
        ui.add_space(4.0);
        let mut operation = None;
        let mut detail = None;
        let filter = self.filter.to_lowercase();
        match self.page {
            Page::Audit => {
                ui.small("Source · authenticated broker persistent audit history");
                let records: Vec<_> = self
                    .audit
                    .iter()
                    .rev()
                    .filter(|entry| {
                        format!(
                            "{} {} {} {} {} {}",
                            entry.caller_uid,
                            entry.caller_sid.as_deref().unwrap_or_default(),
                            entry.caller_pid,
                            entry.operation,
                            entry.target,
                            entry.result
                        )
                        .to_lowercase()
                        .contains(&filter)
                    })
                    .collect();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for entry in records {
                        let principal = entry.caller_sid.as_ref().map_or_else(
                            || format!("UID {}", entry.caller_uid),
                            |sid| format!("SID {sid}"),
                        );
                        ui.small(format!(
                            "{} UTC · caller {} / PID {}",
                            clock(entry.timestamp_ms),
                            principal,
                            entry.caller_pid
                        ));
                        ui.label(RichText::new(&entry.operation).strong());
                        ui.add(egui::Label::new(RichText::new(&entry.target).monospace()).wrap());
                        ui.label(&entry.result);
                        ui.separator();
                    }
                });
            }
            Page::Processes => {
                if let Some(snapshot) = &self.snapshot {
                    let ordered = process_order(snapshot, self.tree);
                    let rows: Vec<_> = ordered
                        .into_iter()
                        .filter(|(index, _)| {
                            let process = &snapshot.processes[*index];
                            if let Ok(pid) = filter.parse::<u32>() {
                                return process.identity.pid == pid;
                            }
                            format!(
                                "{} {} {}",
                                process.identity.pid, process.name, process.identity.exe
                            )
                            .to_lowercase()
                            .contains(&filter)
                        })
                        .collect();
                    let (pid_width, cpu_width, memory_width, action_width, column_gap) =
                        if ui.available_width() < 300.0 {
                            (42.0, 42.0, 42.0, 30.0, 4.0)
                        } else {
                            (48.0, 48.0, 56.0, 40.0, 8.0)
                        };
                    let numeric_width = pid_width + cpu_width + memory_width;
                    let column_spacing = column_gap * 4.0;
                    let scrollbar_width =
                        ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_inner_margin;
                    let name_width = (ui.available_width()
                        - numeric_width
                        - action_width
                        - column_spacing
                        - scrollbar_width)
                        .max(35.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = column_gap;
                        for (width, label, align) in [
                            (pid_width, "PID", egui::Align::RIGHT),
                            (name_width, "Name", egui::Align::LEFT),
                            (cpu_width, "CPU %", egui::Align::RIGHT),
                            (memory_width, "MiB", egui::Align::RIGHT),
                        ] {
                            ui.add_sized(
                                [width, 18.0],
                                egui::Label::new(
                                    RichText::new(label).small().color(secondary_color),
                                )
                                .halign(align),
                            );
                        }
                    });
                    egui::ScrollArea::vertical().show_rows(ui, 24.0, rows.len(), |ui, range| {
                        for index in range {
                            let (source, depth) = rows[index];
                            let process = &snapshot.processes[source];
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = column_gap;
                                ui.add_sized(
                                    [pid_width, 24.0],
                                    egui::Label::new(
                                        RichText::new(process.identity.pid.to_string()).monospace(),
                                    )
                                    .halign(egui::Align::RIGHT),
                                );
                                let name = format!("{}{}", "  ".repeat(depth.min(8)), process.name);
                                if ui
                                    .add_sized(
                                        [name_width, 24.0],
                                        egui::Label::new(name)
                                            .halign(egui::Align::LEFT)
                                            .truncate()
                                            .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(&process.identity.exe)
                                    .clicked()
                                {
                                    detail = serde_json::to_string_pretty(process).ok();
                                }
                                ui.add_sized(
                                    [cpu_width, 24.0],
                                    egui::Label::new(
                                        RichText::new(format!("{:.1}", process.cpu_percent))
                                            .small()
                                            .monospace(),
                                    )
                                    .halign(egui::Align::RIGHT),
                                );
                                ui.add_sized(
                                    [memory_width, 24.0],
                                    egui::Label::new(
                                        RichText::new((process.rss_bytes / 1_048_576).to_string())
                                            .small()
                                            .monospace(),
                                    )
                                    .halign(egui::Align::RIGHT),
                                );
                                if ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Button::new("Stop").small().frame(false),
                                    )
                                    .on_hover_text("Prepare a stop operation for this process")
                                    .clicked()
                                {
                                    operation = Some(PrivilegedAction::StopProcess(
                                        process.identity.clone(),
                                    ));
                                }
                            });
                        }
                    });
                }
            }
            Page::Services => {
                if let Some(snapshot) = &self.snapshot {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for service in snapshot.services.iter().filter(|service| {
                            service.identity.label.to_lowercase().contains(&filter)
                        }) {
                            ui.horizontal(|ui| {
                                let label_width = (ui.available_width() - 76.0).max(40.0);
                                if ui
                                    .add_sized(
                                        [label_width, 22.0],
                                        egui::Label::new(&service.identity.label)
                                            .halign(egui::Align::LEFT)
                                            .truncate()
                                            .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(&service.identity.label)
                                    .clicked()
                                {
                                    detail = serde_json::to_string_pretty(service).ok();
                                }
                                if ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Button::new("Restart").small().frame(false),
                                    )
                                    .on_hover_text("Prepare a restart operation for this service")
                                    .clicked()
                                {
                                    operation = Some(PrivilegedAction::RestartUserService(
                                        service.identity.clone(),
                                    ));
                                }
                            });
                            ui.label(
                                RichText::new(format!(
                                    "{} · PID {} · last exit {}",
                                    service.identity.domain,
                                    service.pid.map_or("—".into(), |pid| pid.to_string()),
                                    service
                                        .last_exit
                                        .map_or("—".into(), |code| code.to_string())
                                ))
                                .small()
                                .color(secondary_color),
                            );
                            ui.add_space(4.0);
                        }
                    });
                }
            }
            Page::Network => {
                if let Some(snapshot) = &self.snapshot {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for network in snapshot
                            .network
                            .iter()
                            .filter(|network| network.name.to_lowercase().contains(&filter))
                        {
                            ui.label(RichText::new(&network.name).strong());
                            ui.monospace(format!(
                                "RX {} bytes\nTX {} bytes",
                                network.received_bytes, network.transmitted_bytes
                            ));
                            ui.separator();
                        }
                        if let Some(connections) = &self.connections {
                            ui.label(RichText::new("Native socket connections").strong());
                            match connections {
                                Ok(text) => {
                                    for line in text
                                        .lines()
                                        .filter(|line| line.to_lowercase().contains(&filter))
                                    {
                                        ui.monospace(line);
                                    }
                                }
                                Err(error) => {
                                    ui.colored_label(Color32::from_rgb(245, 187, 120), error);
                                }
                            }
                        }
                    });
                }
            }
            Page::Devices => {
                if let Some(snapshot) = &self.snapshot {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for device in snapshot.devices.iter().filter(|device| {
                            format!("{} {}", device.name, device.kind)
                                .to_lowercase()
                                .contains(&filter)
                        }) {
                            ui.label(RichText::new(&device.name).strong());
                            ui.small(&device.kind);
                            ui.label(&device.details);
                            ui.separator();
                        }
                    });
                }
            }
            Page::Logs => {
                ui.add(
                    egui::TextEdit::singleline(&mut self.logs_process)
                        .id_salt("system_logs_process")
                        .hint_text("Process name (optional)")
                        .char_limit(256),
                );
                ui.small("Native OS log · previous 120 seconds · at most 200 entries");
                #[cfg(target_os = "android")]
                ui.small("logcat main · current effective UID visible logs only");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for log in self.logs.iter().filter(|log| {
                        format!("{} {}", log.process, log.message)
                            .to_lowercase()
                            .contains(&filter)
                    }) {
                        ui.small(format!("{} · {}", log.timestamp, log.process));
                        ui.monospace(&log.message);
                        ui.separator();
                    }
                });
            }
            Page::Timeline | Page::Xray => {
                if self.page == Page::Timeline {
                    egui::CollapsingHeader::new(RichText::new("Timeline source").small().color(secondary_color))
                        .show(ui, |ui| {
                            ui.small("PTY command metadata is unverified observation. Times are captured when the parser receives it; arguments are omitted.");
                        });
                }
                if self.page == Page::Xray {
                    ui.horizontal(|ui| {
                        ui.label("PID");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.observation_pid)
                                .desired_width(80.0)
                                .char_limit(10),
                        );
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("Files and sockets"))
                            .clicked()
                        {
                            self.inspect_resources();
                        }
                    });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.observation_directory)
                            .hint_text("Directory to observe")
                            .char_limit(4096),
                    );
                    let active = self
                        .observation
                        .as_ref()
                        .is_some_and(|observation| observation.active());
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.busy && !active,
                                egui::Button::new("Observe 30 seconds"),
                            )
                            .clicked()
                        {
                            self.observe();
                        }
                        if active && ui.button("Stop observation").clicked() {
                            if let Some(observation) = &self.observation {
                                observation.stop();
                            }
                        }
                    });
                    if self.observation_dropped > 0 {
                        ui.small(format!(
                            "{} events dropped from the bounded collection queue",
                            self.observation_dropped
                        ));
                    }
                    egui::CollapsingHeader::new(RichText::new("Observation scope").small().color(secondary_color))
                        .show(ui, |ui| {
                            ui.small("macOS kernel fork/exec/exit notes and native directory events. Process notes can coalesce. File events have directory scope. Descriptor queries capture open files and socket endpoints at the displayed time. History retains the latest 2000 events.");
                        });
                    if let Some(resources) = &self.resources {
                        egui::CollapsingHeader::new(format!(
                            "PID {} descriptors · {} UTC",
                            resources.identity.pid,
                            clock(resources.at_ms)
                        ))
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.small(&resources.source);
                            if resources.truncated {
                                ui.small("Descriptor list limited to 1024 entries");
                            }
                            egui::ScrollArea::vertical()
                                .id_salt("process-descriptors")
                                .max_height(180.0)
                                .show(ui, |ui| {
                                    for resource in &resources.resources {
                                        ui.monospace(format!(
                                            "{} · {} {} {} · {}",
                                            resource.descriptor,
                                            resource.kind,
                                            resource.protocol.as_deref().unwrap_or_default(),
                                            resource.state.as_deref().unwrap_or_default(),
                                            resource.name
                                        ));
                                    }
                                });
                        });
                    }
                }
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for entry in self.trace.iter().filter(|entry| {
                            format!("{} {}", entry.source, entry.message)
                                .to_lowercase()
                                .contains(&filter)
                                && (self.page != Page::Xray
                                    || entry.source == "OS snapshot delta"
                                    || entry.source == "managed workspace API"
                                    || entry.source.contains("filesystem watcher")
                                    || entry.source.contains("observation"))
                        }) {
                            ui.horizontal(|ui| {
                                ui.small(format!("{} UTC", clock(entry.at_ms)));
                                ui.small(RichText::new(&entry.source).color(secondary_color));
                            });
                            let response = ui.label(&entry.message);
                            if let Some(event) = &entry.socket {
                                response.on_hover_ui(|ui| {
                                    if let Ok(detail) = serde_json::to_string_pretty(event) {
                                        ui.monospace(detail);
                                    }
                                });
                            }
                            ui.separator();
                        }
                    });
            }
            Page::Recovery | Page::Plugins | Page::ShellValues => {}
        }
        if let Some(detail) = detail {
            self.detail = Some(detail);
        }
        if let Some(operation) = operation {
            self.prepare(operation);
        }
    }
    pub fn dialogs(&mut self, ctx: &egui::Context) {
        self.extensions.dialogs(ctx);
        if let Some(detail) = self.detail.clone() {
            let mut open = true;
            egui::Window::new("System object")
                .open(&mut open)
                .default_width(550.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::both().max_height(500.0).show(ui, |ui| {
                        ui.monospace(detail);
                    });
                });
            if !open {
                self.detail = None;
            }
        }
        if let Some(prepared) = self.prepared.clone() {
            let mut confirm = false;
            let mut cancel = false;
            egui::Window::new("Confirm system operation")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&prepared.summary);
                    ui.small(
                        "This authorization is bound to this target and expires after 30 seconds.",
                    );
                    ui.horizontal(|ui| {
                        confirm = ui
                            .add_enabled(
                                !self.busy && now_ms() < prepared.expires_at_ms,
                                egui::Button::new("Confirm operation"),
                            )
                            .clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if cancel {
                self.cancel_pending();
            }
            if confirm {
                self.prepared = None;
                if let Some(broker) = self.broker.clone() {
                    self.worker(move || {
                        Update::Operation(
                            broker
                                .confirm(&prepared.ticket)
                                .and_then(|result| {
                                    anyhow::ensure!(result.success, "{}", result.message);
                                    Ok(result.message)
                                })
                                .map_err(|error| error.to_string()),
                        )
                    });
                }
            }
        }
    }
    pub fn blocks_terminal(&self) -> bool {
        self.prepared.is_some() || self.extensions.blocks_terminal()
    }
}

fn process_snapshot_changes(
    previous: &[system_api::ProcessInfo],
    current: &[system_api::ProcessInfo],
) -> Vec<String> {
    let old: HashMap<_, _> = previous
        .iter()
        .map(|process| (process.identity.pid, process))
        .collect();
    let current_identities: HashSet<_> = current
        .iter()
        .map(|process| (process.identity.pid, process.identity.start_time))
        .collect();
    let mut uncertain = HashSet::new();
    let mut changes = Vec::new();
    for process in current {
        let identity = &process.identity;
        if let Some(previous) = old.get(&identity.pid) {
            let before = &previous.identity;
            if before.start_time == identity.start_time {
                continue;
            }
            // macOS can temporarily fall back from the kernel's microsecond
            // birth identity to sysinfo's cached whole-second birth time.
            if cfg!(target_os = "macos")
                && before.start_time > 0
                && identity.start_time > 0
                && before.uid == identity.uid
                && before.exe == identity.exe
                && before.start_time / 1_000_000 == identity.start_time / 1_000_000
                && (before.start_time % 1_000_000 == 0 || identity.start_time % 1_000_000 == 0)
            {
                uncertain.insert(identity.pid);
                changes.push(format!(
                    "Observed process identity precision unavailable/changed · {} · PID {} · start {} → {}",
                    process.name, identity.pid, before.start_time, identity.start_time
                ));
                continue;
            }
        }
        changes.push(format!(
            "Observed process start · {} · PID {}",
            process.name, identity.pid
        ));
    }
    for process in previous {
        let identity = &process.identity;
        if !uncertain.contains(&identity.pid)
            && !current_identities.contains(&(identity.pid, identity.start_time))
        {
            changes.push(format!(
                "Observed process exit · {} · PID {}",
                process.name, identity.pid
            ));
        }
    }
    changes
}

fn retain_authorization<T>(discarded: bool, authorization: T, revoke: impl FnOnce(T)) -> Option<T> {
    if discarded {
        revoke(authorization);
        None
    } else {
        Some(authorization)
    }
}

fn clock(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        (seconds / 3600) % 24,
        (seconds / 60) % 60,
        seconds % 60
    )
}

fn process_order(snapshot: &SystemSnapshot, tree: bool) -> Vec<(usize, usize)> {
    if !tree {
        return (0..snapshot.processes.len())
            .map(|index| (index, 0))
            .collect();
    }
    let by_pid: HashMap<_, _> = snapshot
        .processes
        .iter()
        .enumerate()
        .map(|(index, process)| (process.identity.pid, index))
        .collect();
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (index, process) in snapshot.processes.iter().enumerate() {
        match process.parent_pid {
            Some(parent) if parent != process.identity.pid && by_pid.contains_key(&parent) => {
                children.entry(parent).or_default().push(index)
            }
            _ => roots.push(index),
        }
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<_> = roots.into_iter().rev().map(|index| (index, 0)).collect();
    while let Some((index, depth)) = stack.pop() {
        if !seen.insert(index) {
            continue;
        }
        result.push((index, depth));
        if let Some(next) = children.get(&snapshot.processes[index].identity.pid) {
            stack.extend(next.iter().rev().map(|index| (*index, depth + 1)));
        }
    }
    result.extend(
        (0..snapshot.processes.len())
            .filter(|index| !seen.contains(index))
            .map(|index| (index, 0)),
    );
    result
}

#[cfg(test)]
mod authorization_tests {
    use super::retain_authorization;
    use std::{cell::Cell, rc::Rc};

    struct BrokerProbe {
        revoked: Rc<Cell<usize>>,
        dropped: Rc<Cell<usize>>,
    }
    impl BrokerProbe {
        fn revoke(self) {
            self.revoked.set(self.revoked.get() + 1);
        }
    }
    impl Drop for BrokerProbe {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }

    #[test]
    fn discarded_late_authorization_revokes_and_drops_its_broker() {
        let revoked = Rc::new(Cell::new(0));
        let dropped = Rc::new(Cell::new(0));
        let broker = BrokerProbe {
            revoked: revoked.clone(),
            dropped: dropped.clone(),
        };
        assert!(retain_authorization(true, broker, BrokerProbe::revoke).is_none());
        assert_eq!(revoked.get(), 1);
        assert_eq!(dropped.get(), 1);
    }

    #[test]
    fn explicit_authorization_is_retained_until_its_owner_releases_it() {
        let revoked = Rc::new(Cell::new(0));
        let dropped = Rc::new(Cell::new(0));
        let broker = BrokerProbe {
            revoked: revoked.clone(),
            dropped: dropped.clone(),
        };
        let accepted = retain_authorization(false, broker, BrokerProbe::revoke).unwrap();
        assert_eq!(revoked.get(), 0);
        assert_eq!(dropped.get(), 0);
        drop(accepted);
        assert_eq!(dropped.get(), 1);
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    fn resources(start_time: u64, at_ms: u64, state: &str) -> system_api::ProcessResources {
        system_api::ProcessResources {
            at_ms,
            identity: system_api::ProcessIdentity {
                pid: 42,
                start_time,
                uid: "501".into(),
                exe: "/owned/socket-fixture".into(),
            },
            resources: vec![system_api::ProcessResource {
                descriptor: "7u".into(),
                kind: "IPv4".into(),
                name: "127.0.0.1:1234->127.0.0.1:4321".into(),
                protocol: Some("TCP".into()),
                state: Some(state.into()),
            }],
            truncated: false,
            source: "owned fixture".into(),
        }
    }

    #[test]
    fn socket_baseline_repetition_state_changes_and_pid_reuse_are_distinct() {
        let first = resources(100, 1000, "ESTABLISHED");
        let baseline = resource_socket_events(None, &first).unwrap();
        assert!(matches!(
            baseline.as_slice(),
            [SocketEvent {
                change: SocketChange::Baseline { connections: 1 },
                ..
            }]
        ));
        let repeated = resources(100, 2000, "ESTABLISHED");
        assert!(resource_socket_events(Some(&first), &repeated)
            .unwrap()
            .is_empty());
        let changed = resources(100, 3000, "CLOSE_WAIT");
        let events = resource_socket_events(Some(&repeated), &changed).unwrap();
        assert!(
            matches!(&events[0].change, SocketChange::StateChanged { previous_state: Some(state), .. } if state == "ESTABLISHED")
        );
        assert_eq!(events[0].previous_sampled_at_ms, Some(2000));
        let replacement = resources(200, 4000, "LISTEN");
        assert!(matches!(
            &resource_socket_events(Some(&changed), &replacement).unwrap()[0].change,
            SocketChange::Baseline { .. }
        ));
        let old = SocketSample::capture(3000, &[changed]).unwrap();
        let new = SocketSample::capture(4000, std::slice::from_ref(&replacement)).unwrap();
        let events = socket_events(Some(&old), &new);
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| matches!(&event.change, SocketChange::Appeared { connection, .. } if connection.start_time == 200)));
        assert!(events.iter().any(|event| matches!(&event.change, SocketChange::Disappeared { connection, .. } if connection.start_time == 100)));
        let mut incomplete = replacement;
        incomplete.truncated = true;
        assert!(resource_socket_events(None, &incomplete).is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod process_precision_tests {
    use super::*;

    fn process(start_time: u64) -> system_api::ProcessInfo {
        system_api::ProcessInfo {
            identity: system_api::ProcessIdentity {
                pid: 42,
                start_time,
                uid: "501".into(),
                exe: "/owned/process-fixture".into(),
            },
            name: "owned fixture".into(),
            cpu_percent: 0.0,
            rss_bytes: 0,
            parent_pid: None,
        }
    }

    #[test]
    fn full_to_floor_birth_precision_is_not_a_restart_and_true_reuse_is() {
        let full = process(1_794_498_026_883_325);
        let floor = process(1_794_498_026_000_000);
        let changes =
            process_snapshot_changes(std::slice::from_ref(&full), std::slice::from_ref(&floor));
        assert_eq!(changes.len(), 1);
        assert!(changes[0].contains("identity precision unavailable/changed"));
        let changes = process_snapshot_changes(std::slice::from_ref(&floor), &[]);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].contains("Observed process exit"));
        let changes =
            process_snapshot_changes(std::slice::from_ref(&floor), std::slice::from_ref(&full));
        assert_eq!(changes.len(), 1);
        assert!(changes[0].contains("identity precision unavailable/changed"));
        let replacement = process(1_794_498_026_999_999);
        let changes = process_snapshot_changes(
            std::slice::from_ref(&full),
            std::slice::from_ref(&replacement),
        );
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .any(|change| change.contains("Observed process start")));
        assert!(changes
            .iter()
            .any(|change| change.contains("Observed process exit")));
        let mut different_owner = floor;
        different_owner.identity.uid = "502".into();
        assert_eq!(
            process_snapshot_changes(std::slice::from_ref(&full), &[different_owner]).len(),
            2
        );
    }
}

// FVPROV1 80286 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOTgwYjM0NDBlZDE1NDIwZDU4Y2M3NDdlMzcyMzYwYjUxNDgyMTdkMzZlNTE3ZjAyM2FkYTI3MjYyZjdmMjM2MCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYjlmYTI3NjkwODlkNGIwOWNmYmFmNWVmZGRiYzY0NTM4MTM0NDBlNDc4YWM1ZDYxMjYxOTE4ZjNkZDU0ZmRkODlmNzRiNDA4N2U2Mzg1MTQ3NTVmMjE0ZGJkNTg0YjVjYTgxNTY4MjJmNzhjOGM2OGIxMjA5ZDdmMzFlY2UzMDMifV19XQ== FVPROV1-END
