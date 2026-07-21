//! ACP protocol layer: SDK integration for JSON-RPC communication.
//!
//! This module owns the `agent-client-protocol` SDK connection. It provides
//! typed async methods for all ACP operations (wrappers around the SDK's
//! `send_request` / `send_notification`) plus dedicated handlers for the
//! notifications and permission requests the CLI sends back.
//!
//! # Concurrency model
//!
//! We follow the SDK's documented best practice (see
//! `jsonrpc::Builder` "Event Loop and Concurrency" and
//! `jsonrpc::SentRequest::block_task` doc comments): `connect_with` runs the
//! SDK background actors on a dedicated tokio task; its `main_fn` completes
//! the `initialize` handshake, hands the resulting [`ConnectionTo<Agent>`] out
//! to this struct, and then parks on a shutdown oneshot until
//! [`AcpProtocol`] is dropped. The connection handle is `Clone + Send` and
//! is used directly by every method — outgoing requests / notifications go
//! through the SDK's own outgoing actor, so they are naturally concurrent.
//! No hand-rolled command channel is involved.
//!
//! This is what makes `session/cancel` preempt an in-flight `session/prompt`:
//! both requests are just `send_request` / `send_notification` calls on the
//! shared connection, each awaited in its own caller task.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use agent_client_protocol::schema::{
    AGENT_METHOD_NAMES, AuthenticateResponse, ClientNotification, ClientRequest, CloseSessionResponse, ExtResponse,
    ForkSessionResponse, Implementation, InitializeRequest, LoadSessionResponse, PromptResponse, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResumeSessionResponse,
    SelectedPermissionOutcome, SessionNotification, SetSessionConfigOptionResponse, SetSessionModeResponse,
    SetSessionModelResponse,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Responder, on_receive_notification, on_receive_request,
};
use aionui_common::ErrorChain;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, info, warn};

use crate::protocol::error::AcpError;
use crate::protocol::events::{self as stream_event, AgentStreamEvent};

use agent_client_protocol::schema::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, CancelNotification, CloseSessionRequest, ExtNotification,
    ExtRequest, ForkSessionRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    NewSessionRequest, NewSessionResponse, PromptRequest, ResumeSessionRequest, SetSessionConfigOptionRequest,
    SetSessionModeRequest, SetSessionModelRequest,
};

/// Timeout for the ACP initialize handshake (seconds).
const INIT_TIMEOUT_SECS: u64 = 30;

/// Timeout for the short config/mode/model RPCs (seconds). Intentionally
/// shorter than INIT_TIMEOUT_SECS; a dropped/absent response self-heals via retry.
const CONFIG_RPC_TIMEOUT_SECS: u64 = 10;

/// Client identity reported in the ACP `initialize` handshake (`clientInfo`).
///
/// Some agents forward these fields downstream as client metadata — e.g. Mistral
/// Vibe passes them to the Mistral API as `client_name`/`client_version`, which the
/// API rejects when empty. Always send non-empty values (see issue #3326).
const ACP_CLIENT_NAME: &str = "AionUi";
const ACP_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpConnectionPhase {
    Starting,
    Initializing,
    Ready,
    ShuttingDown,
}

/// Build the ACP `initialize` request, always populating `clientInfo` with a
/// non-empty name and version so downstream agents that require client metadata
/// (e.g. Mistral Vibe) accept the request. See issue #3326.
fn build_initialize_request() -> InitializeRequest {
    InitializeRequest::new(ProtocolVersion::LATEST)
        .client_info(Implementation::new(ACP_CLIENT_NAME, ACP_CLIENT_VERSION))
}

/// A pending permission request from the agent, awaiting user decision.
pub struct PermissionRequest {
    /// Raw ACP permission request as defined by the SDK schema.
    pub request: RequestPermissionRequest,
    /// Channel to send the user's decision back to the SDK responder.
    pub response_tx: oneshot::Sender<PermissionDecision>,
}

/// User's decision on a permission request.
pub enum PermissionDecision {
    /// User selected a permission option.
    Selected { option_id: String },
    /// User cancelled (rejected) the request.
    Cancelled,
}

/// ACP protocol handle: wraps the SDK connection and provides typed operations.
///
/// All request methods are thin wrappers over `connection.send_request(...)
/// .block_task().await` — safe because each caller runs in its own tokio
/// task, separate from the SDK background actors spawned by `connect_with`.
pub struct AcpProtocol {
    /// SDK connection handle. Cheap to clone (channel senders only) and
    /// shared by every method. Kept alive by the background task parked
    /// on `shutdown_rx` in `connect_with`'s `main_fn`.
    connection: ConnectionTo<Agent>,
    /// Signal dropped on `Drop` to make `main_fn` return, which in turn
    /// lets the SDK background actors shut down cleanly.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Flipped to `false` when the background task exits. Used by
    /// [`Self::is_connected`] as a fast synchronous check.
    alive: Arc<AtomicBool>,
    /// Cached initialize response from the ACP handshake.
    initialize_response: Arc<RwLock<Option<InitializeResponse>>>,
    /// Set to `true` for the duration of a `session/load` request so that
    /// the SDK notification handler skips broadcasting the CLI's historical
    /// `session/update` replay to the UI event channel. The flag does NOT
    /// affect `notification_tx` — internal session aggregate updates keep
    /// flowing so metadata like `available_commands_update` still reaches
    /// `event_tracker`.
    ///
    /// Owned by the outer struct; an `Arc` clone is captured by the SDK
    /// background task's `on_receive_notification` closure.
    replay_suppression: Arc<AtomicBool>,
}

#[allow(dead_code)] // Full ACP method set; some methods await wiring (fork, close, list, auth, ext).
impl AcpProtocol {
    /// Connect to a running CLI process and execute the ACP initialize handshake.
    ///
    /// Takes ownership of the child's stdin/stdout (from [`CliAgentProcess::take_stdio`]).
    /// Spawns the SDK background task for JSON-RPC message routing.
    /// Returns after the initialize handshake completes successfully.
    pub async fn connect(
        stdin: ChildStdin,
        stdout: ChildStdout,
        event_tx: broadcast::Sender<AgentStreamEvent>,
        permission_tx: mpsc::Sender<PermissionRequest>,
        notification_tx: mpsc::Sender<SessionNotification>,
    ) -> Result<Self, AcpError> {
        let alive = Arc::new(AtomicBool::new(true));
        let replay_suppression = Arc::new(AtomicBool::new(false));
        let started_at = std::time::Instant::now();
        log_acp_initialize_start();

        // Signals from the background task:
        // - `init_tx`: initialize handshake result (with possible SDK error)
        // - `ready_tx`: connection handle once init succeeded; if init fails
        //   this oneshot is dropped and the caller observes `NotConnected`
        let (init_tx, init_rx) = oneshot::channel::<Result<InitializeResponse, AcpError>>();
        let (ready_tx, ready_rx) = oneshot::channel::<ConnectionTo<Agent>>();

        // Signal from us → background task telling `main_fn` to return,
        // which triggers a clean SDK shutdown.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(run_sdk_background(
            stdin,
            stdout,
            event_tx,
            permission_tx,
            notification_tx,
            init_tx,
            ready_tx,
            shutdown_rx,
            Arc::clone(&alive),
            Arc::clone(&replay_suppression),
        ));

        // Wait for init to complete with timeout.
        let init_response = match tokio::time::timeout(std::time::Duration::from_secs(INIT_TIMEOUT_SECS), init_rx).await
        {
            Ok(Ok(Ok(response))) => {
                log_acp_initialize_success(started_at.elapsed().as_millis() as u64);
                response
            }
            Ok(Ok(Err(err))) => {
                log_acp_initialize_failed("agent_error", started_at.elapsed().as_millis() as u64);
                return Err(err);
            }
            Ok(Err(_)) => {
                log_acp_initialize_failed("channel_dropped", started_at.elapsed().as_millis() as u64);
                return Err(AcpError::Disconnected {
                    exit_code: None,
                    signal: None,
                    stderr: "Init channel dropped".into(),
                });
            }
            Err(_) => {
                log_acp_initialize_failed("timeout", started_at.elapsed().as_millis() as u64);
                return Err(AcpError::InitTimeout {
                    timeout_secs: INIT_TIMEOUT_SECS,
                });
            }
        };

        // `ready_rx` should resolve almost immediately after init_tx fires.
        let connection = ready_rx.await.map_err(|_| AcpError::NotConnected)?;

        Ok(Self {
            connection,
            shutdown_tx: Some(shutdown_tx),
            alive,
            initialize_response: Arc::new(RwLock::new(Some(init_response))),
            replay_suppression,
        })
    }

    pub fn initialize_response(&self) -> Option<InitializeResponse> {
        self.initialize_response.read().unwrap().clone()
    }

    pub fn agent_capabilities(&self) -> Option<AgentCapabilities> {
        self.initialize_response().map(|response| response.agent_capabilities)
    }

    pub fn auth_methods(&self) -> Option<Vec<AuthMethod>> {
        self.initialize_response().map(|response| response.auth_methods)
    }

    /// Create a new ACP session.
    pub async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.session_new).await
    }

    /// Load (resume) an existing ACP session.
    ///
    /// Backends that support `session/load` (e.g. Codex) will replay the
    /// entire conversation as `session/update` notifications between the
    /// moment the request is sent and the moment the response returns.
    /// Those replayed events are historical and must not reach the UI —
    /// the frontend already renders history from the local DB. The RAII
    /// `ReplaySuppressionGuard` flips `replay_suppression` on for the
    /// duration of the request so the SDK notification handler skips the
    /// UI broadcast path for replay events.
    ///
    /// Note: Claude resumes via `session/new` with `_meta.claudeCode.options.resume`
    /// and never calls this method, so it is unaffected by the guard.
    pub async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, AcpError> {
        let _guard = ReplaySuppressionGuard::new(&self.replay_suppression);
        self.send_request(req, AGENT_METHOD_NAMES.session_load).await
    }

    /// Fork an existing ACP session into a new session.
    pub async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.session_fork).await
    }

    /// Resume an existing ACP session.
    pub async fn resume_session(&self, req: ResumeSessionRequest) -> Result<ResumeSessionResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.session_resume).await
    }

    /// Close an ACP session.
    pub async fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.session_close).await
    }

    /// Send a prompt to the agent in an active session.
    ///
    /// Blocks until the agent returns a `PromptResponse` (turn completed).
    /// Streaming events arrive via the `event_tx` broadcast channel.
    pub async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.session_prompt).await
    }

    /// Cancel the current prompt in a session (fire-and-forget notification).
    pub fn cancel(&self, notification: CancelNotification) {
        if !self.is_connected() {
            return;
        }
        log_client_notify(AGENT_METHOD_NAMES.session_cancel, &json_str(&notification));
        let _ = self.connection.send_notification(notification);
    }

    /// Set the session mode.
    ///
    /// Bounded by `CONFIG_RPC_TIMEOUT_SECS`: a dropped or never-arriving
    /// response returns `AcpError::RequestTimeout` instead of hanging forever
    /// (see ELECTRON-3MS). Unlike `session/prompt`/`session/load`, this is a
    /// short config RPC, so the timeout does not truncate a long-running turn.
    /// The timeout is applied via [`Self::send_config_request`], which keeps the
    /// in-flight SDK request alive on timeout (see that method for why).
    pub async fn set_mode(&self, req: SetSessionModeRequest) -> Result<SetSessionModeResponse, AcpError> {
        self.send_config_request(
            req,
            AGENT_METHOD_NAMES.session_set_mode,
            std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS),
        )
        .await
    }

    /// Set the session model.
    ///
    /// Bounded by `CONFIG_RPC_TIMEOUT_SECS`; see [`Self::set_mode`].
    pub async fn set_model(&self, req: SetSessionModelRequest) -> Result<SetSessionModelResponse, AcpError> {
        self.send_config_request(
            req,
            AGENT_METHOD_NAMES.session_set_model,
            std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS),
        )
        .await
    }

    /// Set a session config option.
    ///
    /// Bounded by `CONFIG_RPC_TIMEOUT_SECS`; see [`Self::set_mode`].
    pub async fn set_config_option(
        &self,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, AcpError> {
        self.send_config_request(
            req,
            AGENT_METHOD_NAMES.session_set_config_option,
            std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS),
        )
        .await
    }

    /// List sessions, optionally filtered by working directory.
    pub async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.session_list).await
    }

    /// Authenticate with the agent using a previously advertised auth method.
    pub async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, AcpError> {
        self.send_request(req, AGENT_METHOD_NAMES.authenticate).await
    }

    /// Send an extension request (method name must start with `_`).
    ///
    /// Returns the raw JSON response value from the agent.
    pub async fn ext_request(&self, req: ExtRequest) -> Result<ExtResponse, AcpError> {
        self.ensure_connected()?;
        let method = format!("_{}", req.method);
        let wrapped = ClientRequest::ExtMethodRequest(req);
        let value = self.send_request(wrapped, &method).await?;
        let raw = serde_json::value::to_raw_value(&value).map_err(|e| AcpError::AgentInternal {
            message: format!("Failed to convert ext response: {e}"),
            code: -32603,
            data: None,
        })?;
        Ok(ExtResponse::new(raw.into()))
    }

    /// Send an extension notification (fire-and-forget, method name must start with `_`).
    pub fn ext_notify(&self, notification: ExtNotification) {
        if !self.is_connected() {
            return;
        }
        let method = format!("_{}", notification.method);
        log_client_notify(&method, &json_str(&notification));
        let wrapped = ClientNotification::ExtNotification(notification);
        let _ = self.connection.send_notification(wrapped);
    }

    /// Check whether the SDK connection is still alive.
    pub fn is_connected(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// Bounded config RPC that keeps the in-flight SDK request alive on timeout.
    ///
    /// The naive approach — `tokio::time::timeout(dur, self.send_request(..))` —
    /// drops the `send_request` future when the timeout fires, which drops the
    /// SDK response *receiver* while the matching subscriber is still registered.
    /// A later-arriving response then fails with `failed to send response,
    /// receiver dropped`, and the SDK surfaces that as a fatal error that tears
    /// down the entire ACP connection — collaterally cancelling any concurrent
    /// `session/prompt` (observed on the Claude backend as a `-32603`
    /// "oneshot canceled" turn failure; codex happens to hit the harmless
    /// `no subscriber found` path instead, but the defect is in this shared
    /// protocol layer and is backend-agnostic). See ELECTRON-3MS follow-up.
    ///
    /// Fix: run the SDK call on a detached task that *owns* the receiver, and
    /// bound only the caller-side await. On timeout the task is detached (a
    /// dropped `JoinHandle` does not abort), so a late response is delivered to
    /// a live-but-ignored receiver and discarded; the task then completes and
    /// drops cleanly, leaving the connection intact. The detached task is
    /// bounded by the connection lifetime — when the agent responds or the SDK
    /// connection closes, `block_task().await` resolves and the task exits.
    async fn send_config_request<Req>(
        &self,
        req: Req,
        method: &str,
        duration: std::time::Duration,
    ) -> Result<Req::Response, AcpError>
    where
        Req: agent_client_protocol::JsonRpcRequest + serde::Serialize + std::fmt::Debug + Send + 'static,
        Req::Response: serde::Serialize + std::fmt::Debug + Send + 'static,
    {
        self.ensure_connected()?;
        log_client_request(method, &json_str(&req));
        let connection = self.connection.clone();
        let method_owned = method.to_owned();
        let sdk_result = Self::await_config_rpc_detached(method, duration, async move {
            let rsp = connection.send_request(req).block_task().await;
            log_agent_response(&method_owned, &json_or_err(&rsp));
            rsp
        })
        .await?;
        sdk_result.map_err(|e| AcpError::from_sdk(e, method))
    }

    /// Await `fut` on a detached task, bounded by `duration`, mapping elapsed
    /// time into `AcpError::RequestTimeout`. On timeout the spawned task is
    /// detached (never aborted) so its in-flight work — the SDK response
    /// receiver — survives; see [`Self::send_config_request`] for why that
    /// matters. `duration` is a parameter so unit tests can drive it
    /// deterministically under a paused clock.
    async fn await_config_rpc_detached<F>(
        method: &str,
        duration: std::time::Duration,
        fut: F,
    ) -> Result<F::Output, AcpError>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = tokio::spawn(fut);
        match tokio::time::timeout(duration, handle).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(join_err)) => Err(AcpError::AgentInternal {
                message: format!("{method} config RPC task panicked: {join_err}"),
                code: -32603,
                data: None,
            }),
            Err(_) => Err(AcpError::RequestTimeout {
                method: method.to_owned(),
                timeout_secs: duration.as_secs(),
            }),
        }
    }

    /// Shared request path: connectivity check, structured logging, SDK call.
    async fn send_request<Req>(&self, req: Req, method: &str) -> Result<Req::Response, AcpError>
    where
        Req: agent_client_protocol::JsonRpcRequest + serde::Serialize + std::fmt::Debug,
        Req::Response: serde::Serialize + std::fmt::Debug + Send,
    {
        self.ensure_connected()?;
        log_client_request(method, &json_str(&req));
        let rsp = self.connection.send_request(req).block_task().await;
        log_agent_response(method, &json_or_err(&rsp));
        rsp.map_err(|e| AcpError::from_sdk(e, method))
    }

    /// Return `Err(NotConnected)` if the connection is dead.
    fn ensure_connected(&self) -> Result<(), AcpError> {
        if self.is_connected() {
            Ok(())
        } else {
            Err(AcpError::NotConnected)
        }
    }
}

impl Drop for AcpProtocol {
    fn drop(&mut self) {
        // Releasing the oneshot wakes `main_fn` in the background task, which
        // returns, which drives SDK shutdown. The bg_task joins naturally
        // (we don't await it here — Drop can't be async; the task is
        // `tokio::spawn`ed, so it gets cleaned up by the runtime).
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Scoped guard: set an `AtomicBool` to `true` on construction, reset to
/// `false` on drop. Used to mark the inclusive time window of a
/// `session/load` request so that the SDK notification handler can
/// suppress UI broadcasts of the CLI's historical replay.
///
/// Using a guard (instead of manual `store` around `send_request`) ensures
/// the flag is cleared even if the future is cancelled, the request fails,
/// or the task panics.
struct ReplaySuppressionGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> ReplaySuppressionGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self { flag }
    }
}

impl Drop for ReplaySuppressionGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// Run the SDK `connect_with` future: register notification/request
/// handlers, execute the initialize handshake, publish the connection
/// handle, then park on the shutdown signal until [`AcpProtocol`] is dropped.
#[allow(clippy::too_many_arguments)]
async fn run_sdk_background(
    stdin: ChildStdin,
    stdout: ChildStdout,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    permission_tx: mpsc::Sender<PermissionRequest>,
    notification_tx: mpsc::Sender<SessionNotification>,
    init_tx: oneshot::Sender<Result<InitializeResponse, AcpError>>,
    ready_tx: oneshot::Sender<ConnectionTo<Agent>>,
    shutdown_rx: oneshot::Receiver<()>,
    alive: Arc<AtomicBool>,
    replay_suppression: Arc<AtomicBool>,
) {
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());

    // `init_tx` / `ready_tx` are consumed inside the main_fn closure; wrap
    // them in Option so we can .take() without moving out of captured state.
    let mut init_tx = Some(init_tx);
    let mut ready_tx = Some(ready_tx);
    let mut shutdown_rx = Some(shutdown_rx);
    let phase = Arc::new(Mutex::new(AcpConnectionPhase::Starting));
    let phase_for_main = Arc::clone(&phase);

    let result = Client
        .builder()
        .on_receive_notification(
            {
                let event_tx = event_tx.clone();
                let notification_tx = notification_tx.clone();
                let replay_suppression = Arc::clone(&replay_suppression);
                async move |notification: SessionNotification, _cx: ConnectionTo<Agent>| {
                    // Fan out the raw SDK notification to the manager's apply-loop
                    // FIRST, so session state is consistent by the time the UI
                    // event hits the broadcast channel. Swallow send errors — if
                    // the manager has dropped the receiver, session consistency
                    // is moot anyway (we're on our way down).
                    let _ = notification_tx.send(notification.clone()).await;

                    // During a session/load request, the CLI replays historical
                    // session/update notifications back to us. The frontend
                    // already renders history from the local DB, so broadcasting
                    // the replay would produce duplicate UI blocks. Keep feeding
                    // notification_tx (event_tracker still needs metadata like
                    // available_commands_update), but skip the UI broadcast.
                    if !replay_suppression.load(Ordering::Acquire) {
                        handle_session_notification(notification, &event_tx).await;
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                async move |request: RequestPermissionRequest, responder, _cx| {
                    handle_permission_request(request, responder, &permission_tx).await;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
            // Step 1 — initialize handshake. main_fn is the canonical place
            // to call `block_task` (see SDK `connect_with` doc example).
            let init_result = {
                let req = build_initialize_request();
                log_client_request("initialize", &json_str(&req));
                *phase_for_main.lock().unwrap() = AcpConnectionPhase::Initializing;
                let raw = connection.send_request(req).block_task().await;
                log_agent_response("initialize", &json_or_err(&raw));
                raw.map_err(|e| AcpError::from_sdk(e, "initialize"))
            };

            let Some(tx) = init_tx.take() else {
                return Ok(());
            };
            match init_result {
                Ok(resp) => {
                    let _ = tx.send(Ok(resp));
                }
                Err(err) => {
                    let _ = tx.send(Err(err));
                    // init failure: let main_fn return so SDK cleans up.
                    return Ok(());
                }
            }

            // Step 2 — publish the connection handle so the outer
            // AcpProtocol can start issuing requests.
            if let Some(tx) = ready_tx.take()
                && tx.send(connection).is_err()
            {
                // Owner dropped before we became ready — nothing more to do.
                return Ok(());
            }
            *phase_for_main.lock().unwrap() = AcpConnectionPhase::Ready;

            // Step 3 — keep the connection alive until AcpProtocol::drop
            // releases the shutdown oneshot.
            if let Some(rx) = shutdown_rx.take() {
                let _ = rx.await;
                *phase_for_main.lock().unwrap() = AcpConnectionPhase::ShuttingDown;
            }
            Ok(())
        })
        .await;

    alive.store(false, Ordering::Release);

    let close_phase = *phase.lock().unwrap();
    match result {
        Ok(_) => debug!(?close_phase, "ACP SDK connection closed normally"),
        Err(e) => warn!(?close_phase, error = %ErrorChain(&e), "ACP SDK connection closed with error"),
    }
}

/// Fan out a CLI session notification to the event broadcast channel.
async fn handle_session_notification(
    notification: SessionNotification,
    event_tx: &broadcast::Sender<AgentStreamEvent>,
) {
    log_agent_notify("session/update", &json_str(&notification));

    let events = stream_event::session_notification_to_events(&notification);
    for event in events {
        if let Err(e) = event_tx.send(event) {
            // broadcast::SendError means no active receivers — expected when
            // no subscribers are attached to this agent. Log at debug so it
            // doesn't spam after a turn finishes.
            debug!(error = %e, "Dropping ACP event: no active broadcast receivers");
        }
    }
}

/// Relay a CLI permission request to the pending-permission channel and
/// forward the user's decision back to the SDK responder.
async fn handle_permission_request(
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    event_tx: &mpsc::Sender<PermissionRequest>,
) {
    log_agent_request("session/request_permission", &json_str(&request));

    let (response_tx, response_rx) = oneshot::channel();

    if event_tx.send(PermissionRequest { request, response_tx }).await.is_err() {
        warn!("Permission channel closed, cancelling request");
        let _ = responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
        return;
    }

    let response = match response_rx.await {
        Ok(PermissionDecision::Selected { option_id }) => RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
        ),
        Ok(PermissionDecision::Cancelled) | Err(_) => {
            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
        }
    };

    log_client_response("session/request_permission", &json_str(&response));
    let _ = responder.respond(response);
}

/// Serialize a value to a compact JSON string, falling back to Debug on failure.
fn json_str(value: &(impl serde::Serialize + std::fmt::Debug)) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

/// Serialize the Ok side of a Result to JSON, or format the Err with Debug.
fn json_or_err<T: serde::Serialize + std::fmt::Debug, E: std::fmt::Debug>(result: &Result<T, E>) -> String {
    match result {
        Ok(v) => json_str(v),
        Err(e) => format!("{e:?}"),
    }
}

/// Returns `true` when the `session/update` notification body carries a
/// piece of the prompt-reply stream (high-frequency, high-volume content).
///
/// Unknown / new `sessionUpdate` kinds default to `false` so newly added
/// metadata events stay visible at `info!` until explicitly classified.
fn is_streaming_chunk(body: &str) -> bool {
    // Streaming chunks of the prompt reply: token-level message / thought
    // text, and the incremental tool_call / plan structures the agent emits
    // mid-response. Their `_update` siblings are part of the same stream.
    const STREAMING_KINDS: &[&str] = &[
        "agent_message_chunk",
        "agent_thought_chunk",
        "user_message_chunk",
        "tool_call",
        "tool_call_update",
        "plan",
    ];
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let kind = value
        .pointer("/update/sessionUpdate")
        .and_then(serde_json::Value::as_str);
    matches!(kind, Some(k) if STREAMING_KINDS.contains(&k))
}

struct AcpLogSummary {
    payload_bytes: usize,
    payload_json: bool,
    session_id: Option<String>,
    session_update_kind: Option<String>,
}

impl AcpLogSummary {
    fn from_payload(payload: &str) -> Self {
        let payload_bytes = payload.len();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return Self {
                payload_bytes,
                payload_json: false,
                session_id: None,
                session_update_kind: None,
            };
        };

        Self {
            payload_bytes,
            payload_json: true,
            session_id: string_pointer(&value, &["/sessionId", "/session_id"]),
            session_update_kind: string_pointer(&value, &["/update/sessionUpdate", "/update/session_update"]),
        }
    }
}

fn string_pointer(value: &serde_json::Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

fn log_acp_initialize_start() {
    info!(
        target: "aionui_feedback_diagnostics",
        diagnostic_event = "feedback.runtime.acp_initialize_start",
        timeout_secs = INIT_TIMEOUT_SECS,
        "feedback.runtime.acp_initialize_start"
    );
}

fn log_acp_initialize_success(elapsed_ms: u64) {
    info!(
        target: "aionui_feedback_diagnostics",
        diagnostic_event = "feedback.runtime.acp_initialize_success",
        elapsed_ms,
        "feedback.runtime.acp_initialize_success"
    );
}

fn log_acp_initialize_failed(failure_class: &'static str, elapsed_ms: u64) {
    warn!(
        target: "aionui_feedback_diagnostics",
        diagnostic_event = "feedback.runtime.acp_initialize_failed",
        failure_class = %failure_class,
        elapsed_ms,
        "feedback.runtime.acp_initialize_failed"
    );
}

/// Log a JSON-RPC request from AionUi to the ACP agent.
/// `session/prompt` carries large user input and stays at debug.
fn log_client_request(method: &str, body: &str) {
    let summary = AcpLogSummary::from_payload(body);
    if method == "session/prompt" {
        debug!(
            direction = "client_request",
            method,
            payload_bytes = summary.payload_bytes,
            payload_json = summary.payload_json,
            session_id = summary.session_id.as_deref().unwrap_or("none"),
            "[ACP] ->"
        );
    } else {
        info!(
            direction = "client_request",
            method,
            payload_bytes = summary.payload_bytes,
            payload_json = summary.payload_json,
            session_id = summary.session_id.as_deref().unwrap_or("none"),
            "[ACP] ->"
        );
    }
}

/// Log a JSON-RPC response from the ACP agent.
/// `session/prompt` reply is large; stays at debug.
fn log_agent_response(method: &str, body: &str) {
    let summary = AcpLogSummary::from_payload(body);
    if method == "session/prompt" {
        debug!(
            direction = "agent_response",
            method,
            payload_bytes = summary.payload_bytes,
            payload_json = summary.payload_json,
            session_id = summary.session_id.as_deref().unwrap_or("none"),
            "[ACP] <- ${method}"
        );
    } else {
        info!(
            direction = "agent_response",
            method,
            payload_bytes = summary.payload_bytes,
            payload_json = summary.payload_json,
            session_id = summary.session_id.as_deref().unwrap_or("none"),
            "[ACP] <- ${method}"
        );
    }
}

/// Log a fire-and-forget notification from AionUi to the agent.
fn log_client_notify(method: &str, body: &str) {
    let summary = AcpLogSummary::from_payload(body);
    info!(
        direction = "client_notify",
        method,
        payload_bytes = summary.payload_bytes,
        payload_json = summary.payload_json,
        session_id = summary.session_id.as_deref().unwrap_or("none"),
        "[ACP] -> ${method}"
    );
}

/// Log an inbound notification from the agent.
/// `session/update` requires per-kind filtering — streaming chunks stay at debug.
fn log_agent_notify(method: &str, body: &str) {
    let summary = AcpLogSummary::from_payload(body);
    if method == "session/update" && is_streaming_chunk(body) {
        debug!(
            direction = "agent_notify",
            method,
            payload_bytes = summary.payload_bytes,
            payload_json = summary.payload_json,
            session_id = summary.session_id.as_deref().unwrap_or("none"),
            session_update_kind = summary.session_update_kind.as_deref().unwrap_or("none"),
            "[ACP] <- ${method}"
        );
    } else {
        info!(
            direction = "agent_notify",
            method,
            payload_bytes = summary.payload_bytes,
            payload_json = summary.payload_json,
            session_id = summary.session_id.as_deref().unwrap_or("none"),
            session_update_kind = summary.session_update_kind.as_deref().unwrap_or("none"),
            "[ACP] <- ${method}"
        );
    }
}

/// Log an inbound request from the agent (e.g. session/request_permission).
fn log_agent_request(method: &str, body: &str) {
    let summary = AcpLogSummary::from_payload(body);
    info!(
        direction = "agent_request",
        method,
        payload_bytes = summary.payload_bytes,
        payload_json = summary.payload_json,
        session_id = summary.session_id.as_deref().unwrap_or("none"),
        "[ACP] <- ${method}"
    );
}

/// Log a JSON-RPC response from AionUi back to the agent.
fn log_client_response(method: &str, body: &str) {
    let summary = AcpLogSummary::from_payload(body);
    info!(
        direction = "client_response",
        method,
        payload_bytes = summary.payload_bytes,
        payload_json = summary.payload_json,
        session_id = summary.session_id.as_deref().unwrap_or("none"),
        "[ACP] -> ${method}"
    );
}

impl std::fmt::Debug for AcpProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpProtocol")
            .field("alive", &self.is_connected())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_logs(max_level: tracing::Level, f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || SharedBuf(Arc::clone(&buffer))
        };

        let subscriber = fmt::Subscriber::builder()
            .with_max_level(max_level)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn replay_suppression_guard_sets_and_clears_flag() {
        let flag = AtomicBool::new(false);
        assert!(!flag.load(Ordering::Acquire));

        {
            let _guard = ReplaySuppressionGuard::new(&flag);
            assert!(flag.load(Ordering::Acquire));
        }

        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn replay_suppression_guard_clears_on_panic_unwind() {
        // Use catch_unwind to ensure Drop runs even when the scope panics.
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let flag_for_closure = std::sync::Arc::clone(&flag);

        // &AtomicBool is not UnwindSafe (shared ref), so AssertUnwindSafe is required.
        // This test also relies on panic = "unwind" (the default); it would not run under panic = "abort".
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ReplaySuppressionGuard::new(&flag_for_closure);
            assert!(flag_for_closure.load(Ordering::Acquire));
            panic!("simulated failure inside load_session");
        }));

        assert!(!flag.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn replay_suppression_guard_scopes_to_future_lifetime() {
        // Simulate load_session's body: guard lives across an await point
        // then drops at function return. Verify the flag sees true during
        // the await and false afterward.
        let flag = Arc::new(AtomicBool::new(false));
        let flag_probe = Arc::clone(&flag);

        async fn simulated_load(flag: &AtomicBool, probe: Arc<AtomicBool>) -> bool {
            let _guard = ReplaySuppressionGuard::new(flag);
            // Yield to the runtime so we know the guard survives .await.
            tokio::task::yield_now().await;
            probe.load(Ordering::Acquire)
        }

        let seen_during = simulated_load(&flag, Arc::clone(&flag_probe)).await;
        assert!(seen_during, "flag should be true inside guarded scope");
        assert!(!flag.load(Ordering::Acquire), "flag should be false after guard drop");
    }

    #[test]
    fn log_agent_notify_filters_streaming_chunks_and_omits_raw_body() {
        use tracing::Level;

        let captured = capture_logs(Level::INFO, || {
            log_agent_notify(
                "session/update",
                r#"{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hidden stream text"}}}"#,
            );
            log_agent_notify(
                "session/update",
                r#"{"sessionId":"s1","update":{"sessionUpdate":"current_mode_update","modeId":"yolo"}}"#,
            );
        });

        assert!(
            !captured.contains("agent_message_chunk"),
            "streaming chunk should NOT appear at info level: {captured}"
        );
        assert!(
            captured.contains("current_mode_update"),
            "non-streaming update should appear at info level: {captured}"
        );
        assert!(
            !captured.contains("hidden stream text"),
            "streaming content should not be logged: {captured}"
        );
        assert!(
            !captured.contains("modeId") && !captured.contains("yolo"),
            "raw notification body should not be logged: {captured}"
        );
        assert!(
            captured.contains("agent_notify"),
            "structured `direction` field should be `agent_notify`: {captured}"
        );
        assert!(
            captured.contains("session/update"),
            "structured `method` field should be present: {captured}"
        );
        assert!(
            captured.contains("payload_bytes"),
            "payload size summary should be present: {captured}"
        );
    }

    #[test]
    fn log_client_request_omits_prompt_body_even_at_debug_level() {
        use tracing::Level;

        let captured = capture_logs(Level::DEBUG, || {
            log_client_request(
                "session/prompt",
                r#"{"sessionId":"s1","prompt":[{"type":"text","text":"secret prompt text"}]}"#,
            );
        });

        assert!(
            captured.contains("client_request"),
            "structured `direction` field should be present: {captured}"
        );
        assert!(
            captured.contains("session/prompt"),
            "structured `method` field should be present: {captured}"
        );
        assert!(
            captured.contains("payload_bytes"),
            "payload size summary should be present: {captured}"
        );
        assert!(
            !captured.contains("secret prompt text") && !captured.contains("\"prompt\""),
            "raw prompt body should not be logged: {captured}"
        );
    }

    #[test]
    fn acp_initialize_diagnostic_log_helpers_use_stable_contract() {
        use tracing::Level;

        let captured = capture_logs(Level::INFO, || {
            super::log_acp_initialize_start();
            super::log_acp_initialize_success(123);
            super::log_acp_initialize_failed("timeout", 456);
        });

        assert!(captured.contains("aionui_feedback_diagnostics"), "{captured}");
        assert!(captured.contains("feedback.runtime.acp_initialize_start"), "{captured}");
        assert!(
            captured.contains("feedback.runtime.acp_initialize_success"),
            "{captured}"
        );
        assert!(
            captured.contains("feedback.runtime.acp_initialize_failed"),
            "{captured}"
        );
        assert!(captured.contains("timeout_secs=30"), "{captured}");
        assert!(captured.contains("failure_class=timeout"), "{captured}");
        assert!(captured.contains("elapsed_ms=456"), "{captured}");
    }

    #[test]
    fn is_streaming_chunk_recognises_prompt_stream_kinds() {
        // SDK delivers `params` already unwrapped — `body` here mirrors what
        // the log helpers receive: the JSON-RPC params object with `sessionId`
        // and `update` at the top level.
        let body_chunk = r#"{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#;
        let mode_update = r#"{"sessionId":"s1","update":{"sessionUpdate":"current_mode_update","modeId":"yolo"}}"#;
        let unknown = r#"{"sessionId":"s1","update":{"sessionUpdate":"future_unknown_kind"}}"#;
        let malformed = "not json";

        assert!(is_streaming_chunk(body_chunk));
        assert!(!is_streaming_chunk(mode_update));
        assert!(!is_streaming_chunk(unknown), "unknown kinds default to keep");
        assert!(!is_streaming_chunk(malformed), "malformed bodies default to keep");
    }

    #[test]
    fn initialize_request_sends_non_empty_client_info() {
        // Regression test for #3326: agents like Mistral Vibe forward clientInfo
        // downstream as client_name/client_version and reject empty values.
        let req = build_initialize_request();

        let client_info = req.client_info.as_ref().expect("clientInfo must be present");
        assert_eq!(client_info.name, "AionUi");
        assert!(!client_info.name.is_empty(), "client name must not be empty");
        assert!(!client_info.version.is_empty(), "client version must not be empty");

        // The serialized handshake must carry non-empty camelCase clientInfo fields.
        let json = serde_json::to_value(&req).expect("request serializes");
        assert_eq!(json["clientInfo"]["name"], "AionUi");
        assert_ne!(json["clientInfo"]["version"], "");
    }

    #[tokio::test(start_paused = true)]
    async fn await_config_rpc_detached_maps_stuck_future_to_request_timeout() {
        // A never-returning config RPC (dropped/absent response) must resolve
        // to RequestTimeout — not hang forever — for each of the three
        // set-path methods (§10.1). Under `start_paused`, the runtime
        // auto-advances the clock to the timer while the only task is blocked
        // on the timeout, so awaiting resolves deterministically without
        // wall-clock delay.
        for method in [
            AGENT_METHOD_NAMES.session_set_config_option,
            AGENT_METHOD_NAMES.session_set_mode,
            AGENT_METHOD_NAMES.session_set_model,
        ] {
            let result = AcpProtocol::await_config_rpc_detached(
                method,
                std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS),
                std::future::pending::<()>(),
            )
            .await;
            match result {
                Err(AcpError::RequestTimeout {
                    method: m,
                    timeout_secs,
                }) => {
                    assert_eq!(m, method);
                    assert_eq!(timeout_secs, CONFIG_RPC_TIMEOUT_SECS);
                }
                other => panic!("expected RequestTimeout for {method}, got {other:?}"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn await_config_rpc_detached_passes_through_ready_ok() {
        // Success path: a fast-completing RPC returns its Ok value unchanged.
        let result = AcpProtocol::await_config_rpc_detached(
            AGENT_METHOD_NAMES.session_set_config_option,
            std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS),
            async { Ok::<_, AcpError>(()) },
        )
        .await;
        assert!(matches!(result, Ok(Ok(()))), "ready Ok must pass through: {result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn await_config_rpc_detached_leaves_in_flight_task_running_on_timeout() {
        // Regression for the ELECTRON-3MS follow-up (Claude -32603 / connection
        // teardown): when a config RPC times out, the underlying SDK request
        // future must NOT be aborted. If it were, the SDK response receiver
        // would be dropped while the subscriber is still registered, and a
        // late response would hit `failed to send response, receiver dropped`,
        // tearing down the whole ACP connection and killing any concurrent
        // `session/prompt`.
        //
        // We model the SDK call as a spawned task that only finishes *after*
        // the timeout, and assert that (a) the caller sees RequestTimeout and
        // (b) the task still runs to completion — i.e. it was detached, not
        // aborted (which is what keeps the real response receiver alive).
        let completed = Arc::new(AtomicBool::new(false));
        let flag = completed.clone();

        let result = AcpProtocol::await_config_rpc_detached(
            AGENT_METHOD_NAMES.session_set_config_option,
            std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS),
            async move {
                // Resolves well after the caller-side timeout fires.
                tokio::time::sleep(std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS * 3)).await;
                flag.store(true, Ordering::SeqCst);
            },
        )
        .await;

        assert!(
            matches!(result, Err(AcpError::RequestTimeout { .. })),
            "timeout must map to RequestTimeout: {result:?}"
        );
        assert!(
            !completed.load(Ordering::SeqCst),
            "in-flight task must not have completed yet at the moment of timeout"
        );

        // Advance past the in-flight task's own timer; a detached (not aborted)
        // task keeps running and eventually completes.
        tokio::time::sleep(std::time::Duration::from_secs(CONFIG_RPC_TIMEOUT_SECS * 3)).await;
        tokio::task::yield_now().await;
        assert!(
            completed.load(Ordering::SeqCst),
            "timed-out config RPC task must survive the timeout (detached, not aborted)"
        );
    }
}
