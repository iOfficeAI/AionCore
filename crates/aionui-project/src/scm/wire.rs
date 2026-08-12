//! `scm/*` wire layer: JSON-RPC payload shapes and error mapping.
//!
//! The outer transport envelope (`WebSocketMessage { name: "scm", data }`) is the
//! composition layer's concern; this module models only the inner frame. A
//! separate router from the explorer's `name: "fs"` because the two protocols are
//! shaped differently — that one is directory-subscription oriented, this one is
//! repository oriented (`formal/runtime/protocol.md`).
//!
//! Pure: no filesystem, no engine, no transport.

use serde::Deserialize;
use serde_json::{Value, json};

use super::error::ScmError;
use super::types::{ContentRef, ScmRepository};

const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC framing codes.
pub(super) const CODE_INVALID_REQUEST: i64 = -32600;
pub(super) const CODE_METHOD_NOT_FOUND: i64 = -32601;
pub(super) const CODE_INVALID_PARAMS: i64 = -32602;

// Protocol-semantic codes shared with the explorer link.
pub(super) const CODE_OUT_OF_SCOPE: i64 = -32000;
pub(super) const CODE_RESOURCE_NOT_FOUND: i64 = -32002;
pub(super) const CODE_PROVIDER_UNAVAILABLE: i64 = -32006;

// Source-control specific codes. Distinct from the shared range above so a
// client can tell "this root is not a repository" from a generic failure.
pub(super) const CODE_NOT_A_REPOSITORY: i64 = -32050;
pub(super) const CODE_SCM_OPERATION_FAILED: i64 = -32051;
pub(super) const CODE_CAPABILITY_UNSUPPORTED: i64 = -32052;
/// The request was refused because a resource's current state does not allow the
/// action — distinct from `scm_operation_failed`, which means the action ran and
/// broke. Retrying a refusal can never succeed until the resource changes (the
/// user resolves a conflict), so a client must be able to tell the two apart
/// without parsing messages.
pub(super) const CODE_RESOURCE_BLOCKED: i64 = -32053;

/// A decoded inbound frame. `id` is absent for notifications (`scm/unsubscribe`).
#[derive(Debug, Clone, Deserialize)]
pub(super) struct IncomingFrame {
    #[serde(default)]
    pub(super) id: Option<Value>,
    pub(super) method: String,
    #[serde(default)]
    pub(super) params: Value,
}

/// A JSON-RPC success response.
pub(super) fn success(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result })
}

/// A JSON-RPC error response. `data` carries UI context; pass `Value::Null` when
/// there is none.
pub(super) fn error(id: Option<Value>, code: i64, message: &str, data: Value) -> Value {
    let mut err = json!({ "code": code, "message": message });
    if !data.is_null() {
        err["data"] = data;
    }
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "error": err })
}

/// A server-initiated notification.
pub(super) fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "method": method, "params": params })
}

/// The `scm/repositoriesChanged` notification: a project's repository set gained
/// and/or lost members.
///
/// `project_id` is always present so a client that holds one store across several
/// projects (and receives every project's frame on its shared connection) can
/// discard the ones that are not the project it is looking at — the server pushes
/// to a session as long as it once listed *any* of the projects it is interested
/// in, and does not itself know which project that session currently shows.
/// `user_id` is deliberately absent: it is an internal authorization detail, not
/// part of the outward contract.
///
/// `added` carries whole repository descriptors (the client splices them in);
/// `removed` carries only ids (there is nothing left to describe). Each is omitted
/// when empty, and the caller does not emit the frame at all when both are — an
/// empty change is not a change.
pub(super) fn repositories_changed(project_id: &str, added: &[ScmRepository], removed: &[String]) -> Value {
    let mut params = json!({ "project_id": project_id });
    if !added.is_empty() {
        // Infallible in practice (ScmRepository is a plain data struct); an empty
        // array on the impossible error keeps the frame well-formed rather than
        // dropping the whole notification.
        params["added"] = serde_json::to_value(added).unwrap_or_else(|_| json!([]));
    }
    if !removed.is_empty() {
        params["removed"] = json!(removed);
    }
    notification("scm/repositoriesChanged", params)
}

/// Map a domain error onto its wire code, name and context.
///
/// Kept in one place so every method reports the same failure the same way, and
/// so the mapping is reviewable against the protocol's error table.
pub(super) fn map_error(err: &ScmError) -> (i64, &'static str, Value) {
    match err {
        ScmError::OutOfScope { pe_id } => (CODE_OUT_OF_SCOPE, "out_of_scope", json!({ "pe_id": pe_id })),
        ScmError::InvalidParams { what } => (CODE_INVALID_PARAMS, "invalid_params", json!({ "param": what })),
        ScmError::NotARepository { root } => (CODE_NOT_A_REPOSITORY, "not_a_repository", json!({ "root": root })),
        // An unknown repository id is a stale client reference (the repository was
        // released or never discovered), which is a lookup failure, not an engine
        // one.
        ScmError::UnknownRepository { repo_id } => (
            CODE_RESOURCE_NOT_FOUND,
            "resource_not_found",
            json!({ "repo_id": repo_id }),
        ),
        ScmError::NotFound { path } => (CODE_RESOURCE_NOT_FOUND, "resource_not_found", json!({ "path": path })),
        ScmError::CapabilityUnsupported { capability } => (
            CODE_CAPABILITY_UNSUPPORTED,
            "capability_unsupported",
            json!({ "capability": capability }),
        ),
        // A refusal, not a malfunction: the resource is conflicted and stage 1
        // deliberately offers no action on it. Retrying cannot help until the
        // user resolves it, which is why this is not an operation failure.
        ScmError::OpaqueResource { path } => (
            CODE_RESOURCE_BLOCKED,
            "resource_blocked",
            json!({ "path": path, "reason": "conflicted" }),
        ),
        ScmError::OperationFailed { context, message } => (
            CODE_SCM_OPERATION_FAILED,
            "scm_operation_failed",
            json!({ "context": context, "message": message }),
        ),
        ScmError::Io { path, message } => (
            CODE_PROVIDER_UNAVAILABLE,
            "provider_unavailable",
            json!({ "path": path, "message": message }),
        ),
    }
}

/// Build an error response straight from a domain error.
pub(super) fn error_from(id: Option<Value>, err: &ScmError) -> Value {
    let (code, name, data) = map_error(err);
    error(id, code, name, data)
}

/// Parse a `ContentRef` from its wire spelling.
pub(super) fn parse_content_ref(value: &Value) -> Option<ContentRef> {
    match value.as_str()? {
        "working" => Some(ContentRef::Working),
        "committed" => Some(ContentRef::Committed),
        "staged" => Some(ContentRef::Staged),
        _ => None,
    }
}

#[cfg(test)]
#[path = "wire_test.rs"]
mod wire_test;
