//! Inbound JSON-RPC dispatch for the monitor actor.
//!
//! Parses one inner frame, routes by `method`, and drives the runtime:
//! `initialize` handshakes; `fs/subscribe`/`fs/remount`/`fs/unsubscribe` go
//! through the shard (identity resolved via [`ProjectService::resolve_reference`]);
//! the file
//! commands (`fs/mkdir|createFile|remove|rename`) resolve + realpath-guard, then
//! hit the provider directly. Responses/notifications go out via the actor's
//! push port. Errors map to protocol codes ([`wire`]) with `pe_id`/`relative_path`
//! context in `error.data`.
//!
//! [`ProjectService::resolve_reference`]: crate::ProjectService::resolve_reference

use std::path::Path;

use serde_json::{Value, json};

use crate::canonical;
use crate::runtime::{Budget, CancellationToken, Command, Kind, MatchMode, NameMatcher, ShardOutput, Subscriber};
use crate::types::{FileOp, ReferenceInput, ResolvedResource};

use super::actor::FsMonitorActor;
use super::search::{self, ActiveSearch, SearchRoot};
use super::wire::{
    self, CreateFileParams, InitializeParams, MkdirParams, RemountParams, RemoveParams, RenameParams, ResourceRef,
    SearchCancelParams, SearchParams, SubscribeParams, TransferParams, UnsubscribeParams,
};

/// Whether a transfer keeps the source (`Copy`) or removes it after it lands
/// (`Move`). Both share [`handle_transfer`]; this only branches source op intent
/// and the post-landing cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Copy,
    Move,
}

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
            "fs/remount" => self.handle_remount(session, user_id, id, params).await,
            "fs/unsubscribe" => self.handle_unsubscribe(session, user_id, params).await,
            "fs/mkdir" => self.handle_mkdir(session, user_id, id, params).await,
            "fs/createFile" => self.handle_create_file(session, user_id, id, params).await,
            "fs/remove" => self.handle_remove(session, user_id, id, params).await,
            "fs/rename" => self.handle_rename(session, user_id, id, params).await,
            "fs/copy" => self.handle_transfer(session, user_id, id, params, Transfer::Copy).await,
            "fs/move" => self.handle_transfer(session, user_id, id, params, Transfer::Move).await,
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
        // Unlike phase 1 this is NOT atomic, on purpose: mounting arms an OS
        // watch, so one target can fail for reasons that say nothing about the
        // others (watch-descriptor limit exhausted on a large tree, a directory
        // removed between resolve and mount). Failing the whole batch there left
        // the client with an entirely blank explorer (AIONUI-236). Skip the
        // failed target, keep every snapshot that did mount, and fall back to the
        // error reply only when nothing mounted at all.
        let now = self.now();
        let mut snapshots: Vec<Value> = Vec::new();
        let mut failed: usize = 0;
        let mut first_failure: Option<(i64, &'static str, Value)> = None;
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
                    failed += 1;
                    let (code, message) = wire::fs_error_to_rpc(&err);
                    // Degraded target — safely handled, but an operator needs the
                    // cause: `code` is the stable protocol name the wire carries,
                    // `reason` the provider/watcher detail that name hides.
                    tracing::warn!(
                        session,
                        code = message,
                        pe_id = %target.pe_id,
                        rel = %target.relative_path,
                        reason = wire::fs_error_detail(&err),
                        "fs subscribe: target mount failed"
                    );
                    first_failure.get_or_insert_with(|| (code, message, ref_data(&target)));
                }
            }
        }
        // Nothing mounted → keep the request-level error (the first failure, as
        // before) instead of replying with a silently empty batch.
        if snapshots.is_empty()
            && let Some((code, message, data)) = first_failure
        {
            self.push(session, wire::error(id, code, message, data));
            return;
        }
        // Subscription registration succeeded — lifecycle boundary (low volume).
        // `failed` > 0 marks a degraded reply: some targets are not watched.
        tracing::info!(
            session,
            targets = target_count,
            snapshots = snapshots.len(),
            failed,
            "fs subscribe"
        );
        self.push(session, wire::success(id, json!({ "snapshots": snapshots })));
    }

    /// `fs/remount` (request): force a fresh mount of directories the client is
    /// already watching, recovering from a stale backend mount (a dead watch, a
    /// path that was removed then recreated). Unlike `fs/subscribe` this registers
    /// no subscription — it re-arms the watch and re-reads the baseline of nodes
    /// that are already watched, then replies with the fresh snapshots (same
    /// `{ snapshots }` shape as subscribe, so the client applies them identically).
    /// A target that is not currently watched is a silent no-op (it contributes no
    /// snapshot), so a remount of a collapsed root simply returns an empty batch.
    async fn handle_remount(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let Ok(parsed) = serde_json::from_value::<RemountParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };

        let target_count = parsed.targets.len();

        // Phase 1: resolve + canonicalize every target before mutating the shard,
        // so a bad target fails the whole request atomically (mirrors subscribe).
        let mut plan: Vec<(ResourceRef, String)> = Vec::new();
        for target in parsed.targets {
            let resolved = match self.resolve(user_id, &target, FileOp::Browse).await {
                Ok(r) => r,
                Err((code, message)) => {
                    tracing::warn!(session, code = message, pe_id = %target.pe_id, "fs remount rejected");
                    self.push(session, wire::error(id.clone(), code, message, ref_data(&target)));
                    return;
                }
            };
            let canonical = match canonical::canonicalize(&resolved.resource_uri) {
                Ok(c) => c.as_str().to_owned(),
                Err(_) => {
                    tracing::warn!(session, code = "provider_unavailable", pe_id = %target.pe_id, "fs remount rejected");
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

        // Phase 2: remount each. Like subscribe's phase 2 this is NOT atomic — one
        // target can fail to re-read (its path was removed) while others recover
        // fine, so skip the failed target and keep every fresh snapshot. A target
        // that is not currently watched yields no output (a legitimate no-op, not a
        // failure). Fall back to the error reply only when a real failure occurred
        // and nothing was remounted.
        let mut snapshots: Vec<Value> = Vec::new();
        let mut failed: usize = 0;
        let mut first_failure: Option<(i64, &'static str, Value)> = None;
        for (target, canonical) in plan {
            match self.shard_handle(Command::Remount { canonical }).await {
                Ok(outputs) => {
                    for output in outputs {
                        if let ShardOutput::Snapshot { snapshot, .. } = output {
                            snapshots.push(wire::snapshot_params(&snapshot, &target));
                        }
                    }
                }
                Err(err) => {
                    failed += 1;
                    let (code, message) = wire::fs_error_to_rpc(&err);
                    tracing::warn!(
                        session,
                        code = message,
                        pe_id = %target.pe_id,
                        rel = %target.relative_path,
                        reason = wire::fs_error_detail(&err),
                        "fs remount: target re-mount failed"
                    );
                    first_failure.get_or_insert_with(|| (code, message, ref_data(&target)));
                }
            }
        }
        // Nothing remounted *and* something failed → surface the failure rather
        // than a misleadingly empty success. (Empty with no failure is valid: the
        // targets were collapsed / not watched.)
        if snapshots.is_empty()
            && let Some((code, message, data)) = first_failure
        {
            self.push(session, wire::error(id, code, message, data));
            return;
        }
        // Lifecycle boundary (low volume). `failed` > 0 marks a degraded reply.
        tracing::info!(
            session,
            targets = target_count,
            snapshots = snapshots.len(),
            failed,
            "fs remount"
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

    /// `fs/createFile`: create an empty file at `file`. Mirrors [`Self::handle_mkdir`]
    /// — same resolve + realpath guard, then the provider's `create_new` open, which
    /// fails `AlreadyExists` rather than truncating an existing file. Neither this nor
    /// `mkdir` creates missing parent directories; the client creates ancestors first.
    async fn handle_create_file(&mut self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<CreateFileParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let resolved = match self.resolve_guarded(user_id, &p.file, FileOp::Write).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.file)));
                return;
            }
        };
        let outcome = self.runtime().provider().create_file(&resolved.resource_uri).await;
        self.reply_unit(session, id, "createFile", &p.file, outcome);
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

    /// `fs/copy` / `fs/move`: transfer `from` into the directory `to_dir`,
    /// preserving the source basename and auto-renaming to a non-colliding
    /// sibling on conflict. Copy keeps the source; move removes it after it lands
    /// (atomic `rename` within one folder root, else copy-then-remove across roots).
    async fn handle_transfer(
        &mut self,
        session: &str,
        user_id: &str,
        id: Option<Value>,
        params: Value,
        mode: Transfer,
    ) {
        let Ok(p) = serde_json::from_value::<TransferParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        // The workspace root has no basename to carry and is never itself a
        // transfer source — reject rather than silently no-op.
        if p.from.relative_path.is_empty() {
            self.push(
                session,
                wire::error(id, wire::CODE_INVALID_PARAMS, "invalid_params", ref_data(&p.from)),
            );
            return;
        }
        let src_op = match mode {
            Transfer::Copy => FileOp::Read,
            Transfer::Move => FileOp::Rename,
        };
        let from = match self.resolve_guarded(user_id, &p.from, src_op).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.from)));
                return;
            }
        };
        let to_dir = match self.resolve_guarded(user_id, &p.to_dir, FileOp::Write).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.to_dir)));
                return;
            }
        };

        // Source kind drives recursive dir copy and the extension-aware rename.
        // A missing source is a resource_not_found, not a silent success.
        let is_dir = match self.runtime().provider().stat(&from.resource_uri).await {
            Ok(Some(fact)) => matches!(fact.kind, Kind::Dir),
            Ok(None) => {
                self.push(
                    session,
                    wire::error(
                        id,
                        wire::CODE_RESOURCE_NOT_FOUND,
                        "resource_not_found",
                        ref_data(&p.from),
                    ),
                );
                return;
            }
            Err(err) => {
                let (code, message) = wire::fs_error_to_rpc(&err);
                self.push(session, wire::error(id, code, message, ref_data(&p.from)));
                return;
            }
        };

        // A directory cannot be transferred into itself or one of its own
        // descendants — that would recurse without end. Compare the resolved,
        // realpath-guarded resource URIs.
        if is_dir && uri_within_or_equal(&to_dir.resource_uri, &from.resource_uri) {
            self.push(
                session,
                wire::error(id, wire::CODE_INVALID_PARAMS, "invalid_params", ref_data(&p.to_dir)),
            );
            return;
        }

        // Moving an entry into the directory it already sits in is a no-op — the
        // frontend blocks it, but guard here too rather than manufacture a
        // spurious "name copy". (Copy into the same dir is a deliberate duplicate.)
        let base = basename_of(&from);
        if mode == Transfer::Move && parent_uri(&from.resource_uri).as_deref() == Some(to_dir.resource_uri.as_str()) {
            self.push(
                session,
                wire::success(id, transfer_result(&p.from.pe_id, &p.from.relative_path, &base)),
            );
            return;
        }

        // Resolve a non-colliding destination child under `to_dir`; each candidate
        // is re-resolved so it inherits the same lexical + realpath containment guard.
        let dest = match self.resolve_free_dest(user_id, &p.to_dir, &base, is_dir).await {
            Ok(Some(dest)) => dest,
            Ok(None) => {
                self.push(
                    session,
                    wire::error(
                        id,
                        wire::CODE_PROVIDER_UNAVAILABLE,
                        "provider_unavailable",
                        ref_data(&p.to_dir),
                    ),
                );
                return;
            }
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.to_dir)));
                return;
            }
        };

        let provider = self.runtime().provider();
        let outcome = match mode {
            Transfer::Copy => provider.copy(&from.resource_uri, &dest.resource_uri, is_dir).await,
            Transfer::Move => {
                // Same folder root → atomic rename. Across roots (or filesystems)
                // rename may fail EXDEV, so fall back to copy-then-remove.
                if from.root_resource_canonical == to_dir.root_resource_canonical {
                    provider.rename(&from.resource_uri, &dest.resource_uri).await
                } else {
                    match provider.copy(&from.resource_uri, &dest.resource_uri, is_dir).await {
                        Ok(()) => provider.remove(&from.resource_uri, is_dir).await,
                        Err(e) => Err(e),
                    }
                }
            }
        };

        match outcome {
            Ok(()) => {
                let op = match mode {
                    Transfer::Copy => "copy",
                    Transfer::Move => "move",
                };
                tracing::info!(session, op, pe_id = %p.to_dir.pe_id, rel = %dest.relative_path, "fs command ok");
                let name = last_segment(&dest.relative_path);
                self.push(
                    session,
                    wire::success(id, transfer_result(&p.to_dir.pe_id, &dest.relative_path, name)),
                );
            }
            Err(err) => {
                let (code, message) = wire::fs_error_to_rpc(&err);
                let op = match mode {
                    Transfer::Copy => "copy",
                    Transfer::Move => "move",
                };
                tracing::warn!(session, op, pe_id = %p.from.pe_id, rel = %p.from.relative_path, code = message, "fs command failed");
                self.push(session, wire::error(id, code, message, ref_data(&p.from)));
            }
        }
    }

    /// Find and resolve the first non-colliding destination child under
    /// `to_dir_ref` for `base` (`name` → `name copy` → `name copy 2` …). Returns
    /// the resolved, realpath-guarded destination, or `Ok(None)` if every attempt
    /// up to the cap is taken (never overwrites).
    async fn resolve_free_dest(
        &self,
        user_id: &str,
        to_dir_ref: &ResourceRef,
        base: &str,
        is_dir: bool,
    ) -> Result<Option<ResolvedResource>, (i64, &'static str)> {
        const MAX_ATTEMPTS: usize = 10_000;
        for attempt in 0..MAX_ATTEMPTS {
            let name = candidate_name(base, attempt, is_dir);
            let candidate = ResourceRef {
                pe_id: to_dir_ref.pe_id.clone(),
                relative_path: join_rel(&to_dir_ref.relative_path, &name),
            };
            let resolved = self.resolve_guarded(user_id, &candidate, FileOp::Write).await?;
            match self.runtime().provider().stat(&resolved.resource_uri).await {
                Ok(None) => return Ok(Some(resolved)),
                Ok(Some(_)) => continue,
                Err(err) => return Err(wire::fs_error_to_rpc(&err)),
            }
        }
        Ok(None)
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

/// Success `result` for `fs/copy` / `fs/move`: the landed entry's pe-relative
/// identity + its final (possibly auto-renamed) name, so the client can reveal
/// or select it without waiting for the destination's delta.
fn transfer_result(pe_id: &str, relative_path: &str, name: &str) -> Value {
    json!({ "to": { "pe_id": pe_id, "relative_path": relative_path }, "name": name })
}

/// Join a directory-relative path with a child name (wire form: forward slashes,
/// no leading slash; an empty dir yields the bare name).
fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The last `/`-segment of a wire relative path (the entry's own name).
fn last_segment(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Basename of a resolved resource — the real on-disk name (case preserved),
/// falling back to the reference's own last relative segment.
fn basename_of(resolved: &ResolvedResource) -> String {
    resolved
        .absolute_path
        .as_deref()
        .and_then(|abs| Path::new(abs).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| last_segment(&resolved.relative_path).to_owned())
}

/// Parent URI of a canonical `file:` child URI: everything up to the last `/`.
/// `None` when there is no separator (should not happen for a resolved child).
fn parent_uri(uri: &str) -> Option<String> {
    uri.rfind('/').map(|i| uri[..i].to_owned())
}

/// Whether `inner` is `outer` itself or nested under it, by canonical URI prefix.
/// A trailing separator on `outer` avoids the `/a/foo` vs `/a/foobar` false match.
fn uri_within_or_equal(inner: &str, outer: &str) -> bool {
    inner == outer || inner.starts_with(&format!("{outer}/"))
}

/// The conflict-free candidate name for the `attempt`-th try (0 = the original).
/// Files keep their extension (`report.txt` → `report copy.txt`); directories and
/// dotfiles take the suffix wholesale (`.env` → `.env copy`).
fn candidate_name(base: &str, attempt: usize, is_dir: bool) -> String {
    if attempt == 0 {
        return base.to_owned();
    }
    let (stem, ext) = if is_dir { (base, "") } else { split_ext(base) };
    if attempt == 1 {
        format!("{stem} copy{ext}")
    } else {
        format!("{stem} copy {attempt}{ext}")
    }
}

/// Split a filename into `(stem, extension-including-dot)` at the last interior
/// dot. A leading dot (dotfile) is not an extension separator, so `.gitignore`
/// → `(".gitignore", "")`.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
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

#[cfg(test)]
#[path = "dispatch_test.rs"]
mod dispatch_test;

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
