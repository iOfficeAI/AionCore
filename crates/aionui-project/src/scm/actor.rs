//! The source-control actor: one event loop owning the runtime.
//!
//! Mirrors the explorer monitor's shape — a single task consumes inbound frames
//! and watch signals, and pushes outbound frames through a narrow port, so the
//! domain never depends on a concrete transport.
//!
//! It exists to serialize three otherwise-racing inputs: client requests, watch
//! signals, and action completions. Refresh and sequence allocation happen inside
//! [`ScmRuntime`]'s per-repository critical section; this loop's job is to decide
//! *when* to refresh and *who* to tell.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, interval};

use aionui_db::Role;

use crate::service::ProjectService;
use crate::types::{FileOp, ProjectError, ReferenceInput};

use super::debounce::{DirtyDebouncer, flush_interval};
use super::error::ScmError;
use super::provider::IScmProvider;
use super::runtime::ScmRuntime;
use super::types::{FileRef, RepoRef, ResolvedRoot};
use super::watch::ScmDirty;
use super::wire;

/// Identifies one connection. A string mirroring the transport's connection id,
/// so the domain does not depend on the realtime crate's type.
pub type ScmSessionId = String;

/// Narrow outbound port: deliver one inner JSON-RPC frame to a connection.
///
/// The composition layer implements this over the WS manager's unicast, wrapping
/// each frame in the `{ name: "scm", data }` envelope.
pub trait ScmWirePush: Send + Sync {
    fn push(&self, session: &str, frame: Value);
}

/// An inbound event from the transport layer.
#[derive(Debug, Clone)]
pub enum ScmInbound {
    /// One `scm` frame's payload (the inner JSON-RPC value).
    Frame {
        session: ScmSessionId,
        user_id: String,
        frame: Value,
    },
    /// The connection closed — release its subscriptions and watches.
    Disconnect { session: ScmSessionId },
    /// A project's attached-folder set changed (a folder was attached or
    /// detached). Recompute its repositories and tell interested sessions what was
    /// added or removed. `user_id` authorizes the root resolution and is internal
    /// only — it never reaches the wire.
    RootsChanged { project_id: String, user_id: String },
}

/// Source-control protocol actor.
pub struct ScmActor {
    project: Arc<ProjectService>,
    runtime: Arc<ScmRuntime>,
    push: Arc<dyn ScmWirePush>,
    debouncer: DirtyDebouncer,
    started: Instant,
    /// Watch signals, taken by `run`. Held here so `new` does not expose the
    /// internal signal type in its signature.
    dirty_rx: Option<UnboundedReceiver<ScmDirty>>,
}

impl ScmActor {
    /// Build the actor.
    ///
    /// The watch-signal channel stays internal: it is an implementation detail of
    /// how dirt reaches the loop, and the composition layer has no use for it.
    pub fn new(project: Arc<ProjectService>, push: Arc<dyn ScmWirePush>) -> Result<Self, ScmError> {
        let (runtime, dirty_rx) = ScmRuntime::new()?;
        Ok(Self {
            project,
            runtime: Arc::new(runtime),
            push,
            debouncer: DirtyDebouncer::default(),
            started: Instant::now(),
            dirty_rx: Some(dirty_rx),
        })
    }

    /// Run the loop until every inbound sender is dropped.
    pub async fn run(mut self, mut inbound: UnboundedReceiver<ScmInbound>) {
        let mut dirty_rx = self.dirty_rx.take().expect("scm dirty receiver taken once in run");
        let mut flush = interval(flush_interval());
        loop {
            tokio::select! {
                event = inbound.recv() => match event {
                    Some(event) => self.on_inbound(event).await,
                    // All senders gone (router dropped, app shutting down).
                    None => break,
                },
                // The sender lives inside the runtime's watcher, so this stays
                // open for the actor's whole life.
                dirty = dirty_rx.recv() => if let Some(dirty) = dirty {
                    self.debouncer.mark(dirty.repo_id, self.now_ms());
                },
                _ = flush.tick() => self.flush_dirty().await,
            }
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Recompute the repositories whose signal bursts have gone quiet and push
    /// the new frames to their subscribers.
    async fn flush_dirty(&mut self) {
        if self.debouncer.is_idle() {
            return;
        }
        for repo_id in self.debouncer.take_ready(self.now_ms()) {
            let repo = RepoRef {
                repo_id: repo_id.clone(),
            };
            // Recompute once, then fan out the same frame: computing per
            // subscriber would waste work and could hand out differing statuses.
            match self.runtime.refresh(&repo).await {
                Ok(status) => {
                    let frame = wire::notification(
                        "scm/statusChanged",
                        serde_json::to_value(&status).unwrap_or_else(|_| json!({})),
                    );
                    for session in self.runtime.subscribers_of(&repo_id).await {
                        self.push.push(&session, frame.clone());
                    }
                }
                // A repository released between the signal and the flush is
                // ordinary; nothing to tell anyone.
                Err(err) => tracing::debug!(repo_id, error = %err, "scm refresh after dirty signal failed"),
            }
        }
    }

    async fn on_inbound(&mut self, event: ScmInbound) {
        match event {
            ScmInbound::Frame {
                session,
                user_id,
                frame,
            } => self.on_frame(&session, &user_id, frame).await,
            ScmInbound::Disconnect { session } => self.runtime.drop_session(&session).await,
            ScmInbound::RootsChanged { project_id, user_id } => self.on_roots_changed(&project_id, &user_id).await,
        }
    }

    async fn on_frame(&mut self, session: &str, user_id: &str, frame: Value) {
        let Ok(incoming) = serde_json::from_value::<wire::IncomingFrame>(frame) else {
            self.push.push(
                session,
                wire::error(None, wire::CODE_INVALID_REQUEST, "invalid_request", Value::Null),
            );
            return;
        };
        let id = incoming.id.clone();
        let params = incoming.params.clone();

        let outcome = match incoming.method.as_str() {
            "scm/listRepositories" => self.list_repositories(session, user_id, params).await,
            "scm/subscribe" => self.subscribe(session, user_id, params).await,
            "scm/unsubscribe" => {
                self.unsubscribe(session, params).await;
                return; // notification: no reply
            }
            "scm/status" => self.status(user_id, params).await,
            "scm/diff" => self.diff(user_id, params).await,
            "scm/original" => self.original(user_id, params).await,
            "scm/stage" => self.stage(user_id, params, true).await,
            "scm/unstage" => self.stage(user_id, params, false).await,
            "scm/discard" => self.discard(user_id, params).await,
            other => {
                tracing::warn!(session, method = %other, "scm dispatch: unknown method");
                self.push.push(
                    session,
                    wire::error(id, wire::CODE_METHOD_NOT_FOUND, "method_not_found", Value::Null),
                );
                return;
            }
        };

        match outcome {
            Ok(result) => self.push.push(session, wire::success(id, result)),
            Err(err) => self.push.push(session, wire::error_from(id, &err)),
        }
    }

    // ── methods ───────────────────────────────────────────────────────────

    async fn list_repositories(&self, session: &str, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let project_id = params
            .get("project_id")
            .and_then(Value::as_str)
            .ok_or(ScmError::InvalidParams { what: "project_id" })?;

        // Listing a project's repositories is also how a connection registers its
        // interest in them: from now until it disconnects it receives that
        // project's `repositoriesChanged` frames. Registering before discovery
        // closes the window where an attach lands between the two.
        self.runtime.register_interest(session, project_id).await;
        let roots = self.roots_of(user_id, project_id).await?;
        let repositories = self.runtime.discover(project_id, &roots).await;
        Ok(json!({ "repositories": repositories }))
    }

    /// Recompute a project's repositories after its attached folders changed, and
    /// push the added/removed delta to every session interested in the project.
    ///
    /// Wired directly (乙), not through an event bus (甲): source control is the
    /// only subscriber to project-explorer root changes, so a dedicated inbound
    /// message is cheaper and clearer than a general bus. Revisit if a second
    /// subscriber ever appears.
    async fn on_roots_changed(&self, project_id: &str, user_id: &str) {
        let roots = match self.roots_of(user_id, project_id).await {
            Ok(roots) => roots,
            // The project was deleted, or the user can no longer reach it: there is
            // nothing to recompute and nobody to tell.
            Err(err) => {
                tracing::debug!(project_id, error = %err, "scm: roots recompute skipped");
                return;
            }
        };
        let (added, removed) = self.runtime.recompute_project(project_id, &roots).await;
        // An empty delta is not a change — do not wake clients for nothing.
        if added.is_empty() && removed.is_empty() {
            return;
        }
        let frame = wire::repositories_changed(project_id, &added, &removed);
        for session in self.runtime.project_subscribers_of(project_id).await {
            self.push.push(&session, frame.clone());
        }
    }

    async fn subscribe(&self, session: &str, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let repo_ids = params
            .get("repositories")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Per repository, like the multi-file actions: subscribing to several
        // repositories in one request is the same shape of problem, and failing the
        // whole call would leave the two sides disagreeing about what is
        // subscribed — the server has already armed a watch and recorded the
        // subscriber for the ones that succeeded, while the client, seeing only an
        // error, never unsubscribes them and then receives pushes it does not
        // expect.
        let mut statuses = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        for repo_id in repo_ids {
            let repo = RepoRef {
                repo_id: repo_id.clone(),
            };
            let outcome = match self.authorize(user_id, &repo, FileOp::Browse).await {
                Ok(()) => self.runtime.subscribe(session, &repo).await,
                Err(err) => Err(err),
            };
            match outcome {
                Ok(status) => statuses.push(status),
                Err(err) => failed.push(json!({ "repo_id": repo_id, "reason": err.to_string() })),
            }
        }
        // Absent when everything succeeded, so a client written before per-item
        // reporting sees exactly the frame it saw before.
        if failed.is_empty() {
            Ok(json!({ "statuses": statuses }))
        } else {
            Ok(json!({ "statuses": statuses, "failed": failed }))
        }
    }

    async fn unsubscribe(&self, session: &str, params: Value) {
        let repo_ids = params
            .get("repositories")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for repo_id in repo_ids {
            self.runtime.unsubscribe(session, &RepoRef { repo_id }).await;
        }
    }

    async fn status(&self, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let repo = Self::repo_of(&params)?;
        self.authorize(user_id, &repo, FileOp::Browse).await?;
        let status = self.runtime.refresh(&repo).await?;
        Ok(serde_json::to_value(&status).unwrap_or_else(|_| json!({})))
    }

    async fn diff(&self, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let repo = Self::repo_of(&params)?;
        let file = Self::file_of(&params)?;
        let from = wire::parse_content_ref(params.get("from").unwrap_or(&Value::Null))
            .ok_or(ScmError::InvalidParams { what: "from" })?;
        let to = wire::parse_content_ref(params.get("to").unwrap_or(&Value::Null))
            .ok_or(ScmError::InvalidParams { what: "to" })?;

        let file = self.authorize_file(user_id, &file, FileOp::Read).await?;
        let diff = self.runtime.provider().diff(&repo, &file, from, to).await?;
        Ok(serde_json::to_value(&diff).unwrap_or_else(|_| json!({})))
    }

    async fn original(&self, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let repo = Self::repo_of(&params)?;
        let file = Self::file_of(&params)?;
        let at = wire::parse_content_ref(params.get("at").unwrap_or(&Value::Null))
            .ok_or(ScmError::InvalidParams { what: "at" })?;

        let file = self.authorize_file(user_id, &file, FileOp::Read).await?;
        let content = self.runtime.provider().original(&repo, &file, at).await?;
        Ok(match content {
            // Text when it decodes, base64 otherwise: the wire carries either
            // form, and guessing wrong would corrupt content.
            Some(bytes) => match String::from_utf8(bytes.clone()) {
                Ok(text) => json!({ "content": text, "encoding": "utf-8" }),
                Err(_) => json!({
                    "content": base64_encode(&bytes),
                    "encoding": "base64",
                }),
            },
            None => json!({}),
        })
    }

    async fn stage(&self, user_id: &str, params: Value, stage: bool) -> Result<Value, ScmError> {
        let repo = Self::repo_of(&params)?;
        let requested = Self::files_of(&params)?;
        // Collected per file: every entry must be replaced by its normalized form,
        // not just the first one.
        let mut files = Vec::with_capacity(requested.len());
        for file in &requested {
            files.push(self.authorize_file(user_id, file, FileOp::Write).await?);
        }

        let provider = self.runtime.provider();
        let staging = provider
            .staging()
            .ok_or(ScmError::CapabilityUnsupported { capability: "staging" })?;

        // Inside the repository's critical section, so the published status
        // cannot describe a half-applied action.
        let (outcome, status) = self
            .runtime
            .act(&repo, || async {
                if stage {
                    staging.stage(&repo, &files).await
                } else {
                    staging.unstage(&repo, &files).await
                }
            })
            .await?;
        self.broadcast(&status).await;
        Ok(serde_json::to_value(&outcome).unwrap_or_else(|_| json!({})))
    }

    async fn discard(&self, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let repo = Self::repo_of(&params)?;
        let requested = Self::files_of(&params)?;
        let mut files = Vec::with_capacity(requested.len());
        for file in &requested {
            files.push(self.authorize_file(user_id, file, FileOp::Remove).await?);
        }

        let provider = self.runtime.provider();
        let (outcome, status) = self
            .runtime
            .act(&repo, || async { provider.revert(&repo, &files).await })
            .await?;
        self.broadcast(&status).await;
        // Successful files are not listed; an absent `failed` means all of them.
        Ok(serde_json::to_value(&outcome).unwrap_or_else(|_| json!({})))
    }

    /// Push a freshly computed status to everyone subscribed to its repository.
    async fn broadcast(&self, status: &super::types::ScmStatus) {
        let frame = wire::notification(
            "scm/statusChanged",
            serde_json::to_value(status).unwrap_or_else(|_| json!({})),
        );
        for session in self.runtime.subscribers_of(&status.repository.repo_id).await {
            self.push.push(&session, frame.clone());
        }
    }

    // ── guards & params ───────────────────────────────────────────────────

    /// Check the repository's root belongs to this user, via the same
    /// identity/containment authority the explorer link uses.
    async fn authorize(&self, user_id: &str, repo: &RepoRef, op: FileOp) -> Result<(), ScmError> {
        let pe_id = self.runtime.pe_id_of_public(repo).await?;
        self.authorize_file(
            user_id,
            &FileRef {
                pe_id,
                relative_path: String::new(),
            },
            op,
        )
        .await
        .map(|_| ())
    }

    /// Check the reference is in scope **and return it normalized**.
    ///
    /// The returned path is what every downstream call must use. Authorizing one
    /// spelling and then operating on another is not merely untidy: several
    /// lexically equivalent spellings of the same file (`dir/./f`, `dir/sub/../f`,
    /// `./dir/f`) all pass containment, yet the engine treats them differently —
    /// some silently fail to match an index entry, and a leading `./` makes it
    /// reject the path outright. Normalizing once here, at the layer that owns
    /// identity resolution, keeps the engine from ever seeing those forms, and
    /// keeps "the path we checked" and "the path we used" the same value.
    async fn authorize_file(&self, user_id: &str, file: &FileRef, op: FileOp) -> Result<FileRef, ScmError> {
        self.project
            .resolve_reference(
                user_id,
                ReferenceInput {
                    pe_id: file.pe_id.clone(),
                    relative_path: file.relative_path.clone(),
                    op,
                },
            )
            .await
            .map(|resolved| FileRef {
                pe_id: file.pe_id.clone(),
                relative_path: resolved.relative_path,
            })
            .map_err(|err| match err {
                // A pe the user cannot reach, or a path escaping its root: both
                // are scope violations, not engine failures.
                ProjectError::ProjectExplorerNotFound { pe_id } => ScmError::OutOfScope { pe_id },
                ProjectError::ResourceOutsideFolder { .. } | ProjectError::InvalidRelativePath { .. } => {
                    ScmError::OutOfScope {
                        pe_id: file.pe_id.clone(),
                    }
                }
                other => ScmError::OperationFailed {
                    context: "authorize",
                    message: other.to_string(),
                },
            })
    }

    /// Resolve a project's roots into the form discovery consumes.
    async fn roots_of(&self, user_id: &str, project_id: &str) -> Result<Vec<ResolvedRoot>, ScmError> {
        let view = self
            .project
            .get_project(user_id, project_id)
            .await
            .map_err(|err| ScmError::OperationFailed {
                context: "list_repositories",
                message: err.to_string(),
            })?;

        let mut roots = Vec::new();
        for entry in view.explorer.entries {
            let resolved = self
                .project
                .resolve_reference(
                    user_id,
                    ReferenceInput {
                        pe_id: entry.pe_id.clone(),
                        relative_path: String::new(),
                        op: FileOp::Browse,
                    },
                )
                .await;
            match resolved {
                Ok(resolved) => {
                    // Only local paths can host a repository; a root without one
                    // is simply not a candidate.
                    if let Some(absolute_path) = resolved.absolute_path {
                        // The entry's own name, if it has a non-blank one. A blank
                        // (empty or whitespace-only) name counts as no name at all:
                        // it must survive as neither `label` nor `pe_name`, because
                        // the client's `pe_name || label` would otherwise render
                        // whitespace (a blank string is truthy in JS).
                        let named = entry.display_name.clone().filter(|name| !name.trim().is_empty());
                        // Same precedence the rest of the project surface uses: an
                        // explicit name wins, else the folder's derived one, else the
                        // opaque id. Filtering blanks above makes "label is never
                        // blank" a backend contract the client can lean on.
                        let label = named
                            .clone()
                            .or_else(|| entry.folder.default_display_name.clone())
                            .unwrap_or_else(|| entry.pe_id.clone());
                        // Carried raw and separate from the fallback chain; `None`
                        // means the entry has no name of its own (never `Some("")`).
                        let pe_name = named;
                        // Only a workspace pe root relaxes discovery by one level;
                        // an attached pe root stays one-repo-or-none. The role is a
                        // plain string on the explorer entry; compare against the
                        // canonical `Role` spelling rather than a literal.
                        let discover_children = entry.role == Role::Workspace.as_str();
                        roots.push(ResolvedRoot {
                            pe_id: entry.pe_id,
                            absolute_path,
                            label,
                            pe_name,
                            discover_children,
                        });
                    }
                }
                Err(err) => tracing::warn!(pe_id = %entry.pe_id, error = %err, "scm: root resolve failed"),
            }
        }
        Ok(roots)
    }

    fn repo_of(params: &Value) -> Result<RepoRef, ScmError> {
        params
            .get("repository")
            .and_then(Value::as_str)
            .map(|repo_id| RepoRef {
                repo_id: repo_id.to_owned(),
            })
            .ok_or(ScmError::InvalidParams { what: "repository" })
    }

    fn file_of(params: &Value) -> Result<FileRef, ScmError> {
        let file = params.get("file").ok_or(ScmError::InvalidParams { what: "file" })?;
        serde_json::from_value(file.clone()).map_err(|_| ScmError::InvalidParams { what: "file" })
    }

    fn files_of(params: &Value) -> Result<Vec<FileRef>, ScmError> {
        let files = params.get("files").ok_or(ScmError::InvalidParams { what: "files" })?;
        serde_json::from_value(files.clone()).map_err(|_| ScmError::InvalidParams { what: "files" })
    }
}

/// Minimal base64 for binary blob transport (the wire's `base64` encoding).
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
#[path = "actor_test.rs"]
mod actor_test;
