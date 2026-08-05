//! Inbound JSON-RPC dispatch for the monitor actor.
//!
//! Parses one inner frame, routes by `method`, and drives the runtime:
//! `initialize` handshakes; `fs/subscribe`/`fs/unsubscribe` go through the shard
//! (identity resolved via [`ProjectService::resolve_reference`]); the file
//! commands (`fs/mkdir|remove|rename`) resolve + realpath-guard, then
//! hit the provider directly. Responses/notifications go out via the actor's
//! push port. Errors map to protocol codes ([`wire`]) with `pe_id`/`relative_path`
//! context in `error.data`.
//!
//! [`ProjectService::resolve_reference`]: crate::ProjectService::resolve_reference

use std::path::Path;

use serde_json::{Value, json};

use crate::canonical;
use crate::runtime::{Budget, CancellationToken, Command, MatchMode, NameMatcher, ShardOutput, Subscriber};
use crate::types::{FileOp, ReferenceInput, ResolvedResource};

use super::actor::FsMonitorActor;
use super::search::{self, ActiveSearch, SearchRoot};
use super::wire::{
    self, InitializeParams, MkdirParams, RemoveParams, RenameParams, ResourceRef, SearchCancelParams, SearchParams,
    SubscribeParams, UnsubscribeParams,
};

impl FsMonitorActor {
    /// Decode one inbound frame and route it by method. Malformed frames get a
    /// JSON-RPC error; unknown methods get `method_not_found`.
    pub(super) async fn dispatch_frame(&mut self, session: &str, user_id: &str, frame: Value) {
        let parsed = serde_json::from_value::<wire::IncomingFrame>(frame);
        let Ok(incoming) = parsed else {
            // Malformed inbound frame — safely handled (client bug / protocol drift).
            tracing::warn!(session, "fs dispatch: malformed frame");
            self.push(
                session,
                wire::error(None, wire::CODE_INVALID_REQUEST, "invalid_request", Value::Null),
            );
            return;
        };
        let id = incoming.id;
        let params = incoming.params;
        // High-frequency per-frame trace: method + session only (dev diagnostics).
        tracing::debug!(session, method = %incoming.method, "fs dispatch");
        match incoming.method.as_str() {
            "initialize" => self.handle_initialize(session, id, params),
            "fs/subscribe" => self.handle_subscribe(session, user_id, id, params).await,
            "fs/unsubscribe" => self.handle_unsubscribe(session, user_id, params).await,
            "fs/mkdir" => self.handle_mkdir(session, user_id, id, params).await,
            "fs/remove" => self.handle_remove(session, user_id, id, params).await,
            "fs/rename" => self.handle_rename(session, user_id, id, params).await,
            "fs/search" => self.handle_search(session, user_id, id, params).await,
            "fs/searchCancel" => self.handle_search_cancel(session, params),
            other => {
                tracing::warn!(session, method = %other, "fs dispatch: unknown method");
                self.push(
                    session,
                    wire::error(id, wire::CODE_METHOD_NOT_FOUND, "method_not_found", Value::Null),
                )
            }
        }
    }

    // ── handshake ─────────────────────────────────────────────────────────

    fn handle_initialize(&self, session: &str, id: Option<Value>, params: Value) {
        match serde_json::from_value::<InitializeParams>(params) {
            // We speak exactly v1; a client offering >= 1 negotiates down to 1.
            Ok(p) if p.protocol_version >= wire::PROTOCOL_VERSION => {
                self.push(
                    session,
                    wire::success(id, json!({ "protocol_version": wire::PROTOCOL_VERSION })),
                );
            }
            Ok(_) => self.push(
                session,
                wire::error(
                    id,
                    wire::CODE_PROTOCOL_VERSION_UNSUPPORTED,
                    "protocol_version_unsupported",
                    Value::Null,
                ),
            ),
            Err(_) => self.push(session, invalid_params(id)),
        }
    }

    // ── subscribe / unsubscribe ─────────────────────────────────────────────

    async fn handle_subscribe(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let Ok(parsed) = serde_json::from_value::<SubscribeParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };

        let target_count = parsed.targets.len();

        // Phase 1: resolve + canonicalize every target before mutating the shard,
        // so a bad target fails the whole request atomically (no partial mount).
        let mut plan: Vec<(ResourceRef, String)> = Vec::new();
        for target in parsed.targets {
            let resolved = match self.resolve(user_id, &target, FileOp::Browse).await {
                Ok(r) => r,
                Err((code, message)) => {
                    tracing::warn!(session, code = message, pe_id = %target.pe_id, "fs subscribe rejected");
                    self.push(session, wire::error(id.clone(), code, message, ref_data(&target)));
                    return;
                }
            };
            // Fold to the identity the watcher/apply chain keys on (case-folding
            // platforms differ) — otherwise events would fail to attribute back.
            let canonical = match canonical::canonicalize(&resolved.resource_uri) {
                Ok(c) => c.as_str().to_owned(),
                Err(_) => {
                    tracing::warn!(session, code = "provider_unavailable", pe_id = %target.pe_id, "fs subscribe rejected");
                    self.push(
                        session,
                        wire::error(
                            id.clone(),
                            wire::CODE_PROVIDER_UNAVAILABLE,
                            "provider_unavailable",
                            ref_data(&target),
                        ),
                    );
                    return;
                }
            };
            plan.push((target, canonical));
        }

        // Phase 2: subscribe each; the subscribe reply carries every snapshot.
        let now = self.now();
        let mut snapshots: Vec<Value> = Vec::new();
        for (target, canonical) in plan {
            let sub = Subscriber {
                session: session.to_owned(),
                pe_id: target.pe_id.clone(),
                rel: target.relative_path.clone(),
            };
            match self.shard_handle(Command::Subscribe { sub, canonical, now }).await {
                Ok(outputs) => {
                    for output in outputs {
                        if let ShardOutput::Snapshot { snapshot, .. } = output {
                            snapshots.push(wire::snapshot_params(&snapshot, &target));
                        }
                    }
                }
                Err(err) => {
                    let (code, message) = wire::fs_error_to_rpc(&err);
                    tracing::warn!(session, code = message, pe_id = %target.pe_id, "fs subscribe rejected");
                    self.push(session, wire::error(id.clone(), code, message, ref_data(&target)));
                    return;
                }
            }
        }
        // Subscription registration succeeded — lifecycle boundary (low volume).
        tracing::info!(
            session,
            targets = target_count,
            snapshots = snapshots.len(),
            "fs subscribe"
        );
        self.push(session, wire::success(id, json!({ "snapshots": snapshots })));
    }

    /// `fs/unsubscribe` is a notification: best-effort, no reply. A target that
    /// no longer resolves is silently ignored (the live subscription, if any,
    /// self-heals on the next full re-declare).
    async fn handle_unsubscribe(&mut self, session: &str, user_id: &str, params: Value) {
        let Ok(parsed) = serde_json::from_value::<UnsubscribeParams>(params) else {
            return;
        };
        // Subscription de-registration — lifecycle boundary (low volume).
        tracing::info!(session, targets = parsed.targets.len(), "fs unsubscribe");
        let now = self.now();
        for target in parsed.targets {
            let Ok(resolved) = self.resolve(user_id, &target, FileOp::Browse).await else {
                continue;
            };
            let Ok(canonical) = canonical::canonicalize(&resolved.resource_uri) else {
                continue;
            };
            let sub = Subscriber {
                session: session.to_owned(),
                pe_id: target.pe_id,
                rel: target.relative_path,
            };
            let _ = self
                .shard_handle(Command::Unsubscribe {
                    sub,
                    canonical: canonical.as_str().to_owned(),
                    now,
                })
                .await;
        }
    }

    // ── file commands ───────────────────────────────────────────────────────

    async fn handle_mkdir(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<MkdirParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let resolved = match self.resolve_guarded(user_id, &p.dir, FileOp::Write).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.dir)));
                return;
            }
        };
        let outcome = self.runtime().provider().mkdir(&resolved.resource_uri).await;
        self.reply_unit(session, id, "mkdir", &p.dir, outcome);
    }

    async fn handle_remove(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<RemoveParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let resolved = match self.resolve_guarded(user_id, &p.target, FileOp::Remove).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.target)));
                return;
            }
        };
        let outcome = self
            .runtime()
            .provider()
            .remove(&resolved.resource_uri, p.recursive)
            .await;
        self.reply_unit(session, id, "remove", &p.target, outcome);
    }

    async fn handle_rename(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<RenameParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let from = match self.resolve_guarded(user_id, &p.from, FileOp::Rename).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.from)));
                return;
            }
        };
        let to = match self.resolve_guarded(user_id, &p.to, FileOp::Rename).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.to)));
                return;
            }
        };
        let outcome = self
            .runtime()
            .provider()
            .rename(&from.resource_uri, &to.resource_uri)
            .await;
        self.reply_unit(session, id, "rename", &p.from, outcome);
    }

    // ── filename search ───────────────────────────────────────────────────

    /// `fs/search` (request): resolve every root atomically, then hand off to a
    /// spawned coordinator that walks all roots concurrently and streams
    /// `fs/searchMatch` batches + a terminal response. Superseding a prior
    /// in-flight search on this connection is done inside `register_search`.
    async fn handle_search(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        // A search is a request: without an id there is no `search_id` to key
        // matches/terminal on, so a search-shaped notification is ignored.
        let Some(search_id) = id else {
            tracing::warn!(session, "fs/search missing id (not a request); ignoring");
            return;
        };
        let Ok(p) = serde_json::from_value::<SearchParams>(params) else {
            self.push(session, invalid_params(Some(search_id)));
            return;
        };

        // Atomic resolve: each root via resolve_reference(Browse); any failure →
        // whole request errors, no partial search started (mirrors subscribe).
        let mut roots: Vec<SearchRoot> = Vec::with_capacity(p.roots.len());
        for root in &p.roots {
            match self.resolve(user_id, root, FileOp::Browse).await {
                Ok(resolved) => roots.push(SearchRoot {
                    root_uri: resolved.resource_uri,
                    pe_id: root.pe_id.clone(),
                }),
                Err((code, message)) => {
                    tracing::warn!(session, code = message, pe_id = %root.pe_id, "fs search rejected");
                    self.push(session, wire::error(Some(search_id), code, message, ref_data(root)));
                    return;
                }
            }
        }

        let Some(provider) = self.search_provider() else {
            self.push(
                session,
                wire::error(
                    Some(search_id),
                    wire::CODE_PROVIDER_UNAVAILABLE,
                    "provider_unavailable",
                    Value::Null,
                ),
            );
            return;
        };

        let matcher = NameMatcher::new(&p.query, MatchMode::Substring);
        let budget = Budget::new(p.limit.unwrap_or(search::DEFAULT_SEARCH_LIMIT));
        let cancel = CancellationToken::new();
        // Supersede any prior in-flight search on this connection (cancels it).
        self.register_search(
            session,
            ActiveSearch {
                search_id: search_id.clone(),
                cancel: cancel.clone(),
            },
        );
        // Lifecycle boundary — low volume; root count only (no query/paths).
        tracing::info!(session, roots = roots.len(), "fs search start");

        // Spawn the coordinator so the walks never block the actor event loop —
        // it must stay responsive to fs/searchCancel and superseding searches.
        let push = self.push_handle();
        let done = self.search_done_handle();
        tokio::spawn(search::run_search(
            provider,
            push,
            search::SearchJob {
                session: session.to_owned(),
                search_id,
                roots,
                matcher,
                budget,
                cancel,
            },
            done,
        ));
    }

    /// `fs/searchCancel` (notification): cancel the in-flight search iff its
    /// `search_id` matches. Fire-and-forget; the coordinator then sends no
    /// terminal frame (the client discards the cancelled search's matches).
    fn handle_search_cancel(&mut self, session: &str, params: Value) {
        let Ok(p) = serde_json::from_value::<SearchCancelParams>(params) else {
            return;
        };
        let cancelled = self.cancel_search(session, &p.search_id);
        tracing::info!(session, cancelled, "fs search cancel");
    }

    // ── helpers ───────────────────────────────────────────────────────────

    /// Resolve a reference to identity + lexical containment, mapping the
    /// bind-domain error to a protocol `(code, message)`.
    async fn resolve(
        &self,
        user_id: &str,
        target: &ResourceRef,
        op: FileOp,
    ) -> Result<ResolvedResource, (i64, &'static str)> {
        let input = ReferenceInput {
            pe_id: target.pe_id.clone(),
            relative_path: target.relative_path.clone(),
            op,
        };
        self.project()
            .resolve_reference(user_id, input)
            .await
            .map_err(|e| wire::project_error_to_rpc(&e))
    }

    /// Resolve + realpath-guard: identity/lexical containment first, then the
    /// access-time symlink/alias escape check before any command IO.
    async fn resolve_guarded(
        &self,
        user_id: &str,
        target: &ResourceRef,
        op: FileOp,
    ) -> Result<ResolvedResource, (i64, &'static str)> {
        let resolved = self.resolve(user_id, target, op).await?;
        guard_realpath(&resolved)?;
        Ok(resolved)
    }

    /// Reply `{}` on success, or map a provider error to a protocol error.
    /// `op` is the command label used for structured logging (identifier only).
    fn reply_unit(
        &self,
        session: &str,
        id: Option<Value>,
        op: &'static str,
        target: &ResourceRef,
        outcome: Result<(), crate::runtime::FsError>,
    ) {
        match outcome {
            Ok(()) => {
                tracing::info!(session, op, pe_id = %target.pe_id, rel = %target.relative_path, "fs command ok");
                self.push(session, wire::success(id, json!({})));
            }
            Err(err) => {
                let (code, message) = wire::fs_error_to_rpc(&err);
                tracing::warn!(session, op, pe_id = %target.pe_id, rel = %target.relative_path, code = message, "fs command failed");
                self.push(session, wire::error(id, code, message, ref_data(target)));
            }
        }
    }
}

/// Build a JSON-RPC `invalid_params` error for request `id`.
fn invalid_params(id: Option<Value>) -> Value {
    wire::error(id, wire::CODE_INVALID_PARAMS, "invalid_params", Value::Null)
}

/// `error.data` context for a reference.
fn ref_data(target: &ResourceRef) -> Value {
    json!({ "pe_id": target.pe_id, "relative_path": target.relative_path })
}

/// Realpath containment: the access-time symlink/alias escape guard that stage 0
/// deferred. `resolve_reference` already did lexical containment; here the target
/// (or its deepest existing ancestor, for not-yet-created paths) is realpath'd
/// and required to stay within the folder root's realpath. Fails closed.
fn guard_realpath(resolved: &ResolvedResource) -> Result<(), (i64, &'static str)> {
    let Some(absolute) = resolved.absolute_path.as_ref() else {
        // No filesystem path (non-file scheme) → realpath containment N/A here.
        return Ok(());
    };
    if realpath_within(&resolved.root_resource_canonical, Path::new(absolute)) {
        Ok(())
    } else {
        Err((wire::CODE_RESOURCE_OUTSIDE_FOLDER, "resource_outside_folder"))
    }
}

/// Whether `target`'s deepest existing ancestor realpath is inside `root`'s
/// realpath. Walking to the deepest existing ancestor lets not-yet-created
/// targets (write/mkdir/rename-to) be validated by their parent while still
/// catching a symlinked parent that escapes the root.
fn realpath_within(root_uri: &str, target: &Path) -> bool {
    let Ok(root_path) = canonical::uri_to_path(root_uri) else {
        return false;
    };
    let Ok(root_real) = std::fs::canonicalize(&root_path) else {
        return false;
    };
    let mut probe = target;
    loop {
        if let Ok(real) = std::fs::canonicalize(probe) {
            return real.starts_with(&root_real);
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return false,
        }
    }
}
