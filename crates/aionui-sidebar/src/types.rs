//! Wire-format helpers and the crate error type.
//!
//! Everything the two endpoints must parse/format lives here so the service can
//! stay focused on classification: the crate error ([`SidebarError`]), the
//! scope-token grammar ([`ScopeToken`]), the pseudo-dir key codec, the keyset
//! [`Cursor`], and the `win` list parser. See `api-contract-sidebar.md` §3.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64_URL};
use serde::{Deserialize, Serialize};

use aionui_db::DbError;
use aionui_project::ProjectError;

/// Cursor / group-ordering version. Bumped when the keyset ordering changes so a
/// stale client cursor is rejected (400) instead of silently mis-paging.
pub const CURSOR_VERSION: u32 = 1;

/// Default per-group window when the request omits `limit` / a `win` entry.
pub const DEFAULT_LIMIT: i64 = 5;
/// Default `items` window when the request omits `limit`.
pub const DEFAULT_ITEMS_LIMIT: i64 = 10;
/// Per-window hard cap (both `limit` and any `win` entry).
pub const MAX_LIMIT: i64 = 100;
/// Cap on the number of `win` entries in one `GET /api/sidebar` request.
pub const MAX_WIN_ENTRIES: usize = 100;

/// Errors surfaced by the sidebar service. Route handlers map these to
/// `ApiError` with stable machine codes; DB/project detail never leaks out.
#[derive(Debug, thiserror::Error)]
pub enum SidebarError {
    /// Malformed request input (bad `win`, bad cursor, unknown scope/item type,
    /// out-of-range limit). Maps to 400.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The paged scope no longer exists (project removed, pseudo-dir emptied).
    /// Maps to 404 so the frontend drops that group.
    #[error("scope no longer exists")]
    ScopeGone,
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error("internal: {0}")]
    Internal(String),
}

/// A parsed `scope` token (`api-contract-sidebar.md` §3.3).
///
/// The pseudo-dir variant carries the *decoded* canonical path, not the raw
/// base64url key — decoding happens at parse time so the service compares plain
/// canonical strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeToken {
    Pinned,
    Project(String),
    /// Decoded canonical path of the pseudo-dir group.
    Dir(String),
    Chats,
}

impl ScopeToken {
    /// Parse a `scope` token. Unknown prefixes / undecodable dir keys → `None`
    /// (the caller turns that into a 400).
    pub fn parse(token: &str) -> Option<Self> {
        if token == "pinned" {
            return Some(ScopeToken::Pinned);
        }
        if token == "chats" {
            return Some(ScopeToken::Chats);
        }
        if let Some(id) = token.strip_prefix("project:") {
            return (!id.is_empty()).then(|| ScopeToken::Project(id.to_owned()));
        }
        if let Some(key) = token.strip_prefix("dir:") {
            return dir_key_to_canonical(key).map(ScopeToken::Dir);
        }
        None
    }

    /// Format back to the wire token (the inverse of [`parse`](Self::parse)).
    pub fn to_token(&self) -> String {
        match self {
            ScopeToken::Pinned => "pinned".to_owned(),
            ScopeToken::Chats => "chats".to_owned(),
            ScopeToken::Project(id) => format!("project:{id}"),
            ScopeToken::Dir(canonical) => format!("dir:{}", canonical_to_dir_key(canonical)),
        }
    }
}

/// Encode a pseudo-dir canonical path into its scope-token key.
///
/// Canonical paths contain `:` (the `file:` scheme), so they cannot be passed
/// raw in a `<prefix>:<value>` token — base64url (no padding) keeps the token a
/// single opaque segment.
pub fn canonical_to_dir_key(canonical: &str) -> String {
    B64_URL.encode(canonical.as_bytes())
}

/// Decode a pseudo-dir scope-token key back to its canonical path. `None` when
/// the key is not valid base64url / not UTF-8.
pub fn dir_key_to_canonical(key: &str) -> Option<String> {
    let bytes = B64_URL.decode(key.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// Keyset cursor position within a group. `Pinned` scopes page by `order_key`
/// ascending; every other scope pages by `updated_at` descending. Both tie-break
/// on the full `(item_type, item_id)` pair, so no position is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    Pinned {
        order_key: i64,
        item_type: String,
        item_id: String,
    },
    Activity {
        updated_at: i64,
        item_type: String,
        item_id: String,
    },
}

/// Serialized cursor payload (base64 of this JSON). `scope` binds the cursor to
/// the group it was minted for; a cursor replayed against another scope is
/// rejected rather than silently mis-applied.
#[derive(Debug, Serialize, Deserialize)]
struct CursorPayload {
    v: u32,
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order_key: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
    item_type: String,
    item_id: String,
}

impl Cursor {
    /// Encode this cursor for the given scope token: `base64(JSON)`.
    pub fn encode(&self, scope: &ScopeToken) -> String {
        let payload = match self {
            Cursor::Pinned {
                order_key,
                item_type,
                item_id,
            } => CursorPayload {
                v: CURSOR_VERSION,
                scope: scope.to_token(),
                order_key: Some(*order_key),
                updated_at: None,
                item_type: item_type.clone(),
                item_id: item_id.clone(),
            },
            Cursor::Activity {
                updated_at,
                item_type,
                item_id,
            } => CursorPayload {
                v: CURSOR_VERSION,
                scope: scope.to_token(),
                order_key: None,
                updated_at: Some(*updated_at),
                item_type: item_type.clone(),
                item_id: item_id.clone(),
            },
        };
        // Serialization of a plain struct cannot fail; fall back to empty rather
        // than panicking on the impossible branch.
        B64.encode(serde_json::to_vec(&payload).unwrap_or_default())
    }

    /// Decode a cursor and verify it was minted for `scope` at this ordering
    /// version. Bad base64 / bad JSON / wrong version / cross-scope / missing
    /// keyset field → `BadRequest` (never a silent reset to the first page).
    pub fn decode(raw: &str, scope: &ScopeToken) -> Result<Self, SidebarError> {
        let bytes = B64
            .decode(raw.as_bytes())
            .map_err(|_| SidebarError::BadRequest("cursor is not valid base64".into()))?;
        let payload: CursorPayload =
            serde_json::from_slice(&bytes).map_err(|_| SidebarError::BadRequest("cursor JSON is malformed".into()))?;
        if payload.v != CURSOR_VERSION {
            return Err(SidebarError::BadRequest("cursor version mismatch".into()));
        }
        if payload.scope != scope.to_token() {
            return Err(SidebarError::BadRequest("cursor belongs to a different scope".into()));
        }
        match scope {
            ScopeToken::Pinned => {
                let order_key = payload
                    .order_key
                    .ok_or_else(|| SidebarError::BadRequest("pinned cursor missing order_key".into()))?;
                Ok(Cursor::Pinned {
                    order_key,
                    item_type: payload.item_type,
                    item_id: payload.item_id,
                })
            }
            _ => {
                let updated_at = payload
                    .updated_at
                    .ok_or_else(|| SidebarError::BadRequest("activity cursor missing updated_at".into()))?;
                Ok(Cursor::Activity {
                    updated_at,
                    item_type: payload.item_type,
                    item_id: payload.item_id,
                })
            }
        }
    }
}

/// Parse the repeated `win=<scope-token>:<limit>` query params into a
/// scope-token → window-size map.
///
/// The scope token itself contains `:` (`project:P1`, `dir:<key>`), so the limit
/// is taken from after the *last* `:`. Duplicate tokens, malformed entries,
/// out-of-range limits, or more than [`MAX_WIN_ENTRIES`] entries → `BadRequest`.
/// Whether a named project/dir currently exists is *not* checked here — a syntax-
/// valid but stale window is tolerated by the service (BR-15), not rejected.
pub fn parse_win(entries: &[String]) -> Result<Vec<(String, i64)>, SidebarError> {
    if entries.len() > MAX_WIN_ENTRIES {
        return Err(SidebarError::BadRequest("too many win entries".into()));
    }
    let mut out: Vec<(String, i64)> = Vec::with_capacity(entries.len());
    for entry in entries {
        let (token, limit_str) = entry
            .rsplit_once(':')
            .ok_or_else(|| SidebarError::BadRequest(format!("malformed win entry: {entry}")))?;
        if ScopeToken::parse(token).is_none() {
            return Err(SidebarError::BadRequest(format!("unknown win scope: {token}")));
        }
        let limit: i64 = limit_str
            .parse()
            .map_err(|_| SidebarError::BadRequest(format!("win limit is not a number: {entry}")))?;
        validate_limit(limit)?;
        if out.iter().any(|(t, _)| t == token) {
            return Err(SidebarError::BadRequest(format!("duplicate win scope: {token}")));
        }
        out.push((token.to_owned(), limit));
    }
    Ok(out)
}

/// Enforce the per-window `[1, MAX_LIMIT]` bound shared by `limit` and `win`.
pub fn validate_limit(limit: i64) -> Result<(), SidebarError> {
    if (1..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(SidebarError::BadRequest(format!(
            "limit out of range [1,{MAX_LIMIT}]: {limit}"
        )))
    }
}

#[cfg(test)]
#[path = "types_test.rs"]
mod types_test;
