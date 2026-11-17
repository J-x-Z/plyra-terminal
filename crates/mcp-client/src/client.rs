// SPDX-License-Identifier: GPL-3.0-only
use crate::{
    transport::{StdioCleanup, StdioTransport},
    *,
};
use anyhow::{ensure, Context, Result};
use rmcp::{
    model::{ClientCapabilities, ClientConfig, ClientRequest, Implementation, ProtocolVersion},
    service::{PeerRequestOptions, RunningService, ServiceError},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientLifecycleMode, ClientServiceExt, Peer, RoleClient,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as sync_mpsc, Arc, Condvar, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct Cancellation {
    token: CancellationToken,
    workers: Arc<Workers>,
}
#[derive(Default)]
struct Workers {
    state: Mutex<WorkerState>,
    changed: Condvar,
}
#[derive(Default)]
struct WorkerState {
    count: usize,
    budget: Duration,
    deadline: Option<Instant>,
}
struct WorkerRegistration(Arc<Workers>);
impl Drop for WorkerRegistration {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().count -= 1;
        self.0.changed.notify_all();
    }
}
impl Cancellation {
    pub fn cancel(&self) {
        self.token.cancel();
    }
    /// Used when the application exits while a connection is still starting.
    pub fn cancel_and_wait(&self) -> Result<()> {
        self.cancel();
        let mut state = self.workers.state.lock().unwrap();
        let budget = state.budget;
        let deadline = *state
            .deadline
            .get_or_insert_with(|| Instant::now() + budget);
        let (state, _) = self
            .workers
            .changed
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |state| state.count != 0,
            )
            .unwrap();
        ensure!(
            state.count == 0,
            "MCP connection cleanup exceeded its configured shutdown budget"
        );
        Ok(())
    }

    fn register_worker(&self, budget: Duration) -> Result<WorkerRegistration> {
        let mut state = self.workers.state.lock().unwrap();
        ensure!(!self.token.is_cancelled(), "MCP connection was cancelled");
        state.budget = state.budget.max(budget);
        state.count += 1;
        Ok(WorkerRegistration(self.workers.clone()))
    }
}

struct Shutdown {
    state: Mutex<ShutdownState>,
    changed: Condvar,
    budget: Duration,
}
#[derive(Default)]
struct ShutdownState {
    deadline: Option<Instant>,
    finished: bool,
    error: Option<String>,
}
impl Shutdown {
    fn new(budget: Duration) -> Self {
        Self {
            state: Mutex::new(ShutdownState::default()),
            changed: Condvar::new(),
            budget,
        }
    }

    fn finish(&self, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.error = error;
        state.finished = true;
        self.changed.notify_all();
    }

    fn wait(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let deadline = *state
            .deadline
            .get_or_insert_with(|| Instant::now() + self.budget);
        let (state, _) = self
            .changed
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |state| !state.finished,
            )
            .unwrap();
        ensure!(
            state.finished,
            "MCP server shutdown exceeded its configured time budget"
        );
        if let Some(error) = state.error.as_ref() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}
struct Inner {
    config: ServerConfig,
    info: ConnectionInfo,
    sender: tokio::sync::mpsc::Sender<Request>,
    cancellation: CancellationToken,
    request_scope: Mutex<CancellationToken>,
    active: Arc<AtomicBool>,
    shutdown: Arc<Shutdown>,
    tools: Mutex<HashSet<String>>,
    pending: Mutex<HashMap<String, Pending>>,
}
struct Pending {
    tool: String,
    arguments: Value,
    deadline: Instant,
}
struct Request {
    method: String,
    params: Value,
    deadline: Instant,
    cancelled: CancellationToken,
    reply: sync_mpsc::Sender<Result<Value>>,
}

impl Client {
    pub fn connect(config: ServerConfig) -> Result<Self> {
        Self::connect_with_cancel(config, Cancellation::default())
    }
    pub fn connect_with_cancel(config: ServerConfig, cancellation: Cancellation) -> Result<Self> {
        config.validate()?;
        ensure!(config.enabled, "MCP server is disabled in configuration");
        #[cfg(unix)]
        ensure!(
            unsafe { libc::geteuid() } != 0,
            "MCP clients must run with ordinary user permissions"
        );
        let (ready_tx, ready_rx) = sync_mpsc::channel();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let active = Arc::new(AtomicBool::new(false));
        let worker_config = config.clone();
        let worker_cancel = cancellation.token.clone();
        let worker_active = active.clone();
        let shutdown_budget = Duration::from_millis(
            config
                .shutdown_timeout_ms
                .saturating_mul(4)
                .saturating_add(2000),
        );
        let shutdown = Arc::new(Shutdown::new(shutdown_budget));
        let worker_shutdown = shutdown.clone();
        let registration = cancellation.register_worker(shutdown_budget)?;
        std::thread::Builder::new()
            .name(format!("mcp-{}", config.id))
            .spawn(move || {
                let _registration = registration;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let cleanup_result = match runtime {
                    Ok(runtime) => runtime.block_on(async move {
                        let cleanup = StdioCleanup::default();
                        let shutdown = Duration::from_millis(worker_config.shutdown_timeout_ms);
                        worker(
                            worker_config,
                            receiver,
                            ready_tx,
                            worker_cancel,
                            worker_active,
                            &cleanup,
                        )
                        .await;
                        cleanup.finish(shutdown).await
                    }),
                    Err(_) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("MCP runtime could not start")));
                        Err(anyhow::anyhow!("MCP runtime could not start"))
                    }
                };
                worker_shutdown.finish(cleanup_result.err().map(|error| error.to_string()));
            })?;
        let info = match wait_for_response(
            &ready_rx,
            Instant::now() + Duration::from_millis(config.connect_timeout_ms + 1000),
            &[&cancellation.token],
        ) {
            Ok(result) => result?,
            Err(error) => {
                cancellation.cancel();
                return Err(error);
            }
        };
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                info,
                sender,
                cancellation: cancellation.token,
                request_scope: Mutex::new(CancellationToken::new()),
                active,
                shutdown,
                tools: Mutex::new(HashSet::new()),
                pending: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn info(&self) -> ConnectionInfo {
        let mut info = self.inner.info.clone();
        info.connected = self.inner.active.load(Ordering::Acquire);
        info
    }

    fn request(&self, method: &str, params: Value) -> Result<Value> {
        ensure!(
            self.inner.active.load(Ordering::Acquire),
            "MCP server is disconnected"
        );
        ensure!(
            serde_json::to_vec(&params)?.len() <= self.inner.config.max_message_bytes,
            "MCP request exceeds its configured byte budget"
        );
        let (reply, receiver) = sync_mpsc::channel();
        let timeout = Duration::from_millis(self.inner.config.request_timeout_ms);
        let deadline = Instant::now() + timeout;
        let cancelled = self.inner.request_scope.lock().unwrap().clone();
        self.inner
            .sender
            .try_send(Request {
                method: method.into(),
                params,
                deadline,
                cancelled: cancelled.clone(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("MCP request queue is busy or disconnected"))?;
        wait_for_response(
            &receiver,
            deadline + Duration::from_millis(500),
            &[&self.inner.cancellation, &cancelled],
        )?
    }

    fn list(&self, method: &str, key: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut cursor = None;
        let mut seen = HashSet::new();
        for _ in 0..self.inner.config.max_list_items {
            let page = self.request(
                method,
                cursor
                    .as_ref()
                    .map(|cursor| json!({"cursor":cursor}))
                    .unwrap_or_else(|| json!({})),
            )?;
            let entries = page[key]
                .as_array()
                .context("MCP list response has an invalid catalog")?;
            items.extend(
                entries
                    .iter()
                    .take(self.inner.config.max_list_items.saturating_sub(items.len()))
                    .cloned(),
            );
            ensure!(
                serde_json::to_vec(&items)?.len() <= self.inner.config.max_output_bytes,
                "MCP catalog exceeds its configured output budget"
            );
            if items.len() == self.inner.config.max_list_items {
                return Ok(items);
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            let Some(next) = cursor.as_ref() else {
                return Ok(items);
            };
            ensure!(
                next.len() <= 4096 && seen.insert(next.clone()),
                "MCP pagination repeated a cursor or exceeded its limit"
            );
        }
        anyhow::bail!("MCP catalog exceeded its configured pagination limit")
    }

    pub fn list_tools(&self) -> Result<Vec<ToolInfo>> {
        let tools = self
            .list("tools/list", "tools")?
            .into_iter()
            .map(|tool| {
                Ok(ToolInfo {
                    name: required_name(&tool, "name")?,
                    title: optional_text(&tool, "title"),
                    description: optional_text(&tool, "description"),
                    input_schema: tool["inputSchema"].clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        *self.inner.tools.lock().unwrap() = tools.iter().map(|tool| tool.name.clone()).collect();
        Ok(tools)
    }
    pub fn list_resources(&self) -> Result<Vec<ResourceInfo>> {
        self.list("resources/list", "resources")?
            .into_iter()
            .map(|resource| {
                Ok(ResourceInfo {
                    uri: required_name(&resource, "uri")?,
                    name: required_name(&resource, "name")?,
                    title: optional_text(&resource, "title"),
                    description: optional_text(&resource, "description"),
                    mime_type: optional_text(&resource, "mimeType"),
                })
            })
            .collect()
    }
    pub fn list_prompts(&self) -> Result<Vec<PromptInfo>> {
        self.list("prompts/list", "prompts")?
            .into_iter()
            .map(|prompt| {
                let arguments = prompt["arguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|argument| {
                        Ok(PromptArgumentInfo {
                            name: required_name(argument, "name")?,
                            description: optional_text(argument, "description"),
                            required: argument["required"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(PromptInfo {
                    name: required_name(&prompt, "name")?,
                    title: optional_text(&prompt, "title"),
                    description: optional_text(&prompt, "description"),
                    arguments,
                })
            })
            .collect()
    }

    pub fn prepare_tool_call(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<PreparedToolCall> {
        ensure!(self.info().connected, "MCP server is disconnected");
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be a JSON object"
        );
        ensure!(
            self.inner.tools.lock().unwrap().contains(tool_name),
            "list and select an advertised MCP tool before preparing a call"
        );
        ensure!(
            serde_json::to_vec(arguments)?.len() <= self.inner.config.max_output_bytes,
            "MCP tool arguments exceed their configured byte budget"
        );
        let mut pending = self.inner.pending.lock().unwrap();
        pending.retain(|_, call| call.deadline > Instant::now());
        ensure!(
            pending.len() < 16,
            "too many MCP calls are awaiting confirmation"
        );
        let ticket = token()?;
        let timeout = Duration::from_millis(self.inner.config.confirmation_timeout_ms);
        pending.insert(
            ticket.clone(),
            Pending {
                tool: tool_name.into(),
                arguments: arguments.clone(),
                deadline: Instant::now() + timeout,
            },
        );
        Ok(PreparedToolCall {
            ticket,
            server_id: self.inner.config.id.clone(),
            tool_name: tool_name.into(),
            arguments: arguments.clone(),
            expires_at_ms: now_ms().saturating_add(self.inner.config.confirmation_timeout_ms),
        })
    }

    pub fn call_tool(&self, ticket: &str) -> Result<McpReply> {
        let pending = self
            .inner
            .pending
            .lock()
            .unwrap()
            .remove(ticket)
            .context("MCP confirmation expired, was cancelled, or was already consumed")?;
        ensure!(
            pending.deadline > Instant::now(),
            "MCP tool confirmation expired"
        );
        self.reply(
            "tools/call",
            json!({"name":pending.tool,"arguments":pending.arguments}),
        )
    }
    pub fn read_resource(&self, uri: &str) -> Result<McpReply> {
        validate_text(uri)?;
        self.reply("resources/read", json!({"uri":uri}))
    }
    pub fn get_prompt(&self, name: &str, arguments: &Value) -> Result<McpReply> {
        validate_text(name)?;
        let arguments = arguments
            .as_object()
            .context("MCP prompt arguments must be a JSON object")?;
        ensure!(
            arguments.values().all(Value::is_string),
            "MCP prompt argument values must be strings"
        );
        self.reply("prompts/get", json!({"name":name,"arguments":arguments}))
    }
    fn reply(&self, method: &str, params: Value) -> Result<McpReply> {
        let started = Instant::now();
        let data = self.request(method, params)?;
        Ok(McpReply {
            server_id: self.inner.config.id.clone(),
            method: method.into(),
            elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            data,
        })
    }

    pub fn revoke_pending_calls(&self) {
        self.inner.pending.lock().unwrap().clear();
    }
    pub fn cancel_active_requests(&self) {
        let mut scope = self.inner.request_scope.lock().unwrap();
        scope.cancel();
        *scope = CancellationToken::new();
    }
    pub fn disconnect(&self) -> Result<()> {
        self.revoke_pending_calls();
        self.cancel_active_requests();
        self.inner.cancellation.cancel();
        self.inner.shutdown.wait()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn wait_for_response<T>(
    receiver: &sync_mpsc::Receiver<T>,
    deadline: Instant,
    cancellations: &[&CancellationToken],
) -> Result<T> {
    loop {
        let disconnected = match receiver.try_recv() {
            Ok(value) => return Ok(value),
            Err(sync_mpsc::TryRecvError::Empty) => false,
            Err(sync_mpsc::TryRecvError::Disconnected) => true,
        };
        ensure!(
            !cancellations.iter().any(|token| token.is_cancelled()),
            "MCP operation was cancelled"
        );
        ensure!(!disconnected, "MCP response channel closed");
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("MCP operation timed out while awaiting its result")?;
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(value) => return Ok(value),
            Err(sync_mpsc::RecvTimeoutError::Timeout) => {}
            Err(sync_mpsc::RecvTimeoutError::Disconnected) => {
                // Recheck cancellation before classifying a concurrently closed channel.
            }
        }
    }
}

async fn connect_async(
    config: &ServerConfig,
    cleanup: &StdioCleanup,
) -> Result<RunningService<RoleClient, ClientConfig>> {
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("glass-terminal", env!("CARGO_PKG_VERSION")),
    );
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    };
    match &config.transport {
        TransportConfig::Stdio { .. } => client
            .serve_with_lifecycle(StdioTransport::spawn(config, cleanup)?, lifecycle)
            .await
            .map_err(startup_error),
        TransportConfig::StreamableHttp {
            url,
            bearer_token_env,
        } => {
            let mut transport = StreamableHttpClientTransportConfig::with_uri(url.clone());
            transport.max_sse_event_size = config.max_message_bytes;
            transport.reinit_on_expired_session = false;
            transport.max_concurrent_requests = 1;
            transport.control_request_timeout = Duration::from_millis(config.shutdown_timeout_ms);
            if let Some(reference) = bearer_token_env {
                transport.auth_header = Some(std::env::var(reference).with_context(|| {
                    format!("MCP token environment reference {reference} is unavailable")
                })?);
            }
            #[cfg(target_os = "android")]
            let transport = StreamableHttpClientTransport::with_client(
                crate::android_http_client()?,
                transport,
            );
            #[cfg(not(target_os = "android"))]
            let transport = StreamableHttpClientTransport::from_config(transport);
            client
                .serve_with_lifecycle(transport, lifecycle)
                .await
                .map_err(startup_error)
        }
    }
}

async fn worker(
    config: ServerConfig,
    mut receiver: tokio::sync::mpsc::Receiver<Request>,
    ready: sync_mpsc::Sender<Result<ConnectionInfo>>,
    cancellation: CancellationToken,
    active: Arc<AtomicBool>,
    cleanup: &StdioCleanup,
) {
    let connection = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(anyhow::anyhow!("MCP connection cancelled")),
        result = tokio::time::timeout(Duration::from_millis(config.connect_timeout_ms), connect_async(&config, cleanup)) => match result { Ok(result) => result, Err(_) => Err(anyhow::anyhow!("MCP connection timed out")) },
    };
    let mut service = match connection {
        Ok(service) => service,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let peer = service.peer().clone();
    let Some(server) = peer.peer_info() else {
        let _ = ready.send(Err(anyhow::anyhow!("MCP server metadata is unavailable")));
        let _ = service
            .close_with_timeout(Duration::from_millis(config.shutdown_timeout_ms))
            .await;
        return;
    };
    let info = ConnectionInfo {
        server_id: config.id.clone(),
        server_name: server
            .server_info
            .as_ref()
            .map(|info| info.name.clone())
            .unwrap_or_else(|| config.name.clone()),
        server_version: server
            .server_info
            .as_ref()
            .map(|info| info.version.clone())
            .unwrap_or_default(),
        protocol_version: server.protocol_version.to_string(),
        capabilities: serde_json::to_value(&server.capabilities).unwrap_or(Value::Null),
        transport: match &config.transport {
            TransportConfig::Stdio { .. } => "stdio",
            _ => "streamable_http",
        }
        .into(),
        connected_at_ms: now_ms(),
        connected: true,
    };
    active.store(true, Ordering::Release);
    if ready.send(Ok(info)).is_err() {
        cancellation.cancel();
    }
    let service_cancel = service.cancellation_token();
    let mut stopped = tokio::spawn(service.waiting());
    let mut already_stopped = false;
    loop {
        let request = tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = &mut stopped => { already_stopped = true; break; },
            request = receiver.recv() => match request { Some(request) => request, None => break },
        };
        let result = perform(&peer, &request, &config, &request.cancelled, &cancellation).await;
        let _ = request.reply.send(result);
    }
    active.store(false, Ordering::Release);
    if !already_stopped {
        service_cancel.cancel();
        let shutdown = Duration::from_millis(
            config
                .shutdown_timeout_ms
                .saturating_mul(3)
                .saturating_add(500),
        );
        if tokio::time::timeout(shutdown, &mut stopped).await.is_err() {
            stopped.abort();
            let _ = stopped.await;
        }
    }
}

async fn perform(
    peer: &Peer<RoleClient>,
    request: &Request,
    config: &ServerConfig,
    cancelled: &CancellationToken,
    connection: &CancellationToken,
) -> Result<Value> {
    ensure!(
        !cancelled.is_cancelled(),
        "MCP queued request was cancelled"
    );
    ensure!(!connection.is_cancelled(), "MCP connection is closed");
    let remaining = request
        .deadline
        .checked_duration_since(Instant::now())
        .context("MCP queued request expired before execution")?;
    let model: ClientRequest =
        serde_json::from_value(json!({"method":request.method,"params":request.params}))
            .context("unsupported MCP request")?;
    let handle = tokio::select! {
        result = tokio::time::timeout(remaining, peer.send_cancellable_request(model, PeerRequestOptions::with_timeout(remaining))) => result.context("MCP request send timed out")?.map_err(safe_error)?,
        _ = cancelled.cancelled() => anyhow::bail!("MCP request cancelled before send completed"),
        _ = connection.cancelled() => anyhow::bail!("MCP connection closed before send completed"),
    };
    let id = handle.id.clone();
    let response = tokio::select! {
        response = handle.await_response() => response.map_err(safe_error),
        _ = cancelled.cancelled() => Err(anyhow::anyhow!("MCP request cancelled")),
        _ = connection.cancelled() => Err(anyhow::anyhow!("MCP connection closed during request")),
    };
    if cancelled.is_cancelled() || connection.is_cancelled() {
        let _ = tokio::time::timeout(
            Duration::from_millis(config.shutdown_timeout_ms),
            peer.notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                Some(id),
                Some("user cancelled MCP request".into()),
            )),
        )
        .await;
    }
    let data = serde_json::to_value(response?)?;
    ensure!(
        serde_json::to_vec(&data)?.len() <= config.max_output_bytes,
        "MCP response exceeds its configured output byte limit"
    );
    Ok(data)
}

fn safe_error(error: ServiceError) -> anyhow::Error {
    match error {
        ServiceError::McpError(error) => {
            anyhow::anyhow!("MCP server rejected request with code {}", error.code.0)
        }
        ServiceError::Timeout { .. } => {
            anyhow::anyhow!("MCP request timed out; cancellation was requested")
        }
        ServiceError::Cancelled { .. } => anyhow::anyhow!("MCP request was cancelled"),
        ServiceError::TransportClosed => anyhow::anyhow!("MCP transport closed"),
        _ => anyhow::anyhow!("MCP transport returned an invalid or failed response"),
    }
}
fn startup_error(error: rmcp::service::ClientInitializeError) -> anyhow::Error {
    use rmcp::service::ClientInitializeError;
    if error.is_authorization_required() {
        return anyhow::anyhow!(
            "MCP server requires authentication; verify its environment token reference"
        );
    }
    match error {
        ClientInitializeError::NoCompatibleProtocolVersion { .. } => {
            anyhow::anyhow!("MCP server has no compatible protocol version")
        }
        ClientInitializeError::ConnectionClosed(_) => {
            anyhow::anyhow!("MCP server closed during protocol startup")
        }
        ClientInitializeError::Cancelled => anyhow::anyhow!("MCP protocol startup was cancelled"),
        ClientInitializeError::JsonRpcError(error) => anyhow::anyhow!(
            "MCP protocol startup was rejected with code {}",
            error.code.0
        ),
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => startup_error(*fallback),
        ClientInitializeError::TransportError { .. } => anyhow::anyhow!(
            "MCP startup transport failed; verify the program, endpoint and environment references"
        ),
        _ => anyhow::anyhow!("MCP server returned an invalid startup response"),
    }
}
fn required_name(value: &Value, field: &str) -> Result<String> {
    let text = value[field]
        .as_str()
        .context("MCP catalog field is missing")?;
    validate_text(text)?;
    Ok(text.into())
}
fn optional_text(value: &Value, field: &str) -> Option<String> {
    value[field].as_str().map(str::to_owned)
}
fn validate_text(text: &str) -> Result<()> {
    ensure!(
        !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control),
        "invalid MCP name or resource URI"
    );
    Ok(())
}
fn token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("MCP confirmation random source failed"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronous_wait_cancels_connection_and_request_scope_promptly() {
        for cancel_connection in [true, false] {
            let (_sender, receiver) = sync_mpsc::channel::<()>();
            let connection = CancellationToken::new();
            let request = CancellationToken::new();
            let signal = if cancel_connection {
                connection.clone()
            } else {
                request.clone()
            };
            let worker = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                signal.cancel();
            });
            let started = Instant::now();
            let error = wait_for_response(
                &receiver,
                started + Duration::from_secs(10),
                &[&connection, &request],
            )
            .unwrap_err();
            assert!(error.to_string().contains("cancelled"));
            assert!(started.elapsed() < Duration::from_millis(500));
            worker.join().unwrap();
        }
    }

    #[test]
    fn synchronous_wait_preserves_an_already_available_response() {
        let (sender, receiver) = sync_mpsc::channel();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        sender.send(42).unwrap();
        assert_eq!(
            wait_for_response(
                &receiver,
                Instant::now() + Duration::from_secs(1),
                &[&cancellation]
            )
            .unwrap(),
            42
        );
    }

    #[test]
    fn disconnect_waiters_share_one_absolute_deadline() {
        let shutdown = Arc::new(Shutdown::new(Duration::from_millis(60)));
        let other = shutdown.clone();
        let waiter = std::thread::spawn(move || other.wait());
        assert!(shutdown.wait().is_err());
        assert!(waiter.join().unwrap().is_err());
        let deadline = shutdown.state.lock().unwrap().deadline;
        assert!(deadline.unwrap() <= Instant::now());
        assert!(shutdown.wait().is_err());
        assert_eq!(shutdown.state.lock().unwrap().deadline, deadline);
        shutdown.finish(None);
        shutdown.wait().unwrap();
    }

    #[test]
    fn disconnect_completion_and_error_are_shared_between_waiters() {
        let shutdown = Arc::new(Shutdown::new(Duration::from_secs(1)));
        let other = shutdown.clone();
        let waiter = std::thread::spawn(move || other.wait());
        shutdown.finish(Some("owned cleanup failure".into()));
        assert_eq!(
            waiter.join().unwrap().unwrap_err().to_string(),
            "owned cleanup failure"
        );
        assert_eq!(
            shutdown.wait().unwrap_err().to_string(),
            "owned cleanup failure"
        );
    }

    #[test]
    fn connection_cancel_wait_reuses_deadline_and_closes_registration() {
        let cancellation = Cancellation::default();
        let registration = cancellation
            .register_worker(Duration::from_millis(40))
            .unwrap();
        assert!(cancellation.cancel_and_wait().is_err());
        let deadline = cancellation.workers.state.lock().unwrap().deadline;
        assert!(cancellation.cancel_and_wait().is_err());
        assert_eq!(
            cancellation.workers.state.lock().unwrap().deadline,
            deadline
        );
        assert!(cancellation
            .register_worker(Duration::from_secs(1))
            .is_err());
        drop(registration);
        cancellation.cancel_and_wait().unwrap();
    }
}

// FVPROV1 31278 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZTZjN2ZiMDQ4ZGVlMTM1ZGFkMTQ4ZTUyMTM1OTI3YzA4OTgzMTcwYjM4OTFhYzI2MTM5M2IzYjFjMDIwZWJjYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMzg1NGQwN2E3ZTBkYmE5ZTU5N2Y4ODg2MGQ3ZjAzZjhmY2Q2OWI2NDUyODA5MTA2YTM3MTllMmI2NGZhYjk2OTMzODJjZGZjYzEyOTRjYjJjOTZjZjhlMzc5Y2YwYWYwYThjOWU2M2NjMjczMTExMDM0YTRkM2ZjYzUxYzA4MDUifV19XQ== FVPROV1-END
