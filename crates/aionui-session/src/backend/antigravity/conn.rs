//! `AntigravityConnection` / `AntigravitySessionBackend` — the direct-CLI
//! backend for the `agy` CLI.
//!
//! Shape: ONE PROCESS PER TURN-BURST, driven through `--input-format stream-json
//! --output-format stream-json -p ""` (`agy changelog` 1.1.15; live-verified on
//! `agy 1.5.8`). `open_session` still spawns nothing; the first `Send` launches
//! `agy` and delivers the initial prompt as an NDJSON `{"event":"user",...}`
//! line on retained stdin.
//!
//! Mid-turn steering (`Command::Steer`):
//! - While the turn's stdin remains open, a `Steer` writes another
//!   `{"event":"user","message":{"role":"user","content":...}}` line to stdin
//!   and records its `client_msg_id` in `ActiveTurnIo::pending_steers`.
//! - Live-verified on `agy 1.5.8`: `agy` buffers the stdin frame while the
//!   current step runs, emits the intermediate `result` frame, and immediately
//!   consumes the steered `user_input` in the same warm process (`num_turns: 2`).
//!   `spawn_reader` folds intermediate `TurnResult` frames into `MessageDelta`
//!   and `MessageLifecycle::Completed`, closing stdin (`EOF`) only once the
//!   final steered turn settles, so `StreamRelay` stays continuously open on
//!   one `turn_gen`.
//! - Once the final `TurnResult` closes stdin, any late `Steer` returns
//!   `BackendError::Transport("no active turn to steer")`, triggering
//!   `ConversationService::deliver_midturn_message` to cleanly start a new turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aionui_common::CommandSpec;
use aionui_process::{BoxedStdin, BoxedStdout, Spawner};
use futures_util::stream::BoxStream;
use tokio::sync::{Mutex, broadcast, oneshot};

use super::argv::{ArgvInput, build_argv, effective_initial_prompt, encode_stream_user_message};
use super::models::probe_models;
use super::skills::skill_commands_from_dirs;
use super::translate::Translator;
use super::wire::parse_line;
use crate::adapter::{AgentIo, ManagedProcessIo};
use crate::backend::cli_version::session_drift_notice;
use crate::backend::types::{
    Admission, BackendError, CancelTarget, Command, CommandReceipt, ContentBlock, PendingPermissionView,
    PermissionDecision, SessionEnvelope, SessionSpec,
};
use crate::backend::{BackendConnection, SessionBackend, SessionConfig};
use crate::capability::{
    BlockSet, Capabilities, CapabilityTier, CommandSet, ModeInfo, ModelInfo, PromptAcceptedSource, SignalSet,
    SlashCommandInfo, block_kind_name,
};
use crate::event::{MessageLifecyclePhase, PermissionKind, SessionEvent, TurnOutcome};

/// The literal placeholder AionUi's team-creation flow persisted as a member's
/// model when an assistant had no concrete model (AionUi
/// `teamCreateModelResolver.ts`, fixed in the paired AionUi change). agy never
/// reports a model with this id, so it must not pass through the empty-list
/// "unknown" window below; already-persisted member sessions heal at runtime.
const UI_PLACEHOLDER_MODEL: &str = "default";

/// Broadcast backlog for a session's event stream. Matches the other backends:
/// large enough that a slow subscriber does not lose a turn's worth of frames.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Model list shared by every Antigravity session in this process.
///
/// The list belongs to the signed-in agy account, not to a conversation, and
/// discovering it costs a process launch. Caching it keeps the second and later
/// sessions from opening with an empty model picker.
///
/// Only non-empty lists are stored: an empty result means "not signed in" or
/// "probe failed", which must not be cached as though it were the answer.
static MODEL_CACHE: std::sync::OnceLock<std::sync::RwLock<Vec<ModelInfo>>> = std::sync::OnceLock::new();

fn model_cache() -> &'static std::sync::RwLock<Vec<ModelInfo>> {
    MODEL_CACHE.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

fn cached_models() -> Option<Vec<ModelInfo>> {
    let guard = model_cache().read().ok()?;
    (!guard.is_empty()).then(|| guard.clone())
}

fn store_models(models: &[ModelInfo]) {
    // Guard here rather than at the call site: caching "not signed in" would
    // pin an empty picker for the rest of the process.
    if models.is_empty() {
        return;
    }
    if let Ok(mut guard) = model_cache().write() {
        *guard = models.to_vec();
    }
}

/// How long a permission card may stay unanswered before we answer `deny` for
/// the user.
///
/// agy abandons a PreToolUse hook after ~30s and then runs the tool regardless
/// (measured against 1.1.9: a hook that blocked for 180s saw the tool execute
/// at 29.5s / 30.0s while it was still blocked). Deciding before that keeps the
/// refusal ours; leaving it to agy would mean silent approval.
const PERMISSION_ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Which text represents the turn's reply, given agy's terminal frame.
///
/// agy's `result.response` is the SAME reply the deltas carried, but assembled
/// once and therefore intact — the deltas are cut on byte boundaries and mangle
/// any multi-byte character that straddles two of them (verified against agy
/// 1.1.9: `--output-format stream-json` yields U+FFFD where `json`/`text` do
/// not). So the terminal frame wins whenever it actually carries the reply.
///
/// On a failed turn `result_text` is the error, not the reply, so the buffered
/// deltas are the only reply there is.
fn final_turn_text(is_error: bool, result_text: &str, buffered: &str) -> Option<String> {
    let authoritative = (!is_error && !result_text.is_empty()).then(|| result_text.to_owned());
    authoritative
        .or_else(|| (!buffered.is_empty()).then(|| buffered.to_owned()))
        .filter(|t| !t.is_empty())
}

/// Append one `text_delta` to the turn's buffered reply, collapsing the
/// U+FFFD run agy leaves at the join.
///
/// A multi-byte character agy cuts across two deltas arrives as a U+FFFD run —
/// a suffix on one delta plus a prefix on the next — that together stand for
/// exactly ONE lost character (measured against agy 1.1.14:
/// samples/antigravity-cli/1.1.14/print_stream-json_long_chinese_fffd.ndjson,
/// 49 of 49 splits are a 1+2 or 2+1 suffix/prefix pair, never mid-delta).
/// The bytes are gone before agy serializes the frame, so the character cannot
/// be restored — but it can be shown as one U+FFFD instead of three. Only the
/// join case is touched: a U+FFFD away from a boundary passes through, since
/// only the buffered fallback of a failed or cancelled turn ever renders this
/// text (a successful turn is replaced wholesale by `result.response`).
fn append_delta(buffered: &mut String, delta: &str) {
    const REPLACEMENT: char = '\u{FFFD}';
    let mut delta = delta;
    if buffered.ends_with(REPLACEMENT) && delta.starts_with(REPLACEMENT) {
        while buffered.ends_with(REPLACEMENT) {
            buffered.pop();
        }
        delta = delta.trim_start_matches(REPLACEMENT);
        buffered.push(REPLACEMENT);
    }
    buffered.push_str(delta);
}

/// Ensure an intermediate turn's reply ends with a blank line before a steered
/// follow-up turn's prose appends into `StreamRelay`.
fn ensure_turn_separator(mut text: String) -> String {
    if !text.is_empty() {
        if text.ends_with("\n\n") {
            // already paragraph-separated
        } else if text.ends_with('\n') {
            text.push('\n');
        } else {
            text.push_str("\n\n");
        }
    }
    text
}

/// Write one NDJSON `{"event":"user",...}` line + `\n` and flush without closing.
async fn write_stream_user_frame(stdin: &mut BoxedStdin, text: &str) -> Result<(), std::io::Error> {
    use tokio::io::AsyncWriteExt;

    let mut line = encode_stream_user_message(text);
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

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
            // `agy --input-format stream-json` (agy >= 1.1.15; live-verified on
            // 1.5.8) reads NDJSON `{"event":"user",...}` frames from retained
            // stdin while a turn is in flight and runs them in the same warm
            // process without respawning.
            steer: true,
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
            // `agy -p` stream-json takes text user messages only.
            image: false,
            audio: false,
            resource: false,
            at_mention: false,
        },
        // agy has no prompt-ack frame; the backend synthesizes one.
        prompt_accepted: PromptAcceptedSource::Synthesized,
        // Keep proactive queuing available for direct `Command::Send` callers.
        accepts_proactive_input: true,
        // `ConversationService::deliver_midturn_message` routes mid-turn user
        // messages through `Command::Steer` over the retained stream-json stdin,
        // falling back to a fresh turn when `"no active turn to steer"` races
        // turn completion.
        supports_midturn_delivery: true,
        ..Default::default()
    }
}

/// agy's fixed mode axis (`--mode`). Unlike models this never depends on the
/// account, so it needs no probe.
pub fn antigravity_modes() -> Vec<ModeInfo> {
    // `yolo` is NOT one of agy's modes — it is AionUi's sentinel for "run
    // without asking", offered here so a user can pick full auto deliberately
    // and so a teammate / scheduled run carries the same value. The backend
    // answers it by not installing its approval hook and never forwards it as
    // agy's `--mode`.
    [
        ("default", "Default", None),
        (
            "accept-edits",
            "Accept Edits",
            Some("File edits are auto-approved; commands still ask."),
        ),
        ("plan", "Plan", Some("Research and plan only.")),
        (
            "yolo",
            "Full Auto",
            Some("Every tool runs without asking. Used automatically by teams and scheduled runs."),
        ),
    ]
    .into_iter()
    .map(|(id, name, description)| ModeInfo {
        id: id.to_owned(),
        name: name.to_owned(),
        description: description.map(str::to_owned),
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
            // Product decision: antigravity has no headless fork surface (its
            // only fork is the interactive TUI `/fork`). Defensive reject — the
            // fork API already refuses agy via the capability gate.
            SessionSpec::Fork { .. } => {
                return Err(BackendError::Transport(
                    "antigravity does not support session forking".into(),
                ));
            }
        };
        // No workspace MCP file is written. agy does NOT read
        // `{workspace}/.agents/mcp_config.json`: measured with a purpose-built
        // stdio MCP server under `--dangerously-skip-permissions` (which
        // `argv.rs` always passes), the same server is invoked when configured
        // in `~/.gemini/config/mcp_config.json` and is NOT invoked from the
        // workspace file, where agy logs `empty component: prompt section
        // "mcp_servers"`. Its own docs list only the global path and
        // `plugins/<name>/mcp_config.json`, and `descriptor.rs` already declares
        // agy with no MCP transport, which routes Team coordination down the CLI
        // it was silently using anyway. So the writer was producing a file with
        // no consumer -- and doing it inside the user's workspace.

        // From the session's resolved skill dirs, not from the workspace: AionUi
        // no longer writes `.agents/skills`, and this source is exactly the
        // conversation's enabled set.
        let slash_commands = skill_commands_from_dirs(&config.init.skill_dirs);

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let backend = Arc::new(AntigravitySessionBackend {
            session_id,
            slash_commands,
            models: Arc::new(std::sync::RwLock::new(Vec::new())),
            mode_override: Arc::new(std::sync::RwLock::new(None)),
            config,
            spawner: Arc::clone(&self.spawner),
            event_tx,
            turn_gen: AtomicU64::new(0),
            anchor: Arc::new(Mutex::new(anchor)),
            current: Arc::new(Mutex::new(None)),
            turn_io: Arc::new(Mutex::new(ActiveTurnIo::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            turn_started_hooked: Arc::new(AtomicBool::new(false)),
            deferred_mode_confirm: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(test)]
            test_ios: Arc::new(Mutex::new(VecDeque::new())),
        });

        // Discover models OFF the open path. `agy models` costs a process
        // launch, and blocking here would add that latency to every session
        // open for a list that only the picker needs. The catalog write-back
        // already polls `capabilities()` for late discovery, so it picks this
        // up when it lands.
        let _ = backend.weak_self.set(Arc::downgrade(&backend));
        backend.spawn_model_probe();
        backend.spawn_version_check();
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

#[derive(Default)]
struct ActiveTurnIo {
    /// Retained stdin of the active `agy --input-format stream-json` process.
    /// `Some` while the running process can still accept mid-turn `Steer`
    /// frames; cleared (`None`) atomically when the final turn settles or on
    /// `Cancel` / `terminate` so EOF reaches `agy` and late steers fail cleanly
    /// with `"no active turn to steer"`.
    stdin: Option<BoxedStdin>,
    /// Correlation ids of `Command::Steer` frames already flushed to `stdin`
    /// and awaiting consumption as in-process follow-up turns.
    pending_steers: VecDeque<Option<String>>,
}

pub struct AntigravitySessionBackend {
    session_id: String,
    /// Models discovered from `agy models`, surfaced through `capabilities()`
    /// so the catalog write-back can populate the picker. Filled in by a
    /// background probe, hence the lock.
    models: Arc<std::sync::RwLock<Vec<ModelInfo>>>,
    /// Mode chosen at runtime, overriding the create-time seed.
    mode_override: Arc<std::sync::RwLock<Option<String>>>,
    config: SessionConfig,
    spawner: Arc<dyn Spawner>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    turn_gen: AtomicU64,
    /// agy conversation id to resume with. Set from the first `init` frame and
    /// kept for every later turn.
    anchor: Arc<Mutex<Option<String>>>,
    /// The in-flight turn's I/O handle, retained so `Cancel` / `terminate` can
    /// reach it. `None` between turns — that is the normal resting state.
    current: Arc<Mutex<Option<Arc<dyn AgentIo>>>>,
    /// Retained stream-json stdin + queued mid-turn steer correlation ids.
    turn_io: Arc<Mutex<ActiveTurnIo>>,
    /// Tool approvals raised by the PreToolUse hook and not yet answered.
    ///
    /// Each entry parks a hook process (which is holding agy's tool call open)
    /// until the user answers in the UI. Everything that ends a turn must drain
    /// this as `Denied` — a hook left waiting blocks agy until its
    /// `--print-timeout`, which looks like a hang with no explanation.
    pending: Arc<Mutex<HashMap<String, PendingPermission>>>,
    /// Monotonic counter behind each permission's `request_id`.
    permission_seq: AtomicU64,
    /// True while a turn's process is alive.
    in_flight: Arc<AtomicBool>,
    /// Whether the CURRENTLY RUNNING turn was spawned with the approval hook on disk.
    ///
    /// This is what decides whether a mid-turn mode switch can reach agy at all. The
    /// permission gate lives on the host — agy calls back per tool use and
    /// `request_external_permission` re-reads the mode each time — so a hooked turn
    /// honours a switch immediately, in both directions. A turn spawned in FULL AUTO has
    /// no hook, never calls back, and therefore cannot learn about a tightening until the
    /// next process starts; `sync_permission_hook` writes the file back, but nothing
    /// proves a running agy re-reads it.
    ///
    /// Recorded at spawn rather than derived from the current mode, because the current
    /// mode is exactly what the user just changed.
    turn_started_hooked: Arc<AtomicBool>,
    /// A mode switch accepted while the running turn could not hear it. Confirmed at the
    /// next spawn, where argv finally carries it.
    deferred_mode_confirm: Arc<Mutex<Option<String>>>,
    /// Skills provisioned into this workspace, exposed as slash commands. agy
    /// has no command-list interface, so this is read off the skill files.
    slash_commands: Vec<SlashCommandInfo>,
    /// Handle to itself, so the reader task can start the next queued turn when
    /// this one ends. `dispatch` only has `&self` (trait signature), so the
    /// Arc cannot be threaded through the call.
    weak_self: std::sync::OnceLock<std::sync::Weak<Self>>,
    /// Messages queued via direct `Command::Send` while a turn was running.
    ///
    /// Each becomes its OWN next turn rather than being merged: merging would
    /// silently collapse two things the user said into one, and they would
    /// never see the second one echoed.
    queued: Arc<Mutex<VecDeque<Vec<ContentBlock>>>>,
    /// Hermetic test seam: scripted `AgentIo` handles popped by `start_turn`
    /// ahead of `self.spawner` when populated. Empty in all non-scripted tests.
    #[cfg(test)]
    test_ios: Arc<Mutex<VecDeque<Arc<dyn AgentIo>>>>,
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
        // Same env conversation turns use (proxy / agent env_override). The
        // model registry call needs the same network path as a real turn.
        let spawn_env = self.config.spawn_env.clone();

        // Serve a list an earlier session already paid for. agy's models are a
        // property of the signed-in account, not of the conversation, so
        // re-running `agy models` per session buys nothing and costs ~3s during
        // which `get_config_options` reports no models at all — and the frontend
        // fetches those options once, so an empty first answer leaves the picker
        // stuck on its loading state for the whole session.
        if let Some(cached) = cached_models()
            && let Ok(mut guard) = slot.write()
        {
            *guard = cached;
            return;
        }

        tokio::spawn(async move {
            let found = probe_models(&spawner, &program, &session_id, &spawn_env).await;
            if !found.is_empty() {
                store_models(&found);
            }
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

    /// Tell the user once when the installed agy is not the release this
    /// integration was verified against.
    ///
    /// agy ships outside the app (a 157 MB binary Google distributes itself),
    /// so unlike the pinned claude/codex CLIs its version is whatever the user
    /// installed. Every wire contract here — the `stream-json` shapes, the hook
    /// protocol, `--conversation` resume — was verified against one release, and
    /// a drifting install should say so up front rather than fail in some
    /// unexplained way mid-turn.
    ///
    /// Reported once per process: the answer cannot change while agy's binary
    /// stays put, and repeating it on every session would be noise.
    fn spawn_version_check(self: &Arc<Self>) {
        let spawner = Arc::clone(&self.spawner);
        let session_id = self.session_id.clone();
        let program = self
            .config
            .cli_program
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("agy"));
        let weak = self.weak_self.get().cloned();
        tokio::spawn(async move {
            let Some((level, message, localized)) = session_drift_notice(&spawner, "agy", &program, &session_id).await
            else {
                return;
            };
            if let Some(backend) = weak.and_then(|w| w.upgrade()) {
                // Not `emit`: that drops the value when nobody is subscribed
                // yet, which is fine for a turn frame (another one follows) but
                // loses this notice outright — the check runs once per session.
                crate::backend::cli_version::broadcast_notice(
                    &backend.event_tx,
                    SessionEnvelope {
                        session_id: backend.session_id.clone(),
                        turn_gen: backend.turn_gen.load(Ordering::SeqCst),
                        event: SessionEvent::Notice {
                            level,
                            message,
                            localized: Some(localized),
                            supersedes_key: None,
                        },
                    },
                    "agy",
                )
                .await;
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

    /// Install or remove the approval hook so agy's view matches the current mode.
    ///
    /// Removal alone would be unsafe in reverse: with no `hooks.json` agy never
    /// calls back, so switching OUT of full auto has to put the file back or the
    /// session keeps running unattended behind a UI that says otherwise.
    fn sync_permission_hook(&self) {
        let Some(cwd) = self.config.cwd.as_deref() else {
            return;
        };
        let dir = std::path::Path::new(cwd).join(".agents");
        let path = dir.join("hooks.json");
        if self.is_full_auto() {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!(session_id = %self.session_id, "antigravity: full auto — approval hook removed")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(session_id = %self.session_id, error = %e, "antigravity: could not remove the approval hook")
                }
            }
            return;
        }
        let Some(body) = self.config.permission_hook_body.as_deref() else {
            // No hook was ever configured for this session (no callback address),
            // so there is nothing to restore — and nothing gating tools either.
            tracing::warn!(
                session_id = %self.session_id,
                "antigravity: leaving full auto but no approval hook is configured; tools stay unattended"
            );
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, body)) {
            tracing::warn!(session_id = %self.session_id, error = %e, "antigravity: could not restore the approval hook");
        } else {
            tracing::info!(session_id = %self.session_id, "antigravity: approval hook restored");
        }
    }

    /// The mode this session is on right now, honouring a runtime switch.
    fn effective_mode(&self) -> Option<String> {
        self.mode_override
            .read()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.config.mode.clone())
    }

    /// Mark the running turn as finished and release any switch it could not hear.
    ///
    /// The turn ending is what makes a deferred switch real: that agy process is gone, so
    /// nothing is running under the old mode any more and the next spawn necessarily
    /// reads the new one. Confirming here rather than at the next spawn matters because
    /// the next spawn needs the user to send another message — until then the picker
    /// would sit on "switching…" with no way to tell whether it had landed.
    ///
    /// `take()` makes this idempotent: a later turn boundary finds nothing to release.
    async fn settle_turn_end(&self) {
        self.in_flight.store(false, Ordering::SeqCst);
        if let Some(mode) = self.deferred_mode_confirm.lock().await.take() {
            let turn_gen = self.turn_gen.load(Ordering::SeqCst);
            tracing::info!(
                session_id = %self.session_id,
                mode = %mode,
                "antigravity: turn ended — releasing the held mode confirmation"
            );
            self.emit(
                turn_gen,
                SessionEvent::ConfigChanged {
                    mode: Some(mode),
                    model: None,
                },
            );
        }
    }

    /// Whether a mode switch made RIGHT NOW would govern the turn already running.
    ///
    /// Between turns: yes — the next spawn reads the new mode from argv either way.
    /// During a hooked turn: yes — agy calls back per tool use and the host gate re-reads
    /// the mode, so the switch governs the very next tool, in both directions.
    /// During a full-auto turn: no — that process was spawned without a hook, never calls
    /// back, and cannot be told; only the next spawn picks it up.
    fn switch_reaches_current_turn(&self) -> bool {
        !self.in_flight.load(Ordering::SeqCst) || self.turn_started_hooked.load(Ordering::SeqCst)
    }

    /// Whether this session currently runs without approval prompts.
    fn is_full_auto(&self) -> bool {
        self.effective_mode()
            .is_some_and(|m| m.eq_ignore_ascii_case(super::argv::FULL_AUTO_SENTINEL))
    }

    /// The model to actually pass to agy, dropping one it cannot accept.
    ///
    /// A model id can reach us that agy rejects outright ("invalid model
    /// selection"), which fails the whole turn: when discovery has not produced
    /// a list yet, the UI has no agy models to offer and falls back to whatever
    /// model the user last used elsewhere — a Gemini/Claude provider id that
    /// means nothing to agy. Running on agy's own default is far better than
    /// failing the conversation over a picker default the user never chose.
    ///
    /// Only filters once discovery has actually produced a list; an empty list
    /// means "unknown", not "nothing is valid", so the request passes through.
    /// The one exception is [`UI_PLACEHOLDER_MODEL`]: it is a known UI
    /// artifact, never a real agy id, so it is dropped even while the list is
    /// empty — this heals member sessions persisted before the AionUi fix.
    fn effective_model(&self) -> Option<String> {
        let requested = self.config.model.clone()?;
        let known = self.models.read().ok()?;
        if known.is_empty() {
            if requested == UI_PLACEHOLDER_MODEL {
                tracing::warn!(
                    session_id = %self.session_id,
                    requested = %requested,
                    "antigravity: dropping the UI placeholder model while discovery is empty; falling back to agy's default"
                );
                return None;
            }
            return Some(requested);
        }
        if known.iter().any(|m| m.id == requested) {
            return Some(requested);
        }
        tracing::warn!(
            session_id = %self.session_id,
            requested = %requested,
            "antigravity: requested model is not one agy reported; falling back to agy's default"
        );
        None
    }

    async fn start_turn(&self, content: Vec<ContentBlock>) -> Result<u64, BackendError> {
        let turn_gen = self.turn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let input = ArgvInput {
            prompt: Self::prompt_text(&content),
            resume_conversation_id: self.anchor.lock().await.clone(),
            workspace: self.config.cwd.clone(),
            model: self.effective_model(),
            mode: self.effective_mode(),
            // agy has no prompt pipeline of its own, so the composed rules block
            // rides in through `init.preset_context` and `effective_initial_prompt`
            // prepends it on the first invocation only.
            injected_prefix: self.config.init.preset_context.clone(),
            extra_args: self.config.extra_args.clone(),
        };
        let initial_prompt = effective_initial_prompt(&input);
        let mut spawn_env = self.config.spawn_env.clone();
        if let Some(cwd) = self.config.cwd.as_deref() {
            // agy 1.1.13 resolves its primary customization workspace from PWD,
            // not only the process cwd/--add-dir. A stale inherited PWD made it
            // scan the host app directory and skip this session's MCP plugin
            // (verified: ~/.gemini/antigravity-cli/log/cli-20260814_135824.log:48,51).
            spawn_env.retain(|entry| entry.name != "PWD");
            spawn_env.push(aionui_common::EnvVar {
                name: "PWD".to_owned(),
                value: cwd.to_owned(),
            });
        }
        let spec = CommandSpec {
            command: self
                .config
                .cli_program
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("agy")),
            args: build_argv(&input),
            env: spawn_env,
            cwd: self.config.cwd.clone(),
        };

        #[cfg(test)]
        let scripted_io = self.test_ios.lock().await.pop_front();
        #[cfg(not(test))]
        let scripted_io: Option<Arc<dyn AgentIo>> = None;

        let io: Arc<dyn AgentIo> = match scripted_io {
            Some(fake) => fake,
            None => {
                let proc = self
                    .spawner
                    .spawn(spec, &[], &self.session_id)
                    .await
                    .map_err(|e| BackendError::Transport(format!("spawn agy: {e}")))?;
                Arc::new(ManagedProcessIo::new(proc))
            }
        };

        let (mut stdin, stdout) = io
            .take_stdio()
            .await
            .ok_or_else(|| BackendError::Transport("spawn agy: stdio unavailable".into()))?;
        write_stream_user_frame(&mut stdin, &initial_prompt)
            .await
            .map_err(|e| BackendError::Transport(format!("write initial agy prompt: {e}")))?;

        *self.current.lock().await = Some(Arc::clone(&io));
        *self.turn_io.lock().await = ActiveTurnIo {
            stdin: Some(stdin),
            pending_steers: VecDeque::new(),
        };
        // Freeze whether THIS turn can hear about a later mode switch. A hooked turn
        // calls back per tool use, so the host gate applies a switch at once; a full-auto
        // turn never calls back and only the next spawn can pick one up.
        self.turn_started_hooked.store(!self.is_full_auto(), Ordering::SeqCst);
        self.in_flight.store(true, Ordering::SeqCst);
        self.spawn_reader(io, stdout, turn_gen);
        Ok(turn_gen)
    }

    /// Drain the process's stdout, translating each NDJSON line, folding any
    /// steered follow-up turns into the active `turn_gen`, and closing the turn
    /// out when the final result arrives or the process exits.
    fn spawn_reader(&self, io: Arc<dyn AgentIo>, stdout: BoxedStdout, turn_gen: u64) {
        let backend = self.weak_self.get().cloned();
        let event_tx = self.event_tx.clone();
        let session_id = self.session_id.clone();
        // The SESSION's anchor, not a fresh one: the id agy reports in `init`
        // is what the NEXT turn must pass through `--conversation`, so it has
        // to outlive this reader task.
        let anchor = Arc::clone(&self.anchor);
        let current = Arc::clone(&self.current);
        let turn_io = Arc::clone(&self.turn_io);

        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};

            let mut translator = Translator::default();
            let mut saw_terminal = false;

            // agy < 1.1.15 corrupted multi-byte characters when splitting
            // `text_delta` across byte boundaries. Its `result` frame carries
            // the same reply assembled intact, so assistant text is held back
            // per sub-turn and emitted once from the authoritative source.
            let mut buffered_text = String::new();
            let mut text_item_id = String::new();
            let mut text_flushed = false;

            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(ev) = parse_line(&line) else { continue };
                for out in translator.translate(ev) {
                    match out {
                        SessionEvent::MessageDelta { item_id, text } => {
                            if text_item_id.is_empty() {
                                text_item_id = item_id;
                            }
                            append_delta(&mut buffered_text, &text);
                        }
                        SessionEvent::TurnResult {
                            ref is_error,
                            ref result_text,
                            ..
                        } => {
                            // Atomically claim the next steered follow-up OR close
                            // stdin (`EOF`) so no late `Steer` can slip into a
                            // finished turn.
                            let steered_cmid = {
                                let mut guard = turn_io.lock().await;
                                if !*is_error {
                                    if let Some(cmid) = guard.pending_steers.pop_front() {
                                        Some(cmid)
                                    } else {
                                        guard.stdin = None;
                                        None
                                    }
                                } else {
                                    guard.stdin = None;
                                    guard.pending_steers.clear();
                                    None
                                }
                            };

                            if let Some(maybe_cmid) = steered_cmid {
                                // A steered user frame is already buffered on stdin:
                                // agy will immediately start the follow-up turn in
                                // this same process (verified live on agy 1.5.8).
                                // Flush this sub-turn's text, emit the receipt echo
                                // that flips the UI badge ("queued" -> "received"),
                                // and suppress the intermediate `TurnResult` so
                                // `StreamRelay` stays continuously open.
                                if let Some(text) = final_turn_text(false, result_text, &buffered_text) {
                                    let _ = event_tx.send(SessionEnvelope {
                                        session_id: session_id.clone(),
                                        turn_gen,
                                        event: SessionEvent::MessageDelta {
                                            item_id: std::mem::take(&mut text_item_id),
                                            text: ensure_turn_separator(text),
                                        },
                                    });
                                }
                                buffered_text.clear();
                                text_item_id.clear();
                                text_flushed = false;

                                if let Some(client_msg_id) = maybe_cmid {
                                    let _ = event_tx.send(SessionEnvelope {
                                        session_id: session_id.clone(),
                                        turn_gen,
                                        event: SessionEvent::MessageLifecycle {
                                            client_msg_id,
                                            phase: MessageLifecyclePhase::Completed,
                                        },
                                    });
                                }
                            } else {
                                saw_terminal = true;
                                if let Some(text) = final_turn_text(*is_error, result_text, &buffered_text) {
                                    text_flushed = true;
                                    let _ = event_tx.send(SessionEnvelope {
                                        session_id: session_id.clone(),
                                        turn_gen,
                                        event: SessionEvent::MessageDelta {
                                            item_id: std::mem::take(&mut text_item_id),
                                            text,
                                        },
                                    });
                                }
                                let _ = event_tx.send(SessionEnvelope {
                                    session_id: session_id.clone(),
                                    turn_gen,
                                    event: out,
                                });
                            }
                        }
                        other => {
                            let _ = event_tx.send(SessionEnvelope {
                                session_id: session_id.clone(),
                                turn_gen,
                                event: other,
                            });
                        }
                    }
                }
            }

            {
                let mut guard = turn_io.lock().await;
                guard.stdin = None;
                guard.pending_steers.clear();
            }

            // Cancelled or crashed: no `result` frame ever arrives, so the text
            // gathered so far is all there is. Losing the partial reply would be
            // worse than showing agy's own mojibake.
            if !text_flushed && !buffered_text.is_empty() {
                let _ = event_tx.send(SessionEnvelope {
                    session_id: session_id.clone(),
                    turn_gen,
                    event: SessionEvent::MessageDelta {
                        item_id: text_item_id,
                        text: std::mem::take(&mut buffered_text),
                    },
                });
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
                let exit = io.wait_for_exit().await;
                let ok = exit.is_some_and(|s| s.code == Some(0) && s.signal.is_none());
                let _ = event_tx.send(SessionEnvelope {
                    session_id: session_id.clone(),
                    turn_gen,
                    event: SessionEvent::TurnResult {
                        is_error: !ok,
                        api_error_status: None,
                        result_text: if ok { String::new() } else { io.peek_stderr(20).await },
                        epoch: 0,
                        outcome: if ok { TurnOutcome::EndTurn } else { TurnOutcome::Failed },
                    },
                });
            }
            // The turn owns no process any more; leaving a dead handle here
            // would make a later Cancel try to kill an exited pid.
            let mut slot = current.lock().await;
            if slot.as_ref().is_some_and(|p| Arc::ptr_eq(p, &io)) {
                *slot = None;
            }
            drop(slot);

            // The turn is over, so anything queued via direct `Command::Send`
            // while it ran can run now — as its own turn, resuming the same
            // agy conversation.
            let Some(backend) = backend.and_then(|w| w.upgrade()) else {
                // Session dropped; nothing left to run for.
                return;
            };
            backend.settle_turn_end().await;
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
            Command::SetMode { ref mode } => {
                // Recording this is what makes a mid-conversation switch mean
                // anything: the next turn's argv reads it, and so does the
                // full-auto check in `request_external_permission`. Leaving it
                // unrecorded made the UI's mode picker a control that reported
                // success and changed nothing.
                if let Ok(mut guard) = self.mode_override.write() {
                    *guard = Some(mode.clone());
                }
                // The hook FILE has to follow the switch too. Turning full auto
                // off is the dangerous direction: a session that started in it
                // has no hook installed, so agy never calls back and the
                // in-process check never runs — the user would think they had
                // restored approval prompts while everything still ran freely.
                self.sync_permission_hook();
                let turn_gen = self.turn_gen.load(Ordering::SeqCst);
                // Confirm the switch. Unlike the other backends this signal originates
                // here rather than on the wire — agy runs one process per turn and there
                // is nothing to reconfigure mid-flight — but it is not self-fulfilling:
                // the permission decision is re-read per tool call (`is_full_auto` in
                // `request_external_permission`) and the hook file has just been synced,
                // so the host-side gate really has changed.
                //
                // It is also the ONLY trigger that persists `current_mode_id` (see
                // session_agent's `persist_side_effects`); without it a task rebuild
                // reverted the user to the stale create-time mode.
                // Confirm ONLY when the switch really reaches the running turn.
                //
                // `ConfigChanged` is what clears the frontend's pending marker, so
                // emitting it for a deferred switch would flip the picker to "in force"
                // for precisely the case that has not taken effect — telling the user
                // their tightening had landed while the agent kept running unattended.
                // The deferred case confirms itself when the next turn spawns, which is
                // when agy actually reads the mode off argv.
                if !self.switch_reaches_current_turn() {
                    *self.deferred_mode_confirm.lock().await = Some(mode.clone());
                }
                if self.switch_reaches_current_turn() {
                    self.emit(
                        turn_gen,
                        SessionEvent::ConfigChanged {
                            mode: Some(mode.clone()),
                            model: None,
                        },
                    );
                }
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen,
                })
            }
            Command::SetModel { .. } => Ok(CommandReceipt {
                accepted: true,
                admission: Admission::Started,
                turn_gen: self.turn_gen.load(Ordering::SeqCst),
            }),
            Command::Steer { content, client_msg_id } => {
                let allowed = self.capabilities().prompt_blocks;
                if let Some(bad) = content.iter().find(|b| !allowed.allows(b)) {
                    return Err(BackendError::CommandNotSupported {
                        command: block_kind_name(bad),
                    });
                }
                let text = Self::prompt_text(&content);
                let mut guard = self.turn_io.lock().await;
                if !self.in_flight.load(Ordering::SeqCst) {
                    return Err(BackendError::Transport("no active turn to steer".into()));
                }
                let Some(stdin) = guard.stdin.as_mut() else {
                    return Err(BackendError::Transport("no active turn to steer".into()));
                };
                if let Err(e) = write_stream_user_frame(stdin, &text).await {
                    guard.stdin = None;
                    guard.pending_steers.clear();
                    return Err(BackendError::Transport(format!("no active turn to steer: {e}")));
                }
                guard.pending_steers.push_back(client_msg_id);
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
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
            slash_commands: self.slash_commands.clone(),
            // agy runs one process per turn and `--mode` is a spawn flag, so a turn
            // already running keeps the mode it was spawned with; the switch reaches agy
            // only when the next turn spawns.
            //
            // Reported as NextTurn even though the host-side gate does move at once (the
            // hook bridge re-reads `is_full_auto` per tool call). That is the safe
            // direction to be imprecise in: claiming "in force" while the tightening has
            // not reached agy would be the dangerous lie, whereas under-promising a
            // loosening costs the user nothing.
            mode_switch_effect: if self.switch_reaches_current_turn() {
                crate::capability::ModeSwitchEffect::Immediate
            } else {
                crate::capability::ModeSwitchEffect::NextTurn
            },
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
        {
            let mut guard = self.turn_io.lock().await;
            guard.stdin = None;
            guard.pending_steers.clear();
        }
        if let Some(io) = self.current.lock().await.take() {
            io.terminate().await;
        }
        // The process that would have consumed these answers is gone.
        self.deny_all_pending().await;
    }

    async fn request_external_permission(&self, tool_name: String, input: serde_json::Value) -> PermissionDecision {
        // Full auto answers itself. The hook file is only removed for sessions
        // that START in full auto; a session switched into it mid-conversation
        // still has the hook installed, and without this it would keep raising
        // cards the user explicitly asked not to see.
        if self.is_full_auto() {
            tracing::debug!(
                session_id = %self.session_id,
                tool = %tool_name,
                "antigravity: full-auto mode — approving without a prompt"
            );
            return PermissionDecision::Approved;
        }
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
                tool_name: Some(tool_name.clone()),
                input: Some(input),
            },
        );

        // agy does NOT wait indefinitely for its hook, and what it does on
        // expiry is the whole reason this deadline exists: measured against
        // 1.1.9, it abandons a PreToolUse hook after ~30s and then RUNS THE
        // TOOL ANYWAY. So an unanswered card is not a safe "still waiting" —
        // past that point the tool has already run while the card is still on
        // screen, and whatever the user clicks next changes nothing.
        //
        // Answering just before agy gives up turns its fail-open into
        // fail-closed: a user who is too slow gets a refused tool they can
        // retry, never one that silently executed behind a pending prompt.
        match tokio::time::timeout(PERMISSION_ANSWER_DEADLINE, rx).await {
            Ok(Ok(decision)) => decision,
            // Sender dropped without answering — treat as denial, never as
            // silent approval.
            Ok(Err(_)) => PermissionDecision::Denied,
            Err(_) => {
                // Retract the card too. Leaving it up would invite an answer
                // that no longer reaches agy, which reads as "my click did
                // nothing" — worse than the prompt disappearing.
                self.pending.lock().await.remove(&request_id);
                self.emit(
                    self.turn_gen.load(Ordering::SeqCst),
                    SessionEvent::PermissionResolved {
                        request_id: request_id.clone(),
                        kind: PermissionKind::Tool,
                    },
                );
                tracing::warn!(
                    session_id = %self.session_id,
                    request_id = %request_id,
                    tool = %tool_name,
                    "antigravity: denied a tool because agy was about to stop waiting for approval"
                );
                PermissionDecision::Denied
            }
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
        assert!(spec.args.windows(2).any(|w| w == ["--input-format", "stream-json"]));
        assert!(spec.args.windows(2).any(|w| w == ["-p", ""]));
        assert!(
            !spec.args.contains(&"hello".to_string()),
            "prompt text flows over stream-json stdin, not OS argv"
        );
        assert!(spec.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(spec.cwd.as_deref(), Some("/w"));
        assert_eq!(
            spec.env
                .iter()
                .filter(|entry| entry.name == "PWD")
                .map(|entry| entry.value.as_str())
                .collect::<Vec<_>>(),
            vec!["/w"]
        );
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
    async fn steer_without_active_turn_returns_no_active_turn_to_steer() {
        // `ConversationService::deliver_midturn_message` matches the `"no active
        // turn to steer"` substring (`steer_rejection_is_turn_ended`) to fall
        // back cleanly to starting a brand-new turn when an active turn ends
        // right as the user hits Send.
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
                client_msg_id: Some("cmsg-idle".into()),
            })
            .await
            .expect_err("steer without an active turn must fail with race-fallback error");
        match err {
            BackendError::Transport(msg) => assert!(
                msg.contains("no active turn to steer"),
                "expected 'no active turn to steer', got: {msg}"
            ),
            other => panic!("expected BackendError::Transport, got: {other:?}"),
        }
    }

    /// Concrete handle so the permission tests can reach the inherent methods
    /// without going through `Arc<dyn SessionBackend>`.
    async fn backend_for_permissions() -> Arc<AntigravitySessionBackend> {
        let (event_tx, _) = broadcast::channel(16);
        let b = Arc::new(AntigravitySessionBackend {
            session_id: "conv-1".into(),
            models: Arc::new(std::sync::RwLock::new(Vec::new())),
            slash_commands: Vec::new(),
            mode_override: Arc::new(std::sync::RwLock::new(None)),
            config: config("/w"),
            spawner: Arc::new(FakeSpawner::new()),
            event_tx,
            turn_gen: AtomicU64::new(1),
            anchor: Arc::new(Mutex::new(None)),
            current: Arc::new(Mutex::new(None)),
            turn_io: Arc::new(Mutex::new(ActiveTurnIo::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            turn_started_hooked: Arc::new(AtomicBool::new(false)),
            deferred_mode_confirm: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            test_ios: Arc::new(Mutex::new(VecDeque::new())),
        });
        let _ = b.weak_self.set(Arc::downgrade(&b));
        b
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
        // A stranded hook holds agy's tool call open until --print-timeout,
        // which the user experiences as an unexplained hang. That backstop is
        // now an hour, so answering here is what actually keeps it short.
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

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_card_is_denied_before_agy_stops_waiting() {
        // agy abandons its PreToolUse hook after ~30s and then runs the tool
        // regardless (measured on 1.1.9: 29.5 / 29.7 / 29.8 / 30.0s across four
        // runs with a hook that never answered). Waiting past that point would
        // leave a card on screen for a tool that already ran, so we answer
        // `deny` first and the refusal stays ours.
        let b = backend_for_permissions().await;
        let decision = b.request_external_permission("run_command".into(), json!({})).await;
        assert_eq!(decision, PermissionDecision::Denied);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_card_is_retracted_not_left_hanging() {
        // A card that outlives its answer window must disappear: clicking it
        // would change nothing, which reads as "my click did nothing".
        let b = backend_for_permissions().await;
        let _ = b.request_external_permission("run_command".into(), json!({})).await;
        assert!(
            b.pending.lock().await.is_empty(),
            "the pending entry must be dropped when the deadline passes"
        );
        assert!(
            b.pending_permission_requests().is_empty(),
            "the card must no longer be advertised to the UI"
        );
    }

    #[tokio::test]
    async fn an_answer_within_the_window_still_wins() {
        // The deadline must not steal a decision the user did make in time.
        let b = backend_for_permissions().await;
        let b2 = Arc::clone(&b);
        let answer = tokio::spawn(async move {
            for _ in 0..50 {
                let id = b2.pending_permission_requests().first().map(|p| p.request_id.clone());
                if let Some(id) = id {
                    let _ = b2
                        .dispatch(Command::AnswerPermission {
                            request_id: id,
                            decision: PermissionDecision::Approved,
                            selected: None,
                            answers: Vec::new(),
                        })
                        .await;
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        let decision = b.request_external_permission("run_command".into(), json!({})).await;
        let _ = answer.await;
        assert_eq!(decision, PermissionDecision::Approved);
    }

    /// A backend rooted at `cwd`, in `mode`, carrying a prepared hook body.
    fn backend_with_hook_body(
        cwd: &std::path::Path,
        mode: Option<&str>,
        hook_body: Option<&str>,
    ) -> Arc<AntigravitySessionBackend> {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut cfg = config(&cwd.to_string_lossy());
        cfg.mode = mode.map(str::to_owned);
        cfg.permission_hook_body = hook_body.map(str::to_owned);
        Arc::new(AntigravitySessionBackend {
            session_id: "s1".into(),
            slash_commands: Vec::new(),
            mode_override: Arc::new(std::sync::RwLock::new(None)),
            models: Arc::new(std::sync::RwLock::new(Vec::new())),
            config: cfg,
            spawner: Arc::new(crate::testing::FakeSpawner::new()),
            event_tx,
            turn_gen: AtomicU64::new(0),
            anchor: Arc::new(Mutex::new(None)),
            current: Arc::new(Mutex::new(None)),
            turn_io: Arc::new(Mutex::new(ActiveTurnIo::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            turn_started_hooked: Arc::new(AtomicBool::new(false)),
            deferred_mode_confirm: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            test_ios: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    #[tokio::test]
    async fn switching_to_full_auto_mid_conversation_stops_the_prompts() {
        // The hook file is only skipped for sessions that START in full auto, so
        // a session switched into it still has the hook installed. Without this
        // the user would keep seeing cards they explicitly turned off.
        let b = backend_for_permissions().await;
        b.dispatch(Command::SetMode {
            mode: "yolo".to_owned(),
        })
        .await
        .expect("mode switch accepted");

        let decision = b.request_external_permission("run_command".into(), json!({})).await;
        assert_eq!(decision, PermissionDecision::Approved);
        assert!(
            b.pending_permission_requests().is_empty(),
            "full auto must not raise a card at all"
        );
    }

    #[tokio::test]
    async fn switching_back_out_of_full_auto_reinstalls_the_hook_file() {
        // The FILE is what decides whether agy asks at all. A session that
        // started in full auto has none, so agy never calls back and the
        // in-process check never runs — asserting only that the check returns
        // Denied would pass while the session kept running unattended, which is
        // exactly how the original bug hid.
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join(".agents/hooks.json");
        let b = backend_with_hook_body(dir.path(), Some("yolo"), Some("{\"stub\":true}"));
        assert!(!hook.exists(), "a full-auto session starts without the hook");

        b.dispatch(Command::SetMode {
            mode: "default".to_owned(),
        })
        .await
        .unwrap();

        assert!(hook.exists(), "leaving full auto must restore the gate");
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), "{\"stub\":true}");
    }

    #[tokio::test]
    async fn switching_into_full_auto_removes_the_hook_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join(".agents/hooks.json");
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();
        std::fs::write(&hook, "{}").unwrap();
        let b = backend_with_hook_body(dir.path(), Some("default"), Some("{}"));

        b.dispatch(Command::SetMode {
            mode: "yolo".to_owned(),
        })
        .await
        .unwrap();

        assert!(!hook.exists(), "full auto must not leave a hook agy would call");
    }

    /// A session whose turn is running WITH the approval hook installed applies a mode
    /// switch at once, so it must not claim otherwise.
    ///
    /// The gate lives on the host: agy calls back per tool use and
    /// `request_external_permission` re-reads the mode every time. So while those
    /// callbacks are happening, a switch governs the very next tool — in both directions.
    #[tokio::test]
    async fn a_hooked_turn_applies_a_mode_switch_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("default"), Some("{\"stub\":true}"));
        b.in_flight.store(true, Ordering::SeqCst);
        b.turn_started_hooked.store(true, Ordering::SeqCst);

        assert_eq!(
            b.capabilities().mode_switch_effect,
            crate::capability::ModeSwitchEffect::Immediate,
            "a hooked turn keeps calling back, so the new mode governs the next tool call"
        );
    }

    /// The one case that genuinely has to wait: a turn that started in FULL AUTO has no
    /// hook on disk, so agy never calls back and the host gate never runs. Tightening
    /// cannot reach that already-spawned process; it lands when the next turn spawns.
    ///
    /// Claiming `Immediate` here would be the dangerous lie -- the user would be told the
    /// permission was restored while the agent kept running unattended.
    #[tokio::test]
    async fn a_full_auto_turn_defers_a_tightening_switch() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("yolo"), Some("{\"stub\":true}"));
        b.in_flight.store(true, Ordering::SeqCst);
        b.turn_started_hooked.store(false, Ordering::SeqCst);

        assert_eq!(
            b.capabilities().mode_switch_effect,
            crate::capability::ModeSwitchEffect::NextTurn,
            "no hook was installed for this turn, so agy cannot learn about the switch"
        );
    }

    /// Between turns there is no agy process at all, so the next spawn necessarily reads
    /// the new mode from argv.
    #[tokio::test]
    async fn an_idle_session_applies_a_mode_switch_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("yolo"), Some("{\"stub\":true}"));
        assert_eq!(
            b.capabilities().mode_switch_effect,
            crate::capability::ModeSwitchEffect::Immediate
        );
    }

    /// A deferred switch must NOT announce itself as applied.
    ///
    /// `ConfigChanged` is what clears the frontend's pending marker, so emitting it here
    /// would flip the picker to "in force" for exactly the case that has not taken effect
    /// -- the contradiction this pair of rules exists to prevent.
    #[tokio::test]
    async fn a_deferred_switch_emits_no_premature_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("yolo"), Some("{\"stub\":true}"));
        b.in_flight.store(true, Ordering::SeqCst);
        b.turn_started_hooked.store(false, Ordering::SeqCst);
        let mut events = b.events();

        b.dispatch(Command::SetMode {
            mode: "default".to_owned(),
        })
        .await
        .unwrap();

        let confirmed = tokio::time::timeout(std::time::Duration::from_millis(400), async {
            use futures_util::StreamExt as _;
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { mode, .. } = env.event {
                    return mode;
                }
            }
            None
        })
        .await;
        assert!(
            confirmed.is_err() || confirmed.as_ref().unwrap().is_none(),
            "a switch agy cannot see yet must not be confirmed, got {confirmed:?}"
        );
    }

    /// A deferred switch confirms as soon as the turn that could not hear it ENDS.
    ///
    /// Waiting for the next spawn was too conservative: once that turn is over its agy
    /// process is gone, so nothing is running under the old mode any more and the next
    /// spawn necessarily reads the new one. Holding "switching…" until the user happens
    /// to send another message left the picker stuck with no way to tell whether the
    /// switch had landed — and made a second switch appear to work instantly while the
    /// first still looked pending.
    #[tokio::test]
    async fn a_deferred_switch_confirms_when_the_turn_ends() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("yolo"), Some("{\"stub\":true}"));
        b.in_flight.store(true, Ordering::SeqCst);
        b.turn_started_hooked.store(false, Ordering::SeqCst);
        let mut events = b.events();

        b.dispatch(Command::SetMode {
            mode: "default".to_owned(),
        })
        .await
        .unwrap();

        b.settle_turn_end().await;

        let confirmed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            use futures_util::StreamExt as _;
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { mode, .. } = env.event {
                    return mode;
                }
            }
            None
        })
        .await
        .expect("the turn ending must release the held confirmation");
        assert_eq!(confirmed.as_deref(), Some("default"));
    }

    /// ...and only once: a second turn boundary must not re-announce it.
    #[tokio::test]
    async fn a_released_confirmation_is_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("yolo"), Some("{\"stub\":true}"));
        b.in_flight.store(true, Ordering::SeqCst);
        b.turn_started_hooked.store(false, Ordering::SeqCst);

        b.dispatch(Command::SetMode {
            mode: "default".to_owned(),
        })
        .await
        .unwrap();
        b.settle_turn_end().await;

        let mut events = b.events();
        b.settle_turn_end().await;
        let again = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            use futures_util::StreamExt as _;
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { .. } = env.event {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(
            again.is_err() || !again.unwrap(),
            "the held confirmation is released once, not on every turn boundary"
        );
    }

    /// A runtime mode switch must emit `ConfigChanged`.
    ///
    /// Two things ride on it, and both were broken by its absence:
    ///   - PERSISTENCE. `ConfigChanged` is the ONLY trigger that writes
    ///     `current_mode_id` (session_agent's `persist_side_effects`), so without it
    ///     the switch lived only in this backend instance's `mode_override` and a task
    ///     rebuild silently reverted the user to the stale create-time mode.
    ///   - CONFIRMATION. It is what tells the frontend the switch really landed,
    ///     instead of the self-fulfilling `Observed` the task layer synthesizes.
    ///
    /// Honest for agy specifically: the permission decision is re-read per tool call
    /// (`is_full_auto` in `request_external_permission`), so once the override is
    /// recorded and the hook file synced, the host-side gate HAS changed — regardless
    /// of the already-spawned process's `--mode` flag.
    #[tokio::test]
    async fn a_runtime_mode_switch_emits_config_changed() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend_with_hook_body(dir.path(), Some("default"), Some("{}"));
        let mut events = b.events();

        b.dispatch(Command::SetMode {
            mode: "yolo".to_owned(),
        })
        .await
        .unwrap();

        let confirmed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            use futures_util::StreamExt as _;
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { mode, .. } = env.event {
                    return mode;
                }
            }
            None
        })
        .await
        .expect("a mode switch must emit ConfigChanged within 2s");
        assert_eq!(
            confirmed.as_deref(),
            Some("yolo"),
            "the confirmation must carry the mode that is now in force"
        );
    }

    #[test]
    fn a_runtime_switch_reaches_the_next_turns_argv() {
        // agy spawns a fresh process per turn, so the mode only takes effect if
        // the override is what argv reads.
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut cfg = config("/tmp/ws");
        cfg.mode = Some("default".to_owned());
        let b = AntigravitySessionBackend {
            session_id: "s1".into(),
            slash_commands: Vec::new(),
            mode_override: Arc::new(std::sync::RwLock::new(None)),
            models: Arc::new(std::sync::RwLock::new(Vec::new())),
            config: cfg,
            spawner: Arc::new(crate::testing::FakeSpawner::new()),
            event_tx,
            turn_gen: AtomicU64::new(0),
            anchor: Arc::new(Mutex::new(None)),
            current: Arc::new(Mutex::new(None)),
            turn_io: Arc::new(Mutex::new(ActiveTurnIo::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            turn_started_hooked: Arc::new(AtomicBool::new(false)),
            deferred_mode_confirm: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            test_ios: Arc::new(Mutex::new(VecDeque::new())),
        };
        assert_eq!(b.effective_mode().as_deref(), Some("default"));
        *b.mode_override.write().unwrap() = Some("plan".to_owned());
        assert_eq!(b.effective_mode().as_deref(), Some("plan"));
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
            slash_commands: Vec::new(),
            mode_override: Arc::new(std::sync::RwLock::new(None)),
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
            turn_io: Arc::new(Mutex::new(ActiveTurnIo::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            turn_started_hooked: Arc::new(AtomicBool::new(false)),
            deferred_mode_confirm: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            test_ios: Arc::new(Mutex::new(VecDeque::new())),
        };

        let caps = backend.capabilities();
        assert_eq!(
            caps.available_models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gemini-3.1-pro-high"]
        );
        assert_eq!(caps.current_model.as_deref(), Some("gemini-3.1-pro-high"));
        assert_eq!(
            caps.available_modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["default", "accept-edits", "plan", "yolo"]
        );
        assert_eq!(caps.current_mode.as_deref(), Some("plan"));
    }

    #[test]
    fn modes_are_agys_three_plus_the_full_auto_sentinel() {
        // agy's own axis is default / accept-edits / plan. `yolo` is AionUi's
        // sentinel for "no approval prompts" — offered as a choice so a user can
        // pick full auto deliberately, and carried by teams / scheduled runs.
        let modes = antigravity_modes();
        assert_eq!(
            modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["default", "accept-edits", "plan", "yolo"]
        );
        assert_eq!(modes[1].name, "Accept Edits");

        // The two agy explains itself, in agy's own words, plus the sentinel:
        // a mode whose consequence is invisible must say what it does.
        for id in ["accept-edits", "plan", "yolo"] {
            let m = modes.iter().find(|m| m.id == id).unwrap();
            assert!(m.description.is_some(), "{id} needs a description");
        }
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
    fn capabilities_advertise_steering_and_proactive_input() {
        let c = antigravity_capabilities();
        assert!(c.supported_commands.steer);
        assert!(c.accepts_proactive_input);
    }

    #[test]
    fn capabilities_advertise_midturn_delivery() {
        assert!(antigravity_capabilities().supports_midturn_delivery);
        assert_eq!(
            antigravity_capabilities().supports_midturn_delivery,
            crate::capability::backend_supports_midturn_delivery("antigravity"),
        );
    }

    #[test]
    fn capabilities_reflect_the_stream_json_turn_burst_shape() {
        let c = antigravity_capabilities();
        assert!(
            c.supported_commands.steer,
            "stream-json stdin accepts mid-turn user frames"
        );
        assert!(c.supported_commands.answer_permission, "the hook bridge answers");
        assert!(c.supported_commands.set_mode && c.supported_commands.set_model);
        assert!(!c.supported_commands.rewind);
        assert!(c.prompt_blocks.text);
        assert!(!c.prompt_blocks.image, "`agy -p` stream-json takes text input only");
        assert_eq!(c.prompt_accepted, PromptAcceptedSource::Synthesized);
    }

    /// Build a backend with a known model list, bypassing the async probe.
    fn backend_with_models(models: &[&str], requested: Option<&str>) -> AntigravitySessionBackend {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut cfg = config("/tmp/ws");
        cfg.model = requested.map(str::to_owned);
        AntigravitySessionBackend {
            session_id: "s1".into(),
            slash_commands: Vec::new(),
            mode_override: Arc::new(std::sync::RwLock::new(None)),
            models: Arc::new(std::sync::RwLock::new(
                models
                    .iter()
                    .map(|id| crate::capability::ModelInfo {
                        id: (*id).to_owned(),
                        name: (*id).to_owned(),
                        description: None,
                        reasoning_efforts: Vec::new(),
                    })
                    .collect(),
            )),
            config: cfg,
            spawner: Arc::new(crate::testing::FakeSpawner::new()),
            event_tx,
            turn_gen: AtomicU64::new(0),
            anchor: Arc::new(Mutex::new(None)),
            current: Arc::new(Mutex::new(None)),
            turn_io: Arc::new(Mutex::new(ActiveTurnIo::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            permission_seq: AtomicU64::new(0),
            weak_self: std::sync::OnceLock::new(),
            in_flight: Arc::new(AtomicBool::new(false)),
            turn_started_hooked: Arc::new(AtomicBool::new(false)),
            deferred_mode_confirm: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            test_ios: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    #[test]
    fn a_model_agy_never_reported_is_dropped_rather_than_failing_the_turn() {
        // Before discovery lands the UI has no agy models to offer and seeds the
        // conversation with the user's last model from another agent (e.g.
        // `gemini-3.1-pro-preview`). agy rejects it with "invalid model
        // selection" and the whole turn fails; its own default is better.
        let b = backend_with_models(
            &["gemini-3.1-pro-low", "claude-sonnet-4-6"],
            Some("gemini-3.1-pro-preview"),
        );
        assert_eq!(b.effective_model(), None);
    }

    #[test]
    fn a_model_agy_reported_is_passed_through() {
        let b = backend_with_models(&["gemini-3.1-pro-low"], Some("gemini-3.1-pro-low"));
        assert_eq!(b.effective_model().as_deref(), Some("gemini-3.1-pro-low"));
    }

    #[test]
    fn an_empty_model_list_means_unknown_not_invalid() {
        // Discovery may not have finished (or agy is signed out). Dropping the
        // user's model here would silently ignore a perfectly good choice.
        let b = backend_with_models(&[], Some("gemini-3.1-pro-low"));
        assert_eq!(b.effective_model().as_deref(), Some("gemini-3.1-pro-low"));
    }

    #[test]
    fn an_empty_model_list_still_drops_the_ui_placeholder() {
        // AionUi's team-creation flow used to persist the literal placeholder
        // "default" as a member's model. agy never reports a model with that
        // id, so passing it through while discovery is empty fails every turn
        // with a non-retryable model-not-found; agy's own default is strictly
        // better. Real ids keep passing through (see the test above).
        let b = backend_with_models(&[], Some("default"));
        assert_eq!(b.effective_model(), None);
    }

    #[test]
    fn the_terminal_frame_wins_because_the_deltas_are_mangled() {
        // agy cuts `text_delta` on byte boundaries, so a multi-byte character
        // split across two deltas arrives as a pair of U+FFFD. Its `result`
        // frame carries the same reply assembled once, hence intact.
        let buffered = "\u{fffd}\u{fffd}建 Web 应用程序";
        let clean = "设计和构建 Web 应用程序";
        assert_eq!(final_turn_text(false, clean, buffered).as_deref(), Some(clean));
    }

    #[test]
    fn a_failed_turn_keeps_the_reply_it_managed_to_stream() {
        // On failure `result_text` is the error message, not the reply.
        let out = final_turn_text(true, "authentication failed", "partial reply");
        assert_eq!(out.as_deref(), Some("partial reply"));
    }

    #[test]
    fn a_terminal_frame_without_a_reply_falls_back_to_the_deltas() {
        assert_eq!(
            final_turn_text(false, "", "streamed only").as_deref(),
            Some("streamed only")
        );
    }

    #[test]
    fn a_turn_with_no_text_at_all_emits_nothing() {
        // A tool-only turn must not produce an empty assistant bubble.
        assert_eq!(final_turn_text(false, "", ""), None);
        assert_eq!(final_turn_text(true, "", ""), None);
    }

    #[test]
    fn a_split_character_collapses_to_one_replacement_at_the_join() {
        // agy 1.1.14 splits a 3-byte character into a 2-U+FFFD suffix plus a
        // 1-U+FFFD prefix, or the 1+2 mirror (samples/antigravity-cli/1.1.14/
        // print_stream-json_long_chinese_fffd.ndjson). One lost character must
        // render as ONE marker, not three.
        let mut buf = String::from("报告概述与背\u{fffd}\u{fffd}");
        append_delta(&mut buf, "\u{fffd}\n\n在现代");
        assert_eq!(buf, "报告概述与背\u{fffd}\n\n在现代");

        let mut buf = String::from("性能\u{fffd}");
        append_delta(&mut buf, "\u{fffd}\u{fffd}优化");
        assert_eq!(buf, "性能\u{fffd}优化");
    }

    #[test]
    fn a_replacement_on_only_one_side_of_the_join_is_kept() {
        // A marker without a counterpart across the join is not a split
        // character — it must survive untouched.
        let mut buf = String::from("结尾\u{fffd}");
        append_delta(&mut buf, "继续");
        assert_eq!(buf, "结尾\u{fffd}继续");

        let mut buf = String::from("结尾");
        append_delta(&mut buf, "\u{fffd}继续");
        assert_eq!(buf, "结尾\u{fffd}继续");
    }

    #[test]
    fn a_replacement_away_from_the_join_passes_through() {
        // Only join runs are collapsed; anything mid-delta is the model's own
        // text, and the capture shows agy never mangles mid-delta.
        let mut buf = String::from("前文");
        append_delta(&mut buf, "中间\u{fffd}\u{fffd}还在");
        assert_eq!(buf, "前文中间\u{fffd}\u{fffd}还在");
    }

    #[test]
    fn an_empty_probe_result_is_never_cached() {
        // "not signed in" must not be pinned as the answer for the rest of the
        // process — the next session has to be free to probe again.
        store_models(&[]);
        assert!(cached_models().is_none() || !cached_models().unwrap().is_empty());
    }

    #[test]
    fn a_discovered_list_is_shared_with_later_sessions() {
        // agy's models belong to the signed-in account, so re-probing per
        // session only delays the picker.
        store_models(&[ModelInfo {
            id: "gemini-3.1-pro-low".into(),
            name: "gemini-3.1-pro-low".into(),
            description: None,
            reasoning_efforts: Vec::new(),
        }]);
        let cached = cached_models().expect("a non-empty list must be cached");
        assert!(cached.iter().any(|m| m.id == "gemini-3.1-pro-low"));
    }

    #[tokio::test]
    async fn steer_rejects_unsupported_image_block_with_command_not_supported() {
        let b = backend_for_permissions().await;
        let err = b
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: vec![0, 1, 2, 3],
                }],
                client_msg_id: Some("cmsg-img".into()),
            })
            .await
            .expect_err("image block must be rejected before touching stdin");
        assert!(matches!(
            err,
            BackendError::CommandNotSupported {
                command: "content_block:image"
            }
        ));
    }

    /// Replays a real two-turn `agy 1.5.8 --input-format stream-json
    /// --output-format stream-json -p ""` wire capture:
    /// 1. Turn 1 starts (`init` + `tool ACTIVE` on `run_command`).
    /// 2. User sends a mid-turn `Command::Steer` while the tool is running.
    /// 3. Turn 1 finishes (`result` with `num_turns: 1`); the backend flushes
    ///    Turn 1's reply, emits `MessageLifecycle::Completed` for the steered
    ///    `client_msg_id`, and suppresses the intermediate `TurnResult`.
    /// 4. Turn 2 runs in the same warm process (`step_index: 6, 7`, `num_turns: 2`)
    ///    and emits the single authoritative `TurnResult` on the same `turn_gen`.
    #[tokio::test]
    async fn midturn_steer_writes_event_user_ndjson_emits_lifecycle_completed_and_folds_followup_turn() {
        use crate::event::ExitStatusLite;
        use crate::testing::FakeAgentIo;
        use futures_util::StreamExt as _;

        let prefix = concat!(
            r#"{"event":"init","conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","init":{"model":"gemini-3.8-flash-low","cwd":"/w","tools":["run_command"],"permission_mode":"always-proceed"}}"#,
            "\n",
            r#"{"event":"step_update","step_update":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","step_index":0,"state":"DONE","step_type":"user_input"}}"#,
            "\n",
            r#"{"event":"step_update","step_update":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"Start-Sleep -Seconds 6"}}}}"#,
            "\n",
        );
        let seg_turn1_end = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","step_index":2,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","output":"Command exited with code 0."}}}"#,
            "\n",
            r#"{"event":"step_update","step_update":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","step_index":5,"state":"DONE","step_type":"agent_response","text_delta":"APPLE\n"}}"#,
            "\n",
            r#"{"event":"result","result":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","status":"SUCCESS","response":"APPLE\n","duration_seconds":17.2,"num_turns":1,"usage":{"input_tokens":17245,"output_tokens":67,"thinking_tokens":0,"cache_read_tokens":8092,"total_tokens":17312}}}"#,
            "\n",
        );
        let seg_turn2_end = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","step_index":6,"state":"DONE","step_type":"user_input"}}"#,
            "\n",
            r#"{"event":"step_update","step_update":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","step_index":7,"state":"DONE","step_type":"agent_response","text_delta":"BANANA\n"}}"#,
            "\n",
            r#"{"event":"result","result":{"conversation_id":"68a53772-fdd8-458d-88d8-3d11a451b887","status":"SUCCESS","response":"BANANA\n","duration_seconds":19.6,"num_turns":2,"usage":{"input_tokens":21998,"output_tokens":69,"thinking_tokens":0,"cache_read_tokens":12131,"total_tokens":22067}}}"#,
            "\n",
        );

        let fake = FakeAgentIo::new(
            prefix.as_bytes().to_vec(),
            Some(ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        )
        .with_gated_segments(vec![
            seg_turn1_end.as_bytes().to_vec(),
            seg_turn2_end.as_bytes().to_vec(),
        ]);
        let captured_stdin = fake.captured_stdin();
        let release_next = fake.segment_releaser();
        fake.release_exit();

        let b = backend_for_permissions().await;
        b.turn_gen.store(0, Ordering::SeqCst);
        b.test_ios.lock().await.push_back(Arc::new(fake));
        let mut events = b.events();

        let send_receipt = b.dispatch(send("say APPLE after sleep")).await.expect("send starts");
        assert_eq!(send_receipt.admission, Admission::Started);
        assert_eq!(send_receipt.turn_gen, 1);

        // Wait until the active tool step arrives so we know Turn 1 is mid-flight.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::ToolCall { ref name, .. } if name == "run_command") {
                    break;
                }
            }
        })
        .await
        .expect("tool call must stream");

        // Inject mid-turn steer while the tool is running.
        let steer_receipt = b
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Text("reply BANANA instead".into())],
                client_msg_id: Some("cmsg-steer-1".into()),
            })
            .await
            .expect("steer accepted");
        assert!(steer_receipt.accepted);
        assert_eq!(steer_receipt.admission, Admission::NoTurn);
        assert_eq!(steer_receipt.turn_gen, 1);

        // Release Turn 1's completion: must flush Turn 1 text + MessageLifecycle::Completed,
        // WITHOUT emitting an early TurnResult.
        release_next();
        let mut saw_turn1_text = false;
        let mut saw_lifecycle = false;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                assert_eq!(env.turn_gen, 1);
                match env.event {
                    SessionEvent::MessageDelta { text, .. } if text.starts_with("APPLE\n") => {
                        assert_eq!(text, "APPLE\n\n", "intermediate reply must end with paragraph break");
                        saw_turn1_text = true;
                    }
                    SessionEvent::MessageLifecycle { client_msg_id, phase } => {
                        assert_eq!(client_msg_id, "cmsg-steer-1");
                        assert_eq!(phase, MessageLifecyclePhase::Completed);
                        saw_lifecycle = true;
                    }
                    SessionEvent::TurnResult { .. } => {
                        panic!("intermediate result frame must be suppressed while steered turn is pending");
                    }
                    _ => {}
                }
                if saw_turn1_text && saw_lifecycle {
                    break;
                }
            }
        })
        .await
        .expect("Turn 1 boundary events must arrive");

        // Release Turn 2's completion: must emit BANANA + the single terminal TurnResult.
        release_next();
        let mut saw_turn2_text = false;
        let mut terminal_count = 0usize;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                assert_eq!(env.turn_gen, 1);
                match env.event {
                    SessionEvent::MessageDelta { text, .. } if text == "BANANA\n" => {
                        saw_turn2_text = true;
                    }
                    SessionEvent::TurnResult {
                        is_error,
                        outcome,
                        result_text,
                        ..
                    } => {
                        assert!(!is_error);
                        assert_eq!(outcome, TurnOutcome::EndTurn);
                        assert_eq!(result_text, "BANANA\n");
                        terminal_count += 1;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("Turn 2 terminal result must arrive");
        assert!(saw_turn2_text);
        assert_eq!(terminal_count, 1);
        assert_eq!(
            b.anchor.lock().await.as_deref(),
            Some("68a53772-fdd8-458d-88d8-3d11a451b887")
        );

        // Wait briefly for the stdin drain task to capture both flushed NDJSON frames.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let raw_stdin = String::from_utf8(captured_stdin.lock().await.clone()).expect("utf8 stdin");
        let stdin_lines: Vec<&str> = raw_stdin.lines().collect();
        assert_eq!(stdin_lines.len(), 2, "expected 2 NDJSON user frames: {raw_stdin}");
        let f1: serde_json::Value = serde_json::from_str(stdin_lines[0]).unwrap();
        let f2: serde_json::Value = serde_json::from_str(stdin_lines[1]).unwrap();
        assert_eq!(
            f1,
            json!({"event": "user", "message": {"role": "user", "content": "say APPLE after sleep"}})
        );
        assert_eq!(
            f2,
            json!({"event": "user", "message": {"role": "user", "content": "reply BANANA instead"}})
        );
    }

    #[tokio::test]
    async fn race_after_final_turn_result_closes_stdin_and_rejects_late_steer_cleanly() {
        use crate::event::ExitStatusLite;
        use crate::testing::FakeAgentIo;
        use futures_util::StreamExt as _;

        let single_turn = concat!(
            r#"{"event":"init","conversation_id":"conv-race-1","init":{"model":"gemini-3.8-flash-low"}}"#,
            "\n",
            r#"{"event":"result","result":{"conversation_id":"conv-race-1","status":"SUCCESS","response":"done","num_turns":1}}"#,
            "\n",
        );
        let fake = FakeAgentIo::new(
            single_turn.as_bytes().to_vec(),
            Some(ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        );
        fake.release_exit();

        let b = backend_for_permissions().await;
        b.turn_gen.store(0, Ordering::SeqCst);
        b.test_ios.lock().await.push_back(Arc::new(fake));
        let mut events = b.events();

        b.dispatch(send("hi")).await.expect("send starts");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::TurnResult { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("turn result must arrive");

        assert!(
            b.turn_io.lock().await.stdin.is_none(),
            "final TurnResult must drop stdin immediately so EOF reaches agy"
        );
        let err = b
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Text("too late".into())],
                client_msg_id: Some("cmsg-late".into()),
            })
            .await
            .expect_err("late steer after TurnResult must fail cleanly");
        match err {
            BackendError::Transport(msg) => assert!(msg.contains("no active turn to steer"), "{msg}"),
            other => panic!("expected Transport('no active turn to steer'), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_intermediate_turn_drops_pending_steers_and_emits_error_terminal() {
        use crate::event::ExitStatusLite;
        use crate::testing::FakeAgentIo;
        use futures_util::StreamExt as _;

        let prefix = concat!(
            r#"{"event":"init","conversation_id":"conv-err-1","init":{"model":"gemini-3.8-flash-low"}}"#,
            "\n",
            r#"{"event":"step_update","step_update":{"conversation_id":"conv-err-1","step_index":1,"state":"ACTIVE","step_type":"tool","tool_name":"run_command"}}"#,
            "\n",
        );
        let err_tail = concat!(
            r#"{"event":"result","result":{"conversation_id":"conv-err-1","status":"ERROR","error":"quota exhausted","response":""}}"#,
            "\n",
        );
        let fake = FakeAgentIo::new(
            prefix.as_bytes().to_vec(),
            Some(ExitStatusLite {
                code: Some(3),
                signal: None,
            }),
        )
        .with_gated_tail(err_tail.as_bytes().to_vec());
        let release_tail = fake.stdout_releaser();
        fake.release_exit();

        let b = backend_for_permissions().await;
        b.turn_gen.store(0, Ordering::SeqCst);
        b.test_ios.lock().await.push_back(Arc::new(fake));
        let mut events = b.events();

        b.dispatch(send("start")).await.expect("send starts");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::ToolCall { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("tool call");

        b.dispatch(Command::Steer {
            content: vec![ContentBlock::Text("follow up".into())],
            client_msg_id: Some("cmsg-err".into()),
        })
        .await
        .expect("steer queued");

        release_tail();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::TurnResult {
                    is_error,
                    result_text,
                    outcome,
                    ..
                } = env.event
                {
                    return (is_error, result_text, outcome);
                }
            }
            unreachable!()
        })
        .await
        .expect("error TurnResult must not be swallowed by a pending steer");
        assert!(terminal.0);
        assert!(terminal.1.contains("quota exhausted"));
        assert_eq!(terminal.2, TurnOutcome::Failed);
        assert!(b.turn_io.lock().await.pending_steers.is_empty());
    }

    #[tokio::test]
    async fn cancel_clears_pending_steers_and_closes_stdin() {
        use crate::testing::CountingTerminateIo;

        let counting = CountingTerminateIo::new();
        let terminate_calls = counting.terminate_counter();

        let b = backend_for_permissions().await;
        b.turn_gen.store(0, Ordering::SeqCst);
        b.test_ios.lock().await.push_back(Arc::new(counting));

        b.dispatch(send("long running")).await.expect("send starts");
        b.dispatch(Command::Steer {
            content: vec![ContentBlock::Text("steered before cancel".into())],
            client_msg_id: Some("cmsg-cancel".into()),
        })
        .await
        .expect("steer accepted");
        assert_eq!(b.turn_io.lock().await.pending_steers.len(), 1);

        b.dispatch(Command::Cancel {
            target: CancelTarget::Turn,
        })
        .await
        .expect("cancel accepted");

        let guard = b.turn_io.lock().await;
        assert!(guard.stdin.is_none(), "cancel must close stdin");
        assert!(guard.pending_steers.is_empty(), "cancel must clear pending steers");
        assert_eq!(terminate_calls.load(Ordering::SeqCst), 1);
    }
}
