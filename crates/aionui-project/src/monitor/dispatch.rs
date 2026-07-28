//! Inbound JSON-RPC dispatch for the monitor actor.
//!
//! Parses one inner frame, routes by `method`, and drives the runtime:
//! `initialize` handshakes; `fs/subscribe`/`fs/unsubscribe` go through the shard
//! (identity resolved via [`ProjectService::resolve_reference`]); the file
//! commands (`fs/read|write|mkdir|remove|rename`) resolve + realpath-guard, then
//! hit the provider directly. Responses/notifications go out via the actor's
//! push port. Errors map to protocol codes ([`wire`]) with `pe_id`/`relative_path`
//! context in `error.data`.
//!
//! [`ProjectService::resolve_reference`]: crate::ProjectService::resolve_reference

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::canonical;
use crate::runtime::{Command, ShardOutput, Subscriber};
use crate::types::{FileOp, ReferenceInput, ResolvedResource};

use super::actor::FsMonitorActor;
use super::wire::{
    self, Encoding, InitializeParams, MkdirParams, ReadParams, RemoveParams, RenameParams, ResourceRef,
    SubscribeParams, UnsubscribeParams, WriteParams,
};

impl FsMonitorActor {
    /// Decode one inbound frame and route it by method. Malformed frames get a
    /// JSON-RPC error; unknown methods get `method_not_found`.
    pub(super) async fn dispatch_frame(&mut self, session: &str, frame: Value) {
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
            "fs/subscribe" => self.handle_subscribe(session, id, params).await,
            "fs/unsubscribe" => self.handle_unsubscribe(session, params).await,
            "fs/read" => self.handle_read(session, id, params).await,
            "fs/write" => self.handle_write(session, id, params).await,
            "fs/mkdir" => self.handle_mkdir(session, id, params).await,
            "fs/remove" => self.handle_remove(session, id, params).await,
            "fs/rename" => self.handle_rename(session, id, params).await,
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

    async fn handle_subscribe(&mut self, session: &str, id: Option<Value>, params: Value) {
        let Ok(parsed) = serde_json::from_value::<SubscribeParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };

        let target_count = parsed.targets.len();

        // Phase 1: resolve + canonicalize every target before mutating the shard,
        // so a bad target fails the whole request atomically (no partial mount).
        let mut plan: Vec<(ResourceRef, String)> = Vec::new();
        for target in parsed.targets {
            let resolved = match self.resolve(&target, FileOp::Browse).await {
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
    async fn handle_unsubscribe(&mut self, session: &str, params: Value) {
        let Ok(parsed) = serde_json::from_value::<UnsubscribeParams>(params) else {
            return;
        };
        // Subscription de-registration — lifecycle boundary (low volume).
        tracing::info!(session, targets = parsed.targets.len(), "fs unsubscribe");
        let now = self.now();
        for target in parsed.targets {
            let Ok(resolved) = self.resolve(&target, FileOp::Browse).await else {
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

    async fn handle_read(&mut self, session: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<ReadParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let resolved = match self.resolve_guarded(&p.file, FileOp::Read).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.file)));
                return;
            }
        };
        match self.runtime().provider().read(&resolved.resource_uri).await {
            Ok(bytes) => {
                // Byte count only — never the content itself.
                tracing::info!(session, op = "read", pe_id = %p.file.pe_id, rel = %p.file.relative_path, bytes = bytes.len(), "fs command ok");
                let (content, encoding) = encode_content(bytes, p.encoding.unwrap_or_default());
                self.push(
                    session,
                    wire::success(id, json!({ "content": content, "encoding": encoding })),
                );
            }
            Err(err) => {
                let (code, message) = wire::fs_error_to_rpc(&err);
                tracing::warn!(session, op = "read", pe_id = %p.file.pe_id, rel = %p.file.relative_path, code = message, "fs command failed");
                self.push(session, wire::error(id, code, message, ref_data(&p.file)));
            }
        }
    }

    async fn handle_write(&mut self, session: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<WriteParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let bytes = match decode_content(&p.content, p.encoding.unwrap_or_default()) {
            Ok(b) => b,
            Err(()) => {
                self.push(session, invalid_params(id));
                return;
            }
        };
        let resolved = match self.resolve_guarded(&p.file, FileOp::Write).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.file)));
                return;
            }
        };
        let outcome = self.runtime().provider().write(&resolved.resource_uri, &bytes).await;
        self.reply_unit(session, id, "write", &p.file, outcome);
    }

    async fn handle_mkdir(&mut self, session: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<MkdirParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let resolved = match self.resolve_guarded(&p.dir, FileOp::Write).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.dir)));
                return;
            }
        };
        let outcome = self.runtime().provider().mkdir(&resolved.resource_uri).await;
        self.reply_unit(session, id, "mkdir", &p.dir, outcome);
    }

    async fn handle_remove(&mut self, session: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<RemoveParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let resolved = match self.resolve_guarded(&p.target, FileOp::Remove).await {
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

    async fn handle_rename(&mut self, session: &str, id: Option<Value>, params: Value) {
        let Ok(p) = serde_json::from_value::<RenameParams>(params) else {
            self.push(session, invalid_params(id));
            return;
        };
        let from = match self.resolve_guarded(&p.from, FileOp::Rename).await {
            Ok(r) => r,
            Err((code, message)) => {
                self.push(session, wire::error(id, code, message, ref_data(&p.from)));
                return;
            }
        };
        let to = match self.resolve_guarded(&p.to, FileOp::Rename).await {
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

    // ── helpers ───────────────────────────────────────────────────────────

    /// Resolve a reference to identity + lexical containment, mapping the
    /// bind-domain error to a protocol `(code, message)`.
    async fn resolve(&self, target: &ResourceRef, op: FileOp) -> Result<ResolvedResource, (i64, &'static str)> {
        let input = ReferenceInput {
            pe_id: target.pe_id.clone(),
            relative_path: target.relative_path.clone(),
            op,
        };
        self.project()
            .resolve_reference(input)
            .await
            .map_err(|e| wire::project_error_to_rpc(&e))
    }

    /// Resolve + realpath-guard: identity/lexical containment first, then the
    /// access-time symlink/alias escape check before any command IO.
    async fn resolve_guarded(&self, target: &ResourceRef, op: FileOp) -> Result<ResolvedResource, (i64, &'static str)> {
        let resolved = self.resolve(target, op).await?;
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

/// Encode file bytes for the wire, honoring the requested encoding. A utf-8
/// request over non-utf-8 bytes falls back to base64 (the `encoding` field in
/// the result tells the client what it actually got).
fn encode_content(bytes: Vec<u8>, requested: Encoding) -> (String, Encoding) {
    match requested {
        Encoding::Base64 => (STANDARD.encode(&bytes), Encoding::Base64),
        Encoding::Utf8 => match String::from_utf8(bytes) {
            Ok(text) => (text, Encoding::Utf8),
            Err(err) => (STANDARD.encode(err.as_bytes()), Encoding::Base64),
        },
    }
}

/// Decode wire content to bytes per its declared encoding.
fn decode_content(content: &str, encoding: Encoding) -> Result<Vec<u8>, ()> {
    match encoding {
        Encoding::Utf8 => Ok(content.as_bytes().to_vec()),
        Encoding::Base64 => STANDARD.decode(content.as_bytes()).map_err(|_| ()),
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
