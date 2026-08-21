use super::*;

#[test]
fn same_workspace_block_matches_the_spec_shape_exactly() {
    let block = build_session_message_block("重构-鉴权模块", "conv_1", "same", "conv_1");
    assert_eq!(
        block,
        "[[AION_SESSION_MESSAGE]]\n\
         from: 重构-鉴权模块\tconv_1\n\
         workspace: same\n\
         reply_to: conv_1\t（回信: session send-message, to=reply_to）\n\
         [[/AION_SESSION_MESSAGE]]"
    );
}

#[test]
fn cross_workspace_block_carries_the_constraint_inside_the_field_value() {
    let block = build_session_message_block(
        "A",
        "conv_1",
        "/Users/x/proj-a（与你不同，勿用相对路径，勿假设可读）",
        "conv_1",
    );
    assert!(
        block.contains("workspace: /Users/x/proj-a（与你不同，勿用相对路径，勿假设可读）"),
        "{block}"
    );
}

#[test]
fn the_block_always_states_how_to_reply() {
    // spec §8.3: the recipient's user is not present, so the receiving agent
    // must be self-evidently able to reply. A bare `reply_to:` field that only
    // makes sense after reading SKILL.md is betting the model reads docs.
    let block = build_session_message_block("A", "conv_1", "same", "conv_1");
    assert!(block.contains("session send-message"), "{block}");
    assert!(block.contains("to=reply_to"), "{block}");
}

#[test]
fn the_delivered_content_puts_the_block_before_the_body() {
    // Before, not after: it is context, not an attachment.
    let content = compose_delivery_content(
        &build_session_message_block("A", "conv_1", "same", "conv_1"),
        "接口定完了吗？",
    );
    assert!(content.starts_with("[[AION_SESSION_MESSAGE]]"), "{content}");
    assert!(content.trim_end().ends_with("接口定完了吗？"), "{content}");
}

#[test]
fn the_recipient_workspace_field_says_same_only_when_both_sides_match() {
    assert_eq!(recipient_workspace_field(Some("/w/a"), Some("/w/a")), "same");
    assert_eq!(
        recipient_workspace_field(Some("/w/a"), Some("/w/b")),
        "/w/a（与你不同，勿用相对路径，勿假设可读）"
    );
}

#[test]
fn an_unknown_sender_workspace_never_collapses_to_same() {
    // Same failure this field exists to prevent: telling the recipient that
    // relative paths are safe when we do not know that.
    let value = recipient_workspace_field(None, Some("/w/b"));
    assert!(value.starts_with("unknown"), "{value}");
    assert!(value.contains("勿用相对路径"), "{value}");

    let both_unknown = recipient_workspace_field(None, None);
    assert!(both_unknown.starts_with("unknown"), "{both_unknown}");
}

#[test]
fn a_known_sender_workspace_with_an_unknown_target_is_reported_as_different() {
    // The recipient block states the SENDER's path, so it stays usable even
    // when the target row records no workspace.
    let value = recipient_workspace_field(Some("/w/a"), None);
    assert_eq!(value, "/w/a（与你不同，勿用相对路径，勿假设可读）");
}

// ---------------------------------------------------------------------------
// Which delivery failures deserve another tick
// ---------------------------------------------------------------------------

#[test]
fn a_busy_target_is_retried_rather_than_dropped() {
    let verdict = classify_delivery_failure(ConversationError::Busy {
        reason: "a turn is already running".to_owned(),
    });
    assert!(matches!(verdict, DeliverAttemptError::Transient(_)), "{verdict:?}");
}

/// Regression, observed live: the user restarted a conversation's runtime while
/// two messages were queued for it. The cancel hook deliberately KEPT them
/// (`cause = RuntimeRestart`), and 300ms later the drainer threw them away with
/// "dropped after a hard delivery error, reason=Conversation runtime is
/// restarting" — silent message loss, which is this feature's worst failure
/// mode, and it made the hook's whole cause distinction pointless.
///
/// A restart is a ~1s window, not a rejection: the conversation comes back
/// IDLE, which is exactly the state a pending delivery is waiting for.
#[test]
fn a_restarting_runtime_is_retried_rather_than_dropped() {
    let verdict = classify_delivery_failure(ConversationError::RuntimeRestarting {
        conversation_id: "conv_b".to_owned(),
    });
    assert!(
        matches!(verdict, DeliverAttemptError::Transient(_)),
        "a restart window must not discard queued work: {verdict:?}"
    );
}

/// The other half of the contract: retrying must not become "never drop
/// anything". A target that no longer exists, or refuses the send, is a real
/// answer and the item goes.
#[test]
fn a_real_rejection_is_still_dropped() {
    for error in [
        ConversationError::NotFound {
            id: "conv_gone".to_owned(),
        },
        ConversationError::Forbidden {
            reason: "Team-owned conversations must be sent through Team API".to_owned(),
        },
    ] {
        let rendered = error.to_string();
        let verdict = classify_delivery_failure(error);
        assert!(
            matches!(verdict, DeliverAttemptError::Hard(_)),
            "{rendered} must not be retried forever: {verdict:?}"
        );
    }
}
