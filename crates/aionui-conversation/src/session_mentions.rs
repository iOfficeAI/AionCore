//! Resolve `@@` conversation references at the send boundary.
//!
//! Mirrors `[[AION_FILES]]` (`aionui-project/src/chat_files.rs`): the block is
//! appended to the message content and persisted verbatim, so persistence,
//! broadcast, and agent input all see the same bytes.

use aionui_api_types::TeamSessionBinding;
use aionui_common::constants::{AIONUI_SESSIONS_END_MARKER, AIONUI_SESSIONS_MARKER};

use crate::error::ConversationError;

/// One `@@` target, resolved from its id by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMentionTargetInfo {
    pub id: String,
    pub name: String,
    /// Absolute workspace path, when the conversation row records one.
    pub workspace: Option<String>,
}

/// The `workspace:` field value for one target.
///
/// One field with a conditional value rather than `workspace` +
/// `same_workspace`: when the workspaces match, the absolute path carries no
/// information (the agent knows its own cwd). When they differ, the constraint
/// travels inside the value so it does not depend on the skill being read.
///
/// An unknown target workspace must render as `unknown`, never `same`:
/// collapsing it to `same` would tell the agent relative paths are safe when
/// we do not know that, which is the exact silent-misread failure this field
/// exists to prevent.
pub fn workspace_field_value(sender_workspace: Option<&str>, target_workspace: Option<&str>) -> String {
    match (sender_workspace, target_workspace) {
        (Some(sender), Some(target)) if sender == target => "same".to_owned(),
        (_, Some(target)) => format!("{target}（与你不同）"),
        (_, None) => "unknown（与你不同）".to_owned(),
    }
}

/// Build the sender-side block. Deliberately carries no usage instructions
/// (spec §8.3): the trigger is the user typing `@@`, and the auto-inject skill
/// is what must independently explain sending.
pub fn build_sessions_block(sender_workspace: Option<&str>, targets: &[SessionMentionTargetInfo]) -> String {
    let mut block = String::from(AIONUI_SESSIONS_MARKER);
    for target in targets {
        block.push('\n');
        block.push_str(&target.name);
        block.push('\t');
        block.push_str(&target.id);
        block.push_str("\tworkspace: ");
        block.push_str(&workspace_field_value(sender_workspace, target.workspace.as_deref()));
    }
    block.push('\n');
    block.push_str(AIONUI_SESSIONS_END_MARKER);
    block
}

/// Read `extra.workspace` out of a conversation row's raw `extra` JSON string.
pub fn workspace_from_extra(extra: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(extra).ok()?;
    value
        .get("workspace")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// `Some(team_id)` when the row is team-owned. Key is `teamId` (camelCase).
///
/// `pub` rather than `pub(crate)`: `aionui-session-message` reuses it so the
/// team hard-filter and the delivery-time team check cannot drift from the
/// send boundary's.
pub fn team_id_from_extra_str(extra: &str) -> Option<String> {
    TeamSessionBinding::team_id_marker_from_extra_str(extra)
}

/// Reject a reference that must not be usable as a `@@` target.
///
/// Pass an empty `sender_conversation_id` to skip the self-reference check —
/// the `@@` picker already excludes the current conversation (spec §5.3), and
/// the CLI side re-checks it as `target_is_self`.
pub fn reject_unusable_target(
    sender_conversation_id: &str,
    target_id: &str,
    target_extra: &str,
) -> Result<(), ConversationError> {
    if !sender_conversation_id.is_empty() && target_id == sender_conversation_id {
        return Err(ConversationError::BadRequest {
            reason: format!("@@ reference targets the current conversation: {target_id}"),
        });
    }
    if team_id_from_extra_str(target_extra).is_some() {
        return Err(ConversationError::Forbidden {
            reason: format!("@@ reference targets a team-owned conversation: {target_id}"),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "session_mentions_test.rs"]
mod session_mentions_test;
