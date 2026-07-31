//! `AntigravityConnection` / `AntigravitySessionBackend` — the direct-CLI
//! backend for the `agy` CLI.
//!
//! Shape: ONE PROCESS PER TURN. Unlike the claude lane (a persistent process
//! fed through a retained stdin FIFO), agy has no `--input-format`, so a turn
//! is a complete `agy -p …` invocation that exits when the turn ends.
//! Continuity across turns comes from `--conversation <id>`, which agy resumes
//! from its own on-disk store; the id arrives in the `init` frame and is kept
//! as this session's resume anchor.
//!
//! Consequences of that shape:
//! - `open_session` spawns NOTHING. It only registers the session; the first
//!   process appears on the first `Send`.
//! - There is no mid-turn steering wire (agy ignores stdin once running —
//!   verified), so `supported_commands.steer` is false.
//! - `Cancel` kills the process; the turn's own `result` frame may never
//!   arrive, so the reader synthesizes the terminal event on exit.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aionui_common::CommandSpec;
use aionui_process::Spawner;
use futures_util::stream::BoxStream;
use tokio::sync::{Mutex, broadcast, oneshot};

use super::argv::{ArgvInput, build_argv};
use super::mcp_config::write_mcp_config;
use super::models::probe_models;
use super::translate::Translator;
use super::wire::parse_line;
use crate::backend::types::{
    Admission, BackendError, CancelTarget, Command, CommandReceipt, ContentBlock, PendingPermissionView,
    PermissionDecision, SessionEnvelope, SessionSpec,
};
use crate::backend::{BackendConnection, SessionBackend, SessionConfig};
use crate::capability::{
    BlockSet, Capabilities, CapabilityTier, CommandSet, ModeInfo, ModelInfo, PromptAcceptedSource, SignalSet,
};
use crate::event::{PermissionKind, SessionEvent, TurnOutcome};

/// Broadcast backlog for a session's event stream. Matches the other backends:
/// large enough that a slow subscriber does not lose a turn's worth of frames.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// agy's declared capability surface.
pub fn antigravity_capabilities() -> Capabilities {
    Capabilities {
        tier: CapabilityTier::Parsed,
        emits: SignalSet {
            heartbeat: false,
            tool_lifecycle: true,
            terminal_result: true,
        },
        supported_commands: CommandSet {
            // agy ignores stdin once a turn is running, so there is no wire to
            // steer through. Queueing for the NEXT turn is a separate axis.
            steer: false,
            cancel_tool: false,
            answer_permission: true,
            answer_auth: false,
            acknowledge: true,
            set_mode: true,
            set_model: true,
            rewind: false,
            list_checkpoints: false,
            query_session_info: false,
        },
        prompt_blocks: BlockSet {
            text: true,
            // `agy -p` takes a text prompt only — it has no image input flag.
            image: false,
            audio: false,
            resource: false,
            at_mention: false,
        },
        // agy has no prompt-ack frame; the backend synthesizes one.
        prompt_accepted: PromptAcceptedSource::Synthesized,
        // A Send during a running turn is QUEUED for the next one. agy cannot
        // take input mid-turn, but its next turn is a fresh process anyway, so
        // the input box can stay usable instead of locking until the turn ends.
        accepts_proactive_input: true,
        ..Default::default()
    }
}

/// agy's fixed mode axis (`--mode`). Unlike models this never depends on the
/// account, so it needs no probe.
pub fn antigravity_modes() -> Vec<ModeInfo> {
    ["default", "accept-edits", "plan"]
        .into_iter()
        .map(|id| ModeInfo {
            id: id.to_owned(),
            name: match id {
                "accept-edits" => "Accept Edits".to_owned(),
                "plan" => "Plan".to_owned(),
                _ => "Default".to_owned(),
            },
            description: None,
        })
        .collect()
}

/// Connection-level factory. agy is 1:1 (one process per turn, one logical
/// session per backend handle), so this only carries the injected spawner.
pub struct AntigravityConnection {
    spawner: Arc<dyn Spawner>,
}

impl AntigravityConnection {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait::async_trait]
impl BackendConnection for AntigravityConnection {
    async fn open_session(
        &self,
        spec: SessionSpec,
        config: SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, BackendError> {
        // No process is spawned here: agy has nothing to keep alive between
        // turns. Resume simply pre-seeds the anchor the next `Send` will pass
        // through `--conversation`.
        let (session_id, anchor) = match spec {
            SessionSpec::Fresh { session_id } => (session_id, None),
            SessionSpec::Resume {
                session_id,
                backend_session_id,
            } => (session_id, backend_session_id),
        };
        // agy reads MCP servers from files only — there is no per-run flag — so
        // the session's servers (team coordination first, then the user's) have
        // to land in the workspace before the first turn spawns.
        if let Some(cwd) = config.cwd.as_deref()
            && let Err(e) = write_mcp_config(std::path::Path::new(cwd), &config.init.mcp_servers)
        {
            // Not fatal: the session still runs, just without MCP tools.
            tracing::warn!(error = %e, "antigravity: could not write mcp_config.json; MCP tools will be unavailable");
        }

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let backend = Arc::new(AntigravitySessionBackend {
            session_id,
            models: Arc::new(std::sync::RwLock::new(Vec::new())),
            config,
            spawner: Arc::clone(&self.spawner),
            event_tx,
            turn_gen: AtomicU64::new(0),
            anchor: Arc::new(Mutex::new(anchor)),
            current: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
        });

        // Discover models OFF the open path. `agy models` costs a process
        // launch, and blocking here would add that latency to every session
        // open for a list that only the picker needs. The catalog write-back
        // already polls `capabilities()` for late discovery, so it picks this
        // up when it lands.
        let _ = backend.weak_self.set(Arc::downgrade(&backend));
        backend.spawn_model_probe();
        Ok(backend)
    }

    async fn close_session(&self, _session_id: &str) -> Result<(), BackendError> {
        // Nothing to unbind: a session owns no transport between turns.
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        antigravity_capabilities()
    }
}

pub struct AntigravitySessionBackend {
    session_id: String,
    /// Models discovered from `agy models`, surfaced through `capabilities()`
    /// so the catalog write-back can populate the picker. Filled in by a
    /// background probe, hence the lock.
    models: Arc<std::sync::RwLock<Vec<ModelInfo>>>,
    config: SessionConfig,
    spawner: Arc<dyn Spawner>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    turn_gen: AtomicU64,
    /// agy conversation id to resume with. Set from the first `init` frame and
    /// kept for every later turn.
    anchor: Arc<Mutex<Option<String>>>,
    /// The in-flight turn's process, retained so `Cancel` / `terminate` can
    /// reach it. `None` between turns — that is the normal resting state.
    current: Arc<Mutex<Option<Arc<aionui_process::ManagedProcess>>>>,
    /// Tool approvals raised by the PreToolUse hook and not yet answered.
    ///
    /// Each entry parks a hook process (which is holding agy's tool call open)
    /// until the user answers in the UI. Everything that ends a turn must drain
    /// this as `Denied` — a hook left waiting blocks agy until its
    /// `--print-timeout`, which looks like a hang with no explanation.
    pending: Arc<Mutex<HashMap<String, PendingPermission>>>,
    /// Monotonic counter behind each permission's `request_id`.
    permission_seq: AtomicU64,
    /// True while a turn's process is alive. agy has no way to accept input
    /// mid-turn, so a Send arriving now has to wait for the next process.
    in_flight: Arc<AtomicBool>,
    /// Handle to itself, so the reader task can start the next queued turn when
    /// this one ends. `dispatch` only has `&self` (trait signature), so the
    /// Arc cannot be threaded through the call.
    weak_self: std::sync::OnceLock<std::sync::Weak<Self>>,
    /// Messages the user sent while a turn was running, in order.
    ///
    /// Each becomes its OWN next turn rather than being merged: merging would
    /// silently collapse two things the user said into one, and they would
    /// never see the second one echoed.
    queued: Arc<Mutex<VecDeque<Vec<ContentBlock>>>>,
}

struct PendingPermission {
    tool_name: String,
    answer: oneshot::Sender<PermissionDecision>,
}

impl AntigravitySessionBackend {
    /// Flatten a prompt into the single text argument `agy -p` accepts.
    /// Non-text blocks are dropped: `prompt_blocks` advertises text only, so
    /// the conversation layer never sends them.
    fn prompt_text(content: &[ContentBlock]) -> String {
        content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Kick off `agy models` in the background and store whatever it reports.
    ///
    /// Best-effort: a signed-out or missing agy simply leaves the list empty,
    /// which shows up as an empty picker rather than a failed session.
    fn spawn_model_probe(self: &Arc<Self>) {
        let spawner = Arc::clone(&self.spawner);
        let slot = Arc::clone(&self.models);
        let session_id = self.session_id.clone();
        let program = self
            .config
            .cli_program
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("agy"));
        tokio::spawn(async move {
            let found = probe_models(&spawner, &program, &session_id).await;
            if found.is_empty() {
                tracing::info!(
                    session_id = %session_id,
                    "antigravity: `agy models` returned nothing (not signed in?); the model picker stays empty"
                );
            } else {
                tracing::debug!(session_id = %session_id, count = found.len(), "antigravity: models discovered");
            }
            if let Ok(mut guard) = slot.write() {
                *guard = found;
            }
        });
    }

    fn emit(&self, turn_gen: u64, event: SessionEvent) {
        // A send error just means nobody is subscribed yet; the reducer is
        // driven by whoever holds `events()`, so this is not fatal.
        let _ = self.event_tx.send(SessionEnvelope {
            session_id: self.session_id.clone(),
            turn_gen,
            event,
        });
    }

    /// Answer every parked permission with `Denied`.
    ///
    /// Called whenever the turn stops being able to consume an answer (turn
    /// ended, cancelled, process gone). Each parked entry is a hook process
    /// holding one of agy's tool calls open; abandoning it would block agy
    /// until `--print-timeout` (5m by default) with nothing to explain the
    /// stall. Denying is also the safe reading of "the user never answered".
    async fn deny_all_pending(&self) {
        let drained: Vec<_> = self.pending.lock().await.drain().collect();
        for (request_id, entry) in drained {
            let _ = entry.answer.send(PermissionDecision::Denied);
            self.emit(
                self.turn_gen.load(Ordering::SeqCst),
                SessionEvent::PermissionResolved {
                    request_id,
                    kind: PermissionKind::Tool,
                },
            );
        }
    }

    async fn start_turn(&self, content: Vec<ContentBlock>) -> Result<u64, BackendError> {
        let turn_gen = self.turn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let input = ArgvInput {
            prompt: Self::prompt_text(&content),
            resume_conversation_id: self.anchor.lock().await.clone(),
            workspace: self.config.cwd.clone(),
            model: self.config.model.clone(),
            mode: self.config.mode.clone(),
        };
        let spec = CommandSpec {
            command: self
                .config
                .cli_program
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("agy")),
            args: build_argv(&input),
            env: self.config.spawn_env.clone(),
            cwd: self.config.cwd.clone(),
        };

        let proc = self
            .spawner
            .spawn(spec, &[], &self.session_id)
            .await
            .map_err(|e| BackendError::Transport(format!("spawn agy: {e}")))?;
        *self.current.lock().await = Some(Arc::clone(&proc));
        self.in_flight.store(true, Ordering::SeqCst);

        self.spawn_reader(Arc::clone(&proc), turn_gen);
        Ok(turn_gen)
    }

    /// Drain the process's stdout, translating each NDJSON line, and close the
    /// turn out when the process exits.
    fn spawn_reader(&self, proc: Arc<aionui_process::ManagedProcess>, turn_gen: u64) {
        let backend = self.weak_self.get().cloned();
        let event_tx = self.event_tx.clone();
        let session_id = self.session_id.clone();
        // The SESSION's anchor, not a fresh one: the id agy reports in `init`
        // is what the NEXT turn must pass through `--conversation`, so it has
        // to outlive this reader task.
        let anchor = Arc::clone(&self.anchor);
        let current = Arc::clone(&self.current);

        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};

            let mut translator = Translator::default();
            let mut saw_terminal = false;

            if let Some((_stdin, stdout)) = proc.take_stdio().await {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Some(ev) = parse_line(&line) else { continue };
                    for out in translator.translate(ev) {
                        if matches!(out, SessionEvent::TurnResult { .. }) {
                            saw_terminal = true;
                        }
                        let _ = event_tx.send(SessionEnvelope {
                            session_id: session_id.clone(),
                            turn_gen,
                            event: out,
                        });
                    }
                }
            }

            if let Some(id) = translator.backend_session_id() {
                *anchor.lock().await = Some(id.to_owned());
            }
            if translator.model_fallback_detected() {
                // agy drops an unusable `--model` silently: no error, no stderr
                // line. Without this the user just gets answers from a model
                // they did not choose.
                tracing::warn!(
                    session_id = %session_id,
                    "agy ignored the requested model and fell back to its default"
                );
            } else if let Some(model) = translator.current_model() {
                tracing::debug!(session_id = %session_id, model = %model, "agy turn model confirmed");
            }

            // A cancelled or crashed run exits without emitting `result`; the
            // FSM still needs a terminal, so synthesize one from the exit.
            if !saw_terminal {
                let exit = proc.wait_for_exit().await;
                let ok = exit.map(|s| s.success()).unwrap_or(false);
                let _ = event_tx.send(SessionEnvelope {
                    session_id: session_id.clone(),
                    turn_gen,
                    event: SessionEvent::TurnResult {
                        is_error: !ok,
                        api_error_status: None,
                        result_text: if ok {
                            String::new()
                        } else {
                            proc.peek_stderr_tail(20).await
                        },
                        epoch: 0,
                        outcome: if ok { TurnOutcome::EndTurn } else { TurnOutcome::Failed },
                    },
                });
            }
            // The turn owns no process any more; leaving a dead handle here
            // would make a later Cancel try to kill an exited pid.
            let mut slot = current.lock().await;
            if slot.as_ref().is_some_and(|p| Arc::ptr_eq(p, &proc)) {
                *slot = None;
            }
            drop(slot);

            // The turn is over, so anything the user typed while it ran can run
            // now — as its own turn, resuming the same agy conversation.
            let Some(backend) = backend.and_then(|w| w.upgrade()) else {
                // Session dropped; nothing left to run for.
                return;
            };
            backend.in_flight.store(false, Ordering::SeqCst);
            let next = backend.queued.lock().await.pop_front();
            if let Some(content) = next
                && let Err(e) = backend.start_turn(content).await
            {
                tracing::error!(error = %e, "antigravity: queued message could not be started");
            }
        });
    }
}

#[async_trait::async_trait]
impl SessionBackend for AntigravitySessionBackend {
    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        match command {
            Command::Send { content, .. } => {
                if self.in_flight.load(Ordering::SeqCst) {
                    self.queued.lock().await.push_back(content);
                    return Ok(CommandReceipt {
                        accepted: true,
                        admission: Admission::Queued,
                        turn_gen: self.turn_gen.load(Ordering::SeqCst),
                    });
                }
                let turn_gen = self.start_turn(content).await?;
                // agy has no prompt-ack frame of its own.
                self.emit(
                    turn_gen,
                    SessionEvent::PromptAccepted {
                        client_msg_id: String::new(),
                    },
                );
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::Started,
                    turn_gen,
                })
            }
            Command::AnswerPermission {
                request_id, decision, ..
            } => {
                let entry = self.pending.lock().await.remove(&request_id);
                match entry {
                    Some(p) => {
                        let _ = p.answer.send(decision);
                        self.emit(
                            self.turn_gen.load(Ordering::SeqCst),
                            SessionEvent::PermissionResolved {
                                request_id,
                                kind: PermissionKind::Tool,
                            },
                        );
                    }
                    // Already resolved (double click, or the turn ended and
                    // drained it). Accept it so the caller does not surface an
                    // error for a harmless race.
                    None => tracing::debug!(request_id = %request_id, "antigravity: permission already resolved"),
                }
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::Started,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::Cancel { target } => {
                if matches!(target, CancelTarget::Tool { .. }) {
                    return Err(BackendError::CommandNotSupported { command: "cancel_tool" });
                }
                self.terminate().await;
                // The user asked to stop — running what they queued afterwards
                // would be the opposite of what they meant.
                self.queued.lock().await.clear();
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::Started,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::Acknowledge { .. } => Ok(CommandReceipt {
                accepted: true,
                admission: Admission::Started,
                turn_gen: self.turn_gen.load(Ordering::SeqCst),
            }),
            // Mode/model are spawn-time flags for agy: there is no live process
            // to reconfigure, so the next turn simply picks up the new value.
            Command::SetMode { .. } | Command::SetModel { .. } => Ok(CommandReceipt {
                accepted: true,
                admission: Admission::Started,
                turn_gen: self.turn_gen.load(Ordering::SeqCst),
            }),
            Command::Steer { .. } => Err(BackendError::CommandNotSupported { command: "steer" }),
            Command::Rewind { .. } => Err(BackendError::CommandNotSupported { command: "rewind" }),
            Command::AnswerAuth { .. } => Err(BackendError::CommandNotSupported { command: "answer_auth" }),
            _ => Err(BackendError::CommandNotSupported { command: "unsupported" }),
        }
    }

    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        let rx = self.event_tx.subscribe();
        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(env) => return Some((env, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            available_models: self.models.read().map(|m| m.clone()).unwrap_or_default(),
            available_modes: antigravity_modes(),
            current_model: self.config.model.clone(),
            current_mode: self.config.mode.clone(),
            ..antigravity_capabilities()
        }
    }

    fn pending_permission_requests(&self) -> Vec<PendingPermissionView> {
        // Lets the REST `/confirmations` recovery path rebuild permission cards
        // after a page reload; without it a card raised before the client
        // subscribed is lost and the hook waits until agy's timeout.
        let Ok(pending) = self.pending.try_lock() else {
            return Vec::new();
        };
        pending
            .iter()
            .map(|(request_id, entry)| PendingPermissionView {
                request_id: request_id.clone(),
                tool_name: entry.tool_name.clone(),
                // agy has no AskUserQuestion equivalent: every hook request is a
                // plain allow/deny on a tool.
                questions: None,
            })
            .collect()
    }

    async fn terminate(&self) {
        if let Some(proc) = self.current.lock().await.take() {
            let _ = proc.kill(std::time::Duration::from_secs(2)).await;
        }
        // The process that would have consumed these answers is gone.
        self.deny_all_pending().await;
    }

    async fn request_external_permission(&self, tool_name: String, input: serde_json::Value) -> PermissionDecision {
        let request_id = format!("agy-perm-{}", self.permission_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            request_id.clone(),
            PendingPermission {
                tool_name: tool_name.clone(),
                answer: tx,
            },
        );

        self.emit(
            self.turn_gen.load(Ordering::SeqCst),
            SessionEvent::Permission {
                request_id: request_id.clone(),
                kind: PermissionKind::Tool,
                metadata: None,
                tool_name: Some(tool_name),
                input: Some(input),
            },
        );

        // No timeout here on purpose: a user may legitimately leave a permission
        // card unanswered for a long time, and the turn is allowed to wait.
        // Every path that ends the turn drains this table, so the wait always
        // terminates on a real event rather than a clock.
        match rx.await {
            Ok(decision) => decision,
            // Sender dropped without answering — treat as denial, never as
            // silent approval.
            Err(_) => PermissionDecision::Denied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::CommandMeta;
    use crate::testing::FakeSpawner;
    use serde_json::json;

    fn config(cwd: &str) -> SessionConfig {
        SessionConfig {
            cwd: Some(cwd.to_owned()),
            ..Default::default()
        }
    }

    async fn open(spawner: Arc<FakeSpawner>, spec: SessionSpec) -> Arc<dyn SessionBackend> {
        AntigravityConnection::new(spawner)
            .open_session(spec, config("/w"))
            .await
            .expect("open_session must not fail — agy spawns nothing until the first turn")
    }

    fn send(text: &str) -> Command {
        Command::Send {
            content: vec![ContentBlock::Text(text.to_owned())],
            metadata: CommandMeta::default(),
        }
    }

    #[tokio::test]
    async fn open_session_spawns_nothing() {
        // agy has no process to keep alive between turns; spawning at open
        // time would burn a process (and ~6s of startup) for nothing.
        let spawner = Arc::new(FakeSpawner::new());
        let _backend = open(
            Arc::clone(&spawner),
            SessionSpec::Fresh {
                session_id: "conv-1".into(),
            },
        )
        .await;
        assert_eq!(spawner.call_count(), 0);
    }

    #[tokio::test]
    async fn send_spawns_agy_through_the_injected_spawner() {
        let spawner = Arc::new(FakeSpawner::new());
        let backend = open(
            Arc::clone(&spawner),
            SessionSpec::Fresh {
                session_id: "conv-1".into(),
            },
        )
        .await;

        // FakeSpawner records the CommandSpec then errors (it cannot produce a
        // real process), so the dispatch surfaces Transport — the point of this
        // test is that the spawn went through the INJECTED spawner at all.
        let err = backend.dispatch(send("hello")).await.expect_err("fake spawner errors");
        assert!(matches!(err, BackendError::Transport(_)));
        assert_eq!(spawner.call_count(), 1);

        let spec = spawner.last_command().await.expect("recorded");
        assert_eq!(spec.command, std::path::PathBuf::from("agy"));
        assert!(spec.args.contains(&"-p".to_string()));
        assert!(spec.args.contains(&"hello".to_string()));
        assert!(spec.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(spec.cwd.as_deref(), Some("/w"));
    }

    #[tokio::test]
    async fn resume_seeds_the_conversation_flag_on_the_next_turn() {
        let spawner = Arc::new(FakeSpawner::new());
        let backend = open(
            Arc::clone(&spawner),
            SessionSpec::Resume {
                session_id: "conv-1".into(),
                backend_session_id: Some("agy-conv-9".into()),
            },
        )
        .await;

        let _ = backend.dispatch(send("again")).await;
        let spec = spawner.last_command().await.expect("recorded");
        let idx = spec
            .args
            .iter()
            .position(|a| a == "--conversation")
            .expect("resume must pass --conversation");
        assert_eq!(spec.args[idx + 1], "agy-conv-9");
    }

    #[tokio::test]
    async fn steering_is_rejected_because_agy_ignores_stdin_mid_turn() {
        let backend = open(
            Arc::new(FakeSpawner::new()),
            SessionSpec::Fresh {
                session_id: "conv-1".into(),
            },
        )
        .await;
        let err = backend
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Text("stop".into())],
            })
            .await
            .expect_err("steer must not be silently accepted");
        assert!(matches!(err, BackendError::CommandNotSupported { command: "steer" }));
    }

    /// Concrete handle so the permission tests can reach the inherent methods
    /// without going through `Arc<dyn SessionBackend>`.
    async fn backend_for_permissions() -> Arc<AntigravitySessionBackend> {
        let (event_tx, _) = broadcast::channel(16);
        Arc::new(AntigravitySessionBackend {
            session_id: "conv-1".into(),
            models: Arc::new(std::sync::RwLock::new(Vec::new())),
            config: config("/w"),
            spawner: Arc::new(FakeSpawner::new()),
            event_tx,
            turn_gen: AtomicU64::new(1),
            anchor: Arc::new(Mutex::new(None)),
            current: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    #[tokio::test]
    async fn a_hook_request_parks_until_the_user_answers() {
        let b = backend_for_permissions().await;
        let asker = Arc::clone(&b);
        let waiting =
            tokio::spawn(async move { asker.request_external_permission("run_command".into(), json!({})).await });

        // Wait for the request to register, then answer it as the UI would.
        let request_id = loop {
            if let Some(view) = b.pending_permission_requests().first() {
                break view.request_id.clone();
            }
            tokio::task::yield_now().await;
        };
        b.dispatch(Command::AnswerPermission {
            request_id,
            decision: PermissionDecision::Approved,
            selected: None,
            answers: Vec::new(),
        })
        .await
        .expect("answer accepted");

        assert_eq!(waiting.await.unwrap(), PermissionDecision::Approved);
        assert!(
            b.pending_permission_requests().is_empty(),
            "answered request must clear"
        );
    }

    #[tokio::test]
    async fn terminate_denies_parked_requests_instead_of_stranding_them() {
        // A stranded hook holds agy's tool call open until --print-timeout
        // (5 min), which the user experiences as an unexplained hang.
        let b = backend_for_permissions().await;
        let asker = Arc::clone(&b);
        let waiting =
            tokio::spawn(async move { asker.request_external_permission("run_command".into(), json!({})).await });

        while b.pending_permission_requests().is_empty() {
            tokio::task::yield_now().await;
        }
        b.terminate().await;

        assert_eq!(waiting.await.unwrap(), PermissionDecision::Denied);
        assert!(b.pending_permission_requests().is_empty());
    }

    #[tokio::test]
    async fn answering_an_unknown_request_is_not_an_error() {
        // Double-click, or an answer racing the turn's own drain.
        let b = backend_for_permissions().await;
        b.dispatch(Command::AnswerPermission {
            request_id: "agy-perm-does-not-exist".into(),
            decision: PermissionDecision::Approved,
            selected: None,
            answers: Vec::new(),
        })
        .await
        .expect("a resolved-twice answer must not surface an error");
    }

    #[tokio::test]
    async fn other_backends_deny_externally_raised_permissions_by_default() {
        // The trait default must never be "allow": a backend that does not
        // implement this has no way to ask the user.
        struct Bare;
        #[async_trait::async_trait]
        impl SessionBackend for Bare {
            async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
                unreachable!()
            }
            fn events(&self) -> BoxStream<'static, SessionEnvelope> {
                Box::pin(futures_util::stream::empty())
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::default()
            }
        }
        let decision = Bare.request_external_permission("x".into(), json!({})).await;
        assert_eq!(decision, PermissionDecision::Denied);
    }

    #[tokio::test]
    async fn capabilities_carry_the_discovered_models_and_fixed_modes() {
        // The catalog write-back reads capabilities() to populate the pickers;
        // if the discovered models never reach it, the model picker stays empty
        // and the user silently gets agy's default model.
        let (event_tx, _) = broadcast::channel(16);
        let backend = AntigravitySessionBackend {
            session_id: "conv-1".into(),
            models: Arc::new(std::sync::RwLock::new(vec![ModelInfo {
                id: "gemini-3.1-pro-high".into(),
                name: "gemini-3.1-pro-high".into(),
                description: None,
                reasoning_efforts: Vec::new(),
            }])),
            config: SessionConfig {
                cwd: Some("/w".into()),
                model: Some("gemini-3.1-pro-high".into()),
                mode: Some("plan".into()),
                ..Default::default()
            },
            spawner: Arc::new(FakeSpawner::new()),
            event_tx,
            turn_gen: AtomicU64::new(0),
            anchor: Arc::new(Mutex::new(None)),
            current: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
        };

        let caps = backend.capabilities();
        assert_eq!(
            caps.available_models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gemini-3.1-pro-high"]
        );
        assert_eq!(caps.current_model.as_deref(), Some("gemini-3.1-pro-high"));
        assert_eq!(
            caps.available_modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["default", "accept-edits", "plan"]
        );
        assert_eq!(caps.current_mode.as_deref(), Some("plan"));
    }

    #[test]
    fn modes_are_agys_three_and_carry_display_names() {
        let modes = antigravity_modes();
        assert_eq!(modes.len(), 3);
        assert_eq!(modes[1].id, "accept-edits");
        assert_eq!(modes[1].name, "Accept Edits");
    }

    #[tokio::test]
    async fn a_send_during_a_running_turn_is_queued_not_rejected() {
        // agy cannot take input mid-turn, but its next turn is a fresh process
        // anyway — so the input box stays usable instead of locking.
        let b = backend_for_permissions().await;
        b.in_flight.store(true, Ordering::SeqCst);

        let receipt = b.dispatch(send("second")).await.expect("queued send is accepted");
        assert!(receipt.accepted);
        assert_eq!(receipt.admission, Admission::Queued);
        assert_eq!(b.queued.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn queued_messages_stay_separate_and_ordered() {
        // Merging them would collapse two things the user said into one turn,
        // and the second would never be echoed back.
        let b = backend_for_permissions().await;
        b.in_flight.store(true, Ordering::SeqCst);

        b.dispatch(send("first")).await.unwrap();
        b.dispatch(send("second")).await.unwrap();

        let q = b.queued.lock().await;
        assert_eq!(q.len(), 2, "must not merge");
        assert!(matches!(&q[0][0], ContentBlock::Text(t) if t == "first"));
        assert!(matches!(&q[1][0], ContentBlock::Text(t) if t == "second"));
    }

    #[tokio::test]
    async fn cancel_discards_queued_messages() {
        // The user asked to stop; running what they queued afterwards would be
        // the opposite of what they meant.
        let b = backend_for_permissions().await;
        b.in_flight.store(true, Ordering::SeqCst);
        b.dispatch(send("queued")).await.unwrap();

        b.dispatch(Command::Cancel {
            target: CancelTarget::Turn,
        })
        .await
        .unwrap();
        assert!(b.queued.lock().await.is_empty());
    }

    #[test]
    fn capabilities_allow_queueing_but_not_steering() {
        let c = antigravity_capabilities();
        // agy ignores stdin mid-turn, so steering is impossible...
        assert!(!c.supported_commands.steer);
        // ...but queueing for the next turn is natural for a per-turn process.
        assert!(c.accepts_proactive_input);
    }

    #[test]
    fn capabilities_reflect_the_one_process_per_turn_shape() {
        let c = antigravity_capabilities();
        assert!(!c.supported_commands.steer, "agy ignores stdin mid-turn");
        assert!(c.supported_commands.answer_permission, "the hook bridge answers");
        assert!(c.supported_commands.set_mode && c.supported_commands.set_model);
        assert!(!c.supported_commands.rewind);
        assert!(c.prompt_blocks.text);
        assert!(!c.prompt_blocks.image, "`agy -p` has no image input");
        assert_eq!(c.prompt_accepted, PromptAcceptedSource::Synthesized);
    }
}
