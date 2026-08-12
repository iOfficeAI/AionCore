//! Monitor protocol wire types (JSON-RPC 2.0 payload layer).
//!
//! The outer transport envelope (`WebSocketMessage { name: "fs", data }`) is the
//! composition layer's concern; this module models only the inner JSON-RPC frame
//! (`protocol.md`): request/response/notification builders, method params, the
//! outward `Entry`/`Snapshot`/`Delta` shapes, and error-code mapping from the
//! backend error taxonomies (`ProjectError` / `FsError`) to the protocol's
//! `-32000..` codes. Pure and filesystem-free.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::runtime::{Change, DeltaBatch, EntryFact, FsError, Kind, Snapshot};
use crate::types::ProjectError;

/// JSON-RPC version string carried on every frame.
pub const JSONRPC_VERSION: &str = "2.0";

/// Highest monitor protocol version this server implements.
pub const PROTOCOL_VERSION: u32 = 1;

// ── Protocol error codes (protocol.md error table) ────────────────────────
// Only codes stage 1 actually emits are defined here. Protocol also reserves
// `-32700 parse_error` (JSON framing — handled at the realtime transport layer,
// never reaches this payload dispatch) and `-32005 file_operation_denied`
// (conversation/team mode write-guards — a stage-2 capability). Both are
// intentionally NOT defined until wired, per the workspace no-unwired-placeholder
// rule; see the stage-1 test report §6.
pub const CODE_INVALID_REQUEST: i64 = -32600;
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
pub const CODE_INVALID_PARAMS: i64 = -32602;
pub const CODE_OUT_OF_SCOPE: i64 = -32000;
pub const CODE_UNSUPPORTED_RESOURCE_SCHEME: i64 = -32001;
pub const CODE_RESOURCE_NOT_FOUND: i64 = -32002;
pub const CODE_RESOURCE_OUTSIDE_FOLDER: i64 = -32003;
pub const CODE_INVALID_RELATIVE_PATH: i64 = -32004;
pub const CODE_PROVIDER_UNAVAILABLE: i64 = -32006;
pub const CODE_PROTOCOL_VERSION_UNSUPPORTED: i64 = -32010;

// ── Incoming frame ────────────────────────────────────────────────────────

/// A decoded inbound JSON-RPC frame. `id` is absent for notifications
/// (`fs/unsubscribe`); `params` defaults to null when omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingFrame {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A resource reference `{ pe_id, relative_path }`. One shape serves every
/// method's `DirRef`/`FileRef` (identity is the same; op intent is per-method).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ResourceRef {
    pub pe_id: String,
    pub relative_path: String,
}

// ── Method params ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeParams {
    pub targets: Vec<ResourceRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnsubscribeParams {
    pub targets: Vec<ResourceRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MkdirParams {
    pub dir: ResourceRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoveParams {
    pub target: ResourceRef,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RenameParams {
    pub from: ResourceRef,
    pub to: ResourceRef,
}

// ── Filename search (fs/search) ───────────────────────────────────────────

/// `fs/search` request params (protocol.md). `roots` = the project's bound
/// folders (or narrowed subdirs); `query` empty = browse; `limit` is the global
/// hit budget shared across all roots (server default when omitted).
#[derive(Debug, Clone, Deserialize)]
pub struct SearchParams {
    pub roots: Vec<ResourceRef>,
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `fs/searchCancel` notification params. `search_id` echoes the originating
/// `fs/search` request `id` (any JSON value the client used as its id).
#[derive(Debug, Clone, Deserialize)]
pub struct SearchCancelParams {
    pub search_id: Value,
}

/// One filename hit — the chat-ref `project` identity (files only). `pe_id` is
/// stamped by the orchestration layer from the root the hit came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    pub pe_id: String,
    pub relative_path: String,
    pub name: String,
}

/// Build the `fs/searchMatch` notification params: a batch of hits keyed to the
/// originating search's `id` (echoed as `search_id`).
pub fn search_match_params(search_id: &Value, matches: &[SearchHit]) -> Value {
    json!({
        "search_id": search_id,
        "matches": matches,
    })
}

/// Build the `fs/search` terminal response `result` (`{ limit_reached, total }`).
pub fn search_result(limit_reached: bool, total: usize) -> Value {
    json!({
        "limit_reached": limit_reached,
        "total": total,
    })
}

// ── Outward entry / snapshot / delta ──────────────────────────────────────

/// Entry kind on the wire (`protocol.md` `Entry.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WireKind {
    File,
    Dir,
    Symlink,
}

impl From<Kind> for WireKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::File => WireKind::File,
            Kind::Dir => WireKind::Dir,
            Kind::Symlink => WireKind::Symlink,
        }
    }
}

/// One directory entry as the client sees it: name + kind (+ symlink target).
/// `excluded` (excludes-set membership, e.g. `node_modules`) is not wired in
/// stage 1 — the excludes source is not yet plumbed; omitted from output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireEntry {
    pub name: String,
    pub kind: WireKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
}

impl WireEntry {
    /// Project a backend `(name, EntryFact)` to the outward entry (drops inode).
    pub fn from_fact(name: &str, fact: &EntryFact) -> Self {
        WireEntry {
            name: name.to_owned(),
            kind: fact.kind.into(),
            symlink_target: fact.symlink_target.clone(),
        }
    }
}

/// Build the `fs/snapshot` params for one target from a canonical-domain
/// [`Snapshot`]. The `target` is the subscriber's pe-relative identity.
///
/// Used for the `fs/subscribe` reply, where a snapshot is simply the caller's
/// first view of a directory. The push that follows an overflow rescan carries an
/// extra marker and goes through [`overflow_snapshot_params`].
pub fn snapshot_params(snapshot: &Snapshot, target: &ResourceRef) -> Value {
    json!({
        "target": target,
        "entries": wire_entries(snapshot),
    })
}

/// Build the `fs/snapshot` params for an overflow rescan, tagged so the receiver
/// can tell it apart from a first listing.
///
/// The two are indistinguishable on the wire otherwise, and they mean opposite
/// things: a subscribe reply says "here is what this directory holds", whereas an
/// overflow push says "the kernel dropped events, so anything here may have
/// changed — including file contents, which a listing cannot show".
///
/// A receiver that treats the second as the first silently loses every change in
/// that window. Overflow supersedes buffered per-child events during debounce
/// (`runtime::actor`), so it does not merely lack deltas — it replaces the deltas
/// that would otherwise have been sent. The marker is what lets a receiver
/// respond conservatively (re-read what it is showing) instead of assuming
/// nothing happened.
///
/// `reason` is an added optional field, so this needs no protocol version bump:
/// receivers that do not know it ignore it and behave exactly as before, which is
/// the same compatibility argument the `modified` op relies on.
pub fn overflow_snapshot_params(snapshot: &Snapshot, target: &ResourceRef) -> Value {
    json!({
        "target": target,
        "entries": wire_entries(snapshot),
        "reason": "overflow",
    })
}

/// Project a canonical-domain snapshot's entries to their outward form.
fn wire_entries(snapshot: &Snapshot) -> Vec<WireEntry> {
    snapshot
        .entries
        .iter()
        .map(|(name, fact)| WireEntry::from_fact(name, fact))
        .collect()
}

/// Build the `fs/delta` params for one target from a canonical-domain
/// [`DeltaBatch`], translating each [`Change`] to its wire shape.
pub fn delta_params(delta: &DeltaBatch, target: &ResourceRef) -> Value {
    let changes: Vec<Value> = delta.changes.iter().map(change_to_wire).collect();
    json!({
        "target": target,
        "changes": changes,
    })
}

/// One reconciled change → tagged wire object (`op` = added/removed/renamed/
/// modified).
fn change_to_wire(change: &Change) -> Value {
    match change {
        Change::Added { name, kind } => json!({
            "op": "added",
            "name": name,
            "kind": WireKind::from(*kind),
        }),
        Change::Removed { name } => json!({
            "op": "removed",
            "name": name,
        }),
        Change::Renamed { from, to } => json!({
            "op": "renamed",
            "from": from,
            "to": to,
        }),
        // No mtime on the wire, by design — see `Change::Modified`. Adding one op
        // is a backward-compatible extension: clients ignore ops they do not know,
        // so this needs no protocol version bump and no lockstep release.
        Change::Modified { name } => json!({
            "op": "modified",
            "name": name,
        }),
    }
}

// ── Frame builders ────────────────────────────────────────────────────────

/// A JSON-RPC success response carrying `result` for request `id`.
pub fn success(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

/// A JSON-RPC error response for request `id`. `data` carries UI context
/// (e.g. `pe_id`/`relative_path`); pass `Value::Null` when there is none.
pub fn error(id: Option<Value>, code: i64, message: &str, data: Value) -> Value {
    let mut err = json!({ "code": code, "message": message });
    if !data.is_null() {
        err["data"] = data;
    }
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": err,
    })
}

/// A server-initiated JSON-RPC notification (no `id`).
pub fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
        "params": params,
    })
}

// ── Error mapping ─────────────────────────────────────────────────────────

/// Map a bind-domain [`ProjectError`] (identity/containment resolution) to a
/// protocol `(code, message)`. `message` is the stable protocol name.
pub fn project_error_to_rpc(err: &ProjectError) -> (i64, &'static str) {
    match err {
        ProjectError::ProjectExplorerNotFound { .. } | ProjectError::ProjectNotFound { .. } => {
            (CODE_OUT_OF_SCOPE, "out_of_scope")
        }
        ProjectError::InvalidRelativePath { .. } => (CODE_INVALID_RELATIVE_PATH, "invalid_relative_path"),
        ProjectError::ResourceOutsideFolder { .. } => (CODE_RESOURCE_OUTSIDE_FOLDER, "resource_outside_folder"),
        ProjectError::UnsupportedResourceScheme { .. } => {
            (CODE_UNSUPPORTED_RESOURCE_SCHEME, "unsupported_resource_scheme")
        }
        ProjectError::FolderNotFound { .. } => (CODE_RESOURCE_NOT_FOUND, "resource_not_found"),
        ProjectError::FolderPermissionDenied { .. } => (CODE_PROVIDER_UNAVAILABLE, "provider_unavailable"),
        // Remaining bind-chain variants are not reachable from reference
        // resolution on the monitor path; surface as provider_unavailable rather
        // than leaking internal bind semantics onto the fs wire.
        _ => (CODE_PROVIDER_UNAVAILABLE, "provider_unavailable"),
    }
}

/// Map a runtime-link [`FsError`] (provider IO) to a protocol `(code, message)`.
pub fn fs_error_to_rpc(err: &FsError) -> (i64, &'static str) {
    match err {
        FsError::NotFound { .. } => (CODE_RESOURCE_NOT_FOUND, "resource_not_found"),
        FsError::UnsupportedScheme { .. } => (CODE_UNSUPPORTED_RESOURCE_SCHEME, "unsupported_resource_scheme"),
        // AlreadyExists / PermissionDenied / NotADirectory / Io are all provider
        // availability/operation failures from the client's perspective.
        FsError::AlreadyExists { .. }
        | FsError::PermissionDenied { .. }
        | FsError::NotADirectory { .. }
        | FsError::Io { .. } => (CODE_PROVIDER_UNAVAILABLE, "provider_unavailable"),
    }
}

#[cfg(test)]
#[path = "wire_test.rs"]
mod wire_test;
