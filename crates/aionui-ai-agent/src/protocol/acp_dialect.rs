//! Local tolerant pre-parse layer for CodeBuddy's non-standard ACP dialect.
//!
//! The vendored `agent-client-protocol(-schema)` only accepts the canonical
//! `SessionUpdate` variants and hard-rejects two CodeBuddy-specific
//! `session/update` shapes with a `-32602 Invalid params` deserialization
//! error, silently dropping signals we could otherwise use to attribute an
//! empty turn:
//!
//!   (a) `session_end` — a terminal marker carrying `stopReason:"end_turn"`.
//!   (b) `compact-maxtoken` / `codebuddy.ai/compactType:"emergency-auto"` — an
//!       emergency auto-compaction notification emitted as the conversation
//!       approaches the context/token limit.
//!
//! This module classifies a single raw newline-delimited JSON-RPC line at the
//! transport boundary — *before* the SDK parses it — so those two shapes can be
//! absorbed (and turned into an internal signal) instead of surfacing `-32602`.
//! Every other line, including genuinely malformed input and any other unknown
//! variant, is forwarded unchanged so the SDK still surfaces real errors.
//!
//! Wire shapes are grounded in captured real data from this issue's
//! `attachments/extracted/logs.txt` (deserialization-rejection payloads at
//! lines 8798 / 523), not inferred — see AGENTS.md "only assert what an
//! approved source proves".

use crate::protocol::events::AcpDialectSignalKind;
use serde_json::Value;

/// What the tolerant layer decides to do with one incoming JSON-RPC line.
pub(crate) enum LineDisposition {
    /// Hand the line to the SDK unchanged (canonical or unknown-but-not-ours).
    Forward(String),
    /// A recognised CodeBuddy dialect notification: absorb it (never reaches the
    /// SDK, so no `-32602`) and raise the corresponding internal signal.
    Absorb(AcpDialectSignalKind),
}

/// The `_meta` key CodeBuddy uses to tag an emergency-compaction notification.
const COMPACT_TYPE_META_KEY: &str = "codebuddy.ai/compactType";
/// Prefix of the `messageId` CodeBuddy stamps on max-token compaction chunks.
const COMPACT_MESSAGE_ID_PREFIX: &str = "compact-maxtoken";

/// Classify one raw newline-delimited JSON-RPC line per the wire criteria in
/// `logs.txt`. Only the two recognised CodeBuddy shapes are absorbed; anything
/// else (parse failure, other method, other/unknown variant) is forwarded.
pub(crate) fn classify_incoming_line(line: &str) -> LineDisposition {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        // Genuinely malformed input must still reach the SDK (validation #4:
        // never swallow real errors).
        return LineDisposition::Forward(line.to_owned());
    };

    // Only `session/update` notifications are candidates.
    if value.get("method").and_then(Value::as_str) != Some("session/update") {
        return LineDisposition::Forward(line.to_owned());
    }

    let Some(update) = value.pointer("/params/update") else {
        return LineDisposition::Forward(line.to_owned());
    };

    // (a) `session_end` terminal marker — an unknown `sessionUpdate` variant.
    if update.get("sessionUpdate").and_then(Value::as_str) == Some("session_end") {
        return LineDisposition::Absorb(AcpDialectSignalKind::SessionEnd);
    }

    // (b) emergency compaction: identified by the codebuddy compact markers, NOT
    // by the `sessionUpdate` variant (it is the legal `agent_message_chunk`, so
    // an "unknown variant" test would miss it). The internal compaction summary
    // is `isCompactInternal:true` and is safely ignored — never rendered.
    let has_compact_meta = update
        .get("_meta")
        .and_then(|meta| meta.get(COMPACT_TYPE_META_KEY))
        .is_some();
    let has_compact_message_id = update
        .get("messageId")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with(COMPACT_MESSAGE_ID_PREFIX));
    if has_compact_meta || has_compact_message_id {
        return LineDisposition::Absorb(AcpDialectSignalKind::TokenPressure);
    }

    // (c) everything else — including other unknown variants and malformed
    // messages — is forwarded so the SDK can surface `-32602` as before.
    LineDisposition::Forward(line.to_owned())
}

/// Extract non-sensitive correlation context from an absorbed dialect line for
/// `info`-level logging. Returns `(session_id, session_update_marker)`; never
/// touches content/summary/prompt payloads.
pub(crate) fn absorbed_log_context(line: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return (None, None);
    };
    let session_id = value
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let update = value.pointer("/params/update");
    // Prefer the explicit compactType marker; otherwise the sessionUpdate kind.
    let marker = update
        .and_then(|u| {
            u.get("_meta")
                .and_then(|meta| meta.get(COMPACT_TYPE_META_KEY))
                .and_then(Value::as_str)
        })
        .or_else(|| update.and_then(|u| u.get("sessionUpdate").and_then(Value::as_str)))
        .map(str::to_owned);
    (session_id, marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absorb_kind(line: &str) -> Option<AcpDialectSignalKind> {
        match classify_incoming_line(line) {
            LineDisposition::Absorb(kind) => Some(kind),
            LineDisposition::Forward(forwarded) => {
                // Forward must preserve the original line verbatim.
                assert_eq!(forwarded, line);
                None
            }
        }
    }

    // Real `session_end` payload shape (logs.txt:8798), wrapped in its JSON-RPC
    // notification envelope as it appears on the wire.
    const SESSION_END_LINE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"19c4b027-88cd-41d7-a8fb-29dea4208737","update":{"_meta":{"codebuddy.ai":{"timestamp":1785036577598},"timestamp":"2026-07-26T03:29:37.598Z"},"sessionUpdate":"session_end","stopReason":"end_turn"}}}"#;

    // Real `compact-maxtoken` / `emergency-auto` payload (logs.txt:523).
    const COMPACT_LINE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"20bffd06-7f7a-4548-af8e-eea0ea3504ae","update":{"_meta":{"codebuddy.ai":{"messageId":"compact-maxtoken-20bffd06-7f7a-4548-af8e-eea0ea3504ae-1785027303300","timestamp":1785027303301},"codebuddy.ai/compactType":"emergency-auto","codebuddy.ai/isCompactInternal":true,"timestamp":"2026-07-26T00:55:03.301Z"},"content":[{"text":"<conversation_history_summary><summary>","type":"text"}],"messageId":"compact-maxtoken-20bffd06-7f7a-4548-af8e-eea0ea3504ae-1785027303300","sessionUpdate":"agent_message_chunk"}}}"#;

    #[test]
    fn session_end_is_absorbed_as_session_end_signal() {
        assert_eq!(absorb_kind(SESSION_END_LINE), Some(AcpDialectSignalKind::SessionEnd));
    }

    #[test]
    fn compact_maxtoken_is_absorbed_as_token_pressure() {
        assert_eq!(absorb_kind(COMPACT_LINE), Some(AcpDialectSignalKind::TokenPressure));
    }

    #[test]
    fn compact_detected_by_message_id_prefix_alone() {
        // Even without the compactType meta, a `compact-maxtoken` messageId marks it.
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"messageId":"compact-maxtoken-s1-123","sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}"#;
        assert_eq!(absorb_kind(line), Some(AcpDialectSignalKind::TokenPressure));
    }

    #[test]
    fn normal_agent_message_chunk_is_forwarded() {
        // A genuine streaming chunk (no compact markers) must reach the SDK.
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}"#;
        assert_eq!(absorb_kind(line), None);
    }

    #[test]
    fn other_unknown_variant_is_forwarded_for_sdk_to_reject() {
        // An unknown variant that is NOT one of ours must still surface -32602.
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"future_unknown_kind"}}}"#;
        assert_eq!(absorb_kind(line), None);
    }

    #[test]
    fn non_session_update_method_is_forwarded() {
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s1"}}"#;
        assert_eq!(absorb_kind(line), None);
    }

    #[test]
    fn malformed_json_is_forwarded() {
        assert_eq!(absorb_kind("not json at all"), None);
        assert_eq!(absorb_kind("{\"method\":"), None);
    }

    #[test]
    fn absorbed_log_context_extracts_session_and_marker_without_content() {
        let (sid, marker) = absorbed_log_context(COMPACT_LINE);
        assert_eq!(sid.as_deref(), Some("20bffd06-7f7a-4548-af8e-eea0ea3504ae"));
        assert_eq!(marker.as_deref(), Some("emergency-auto"));

        let (sid, marker) = absorbed_log_context(SESSION_END_LINE);
        assert_eq!(sid.as_deref(), Some("19c4b027-88cd-41d7-a8fb-29dea4208737"));
        assert_eq!(marker.as_deref(), Some("session_end"));
    }
}
