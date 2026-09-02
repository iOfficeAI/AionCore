use super::*;

#[test]
fn same_workspace_renders_the_literal_same() {
    assert_eq!(workspace_field_value(Some("/w/a"), Some("/w/a")), "same");
}

#[test]
fn different_workspace_renders_target_path_with_the_warning_copy() {
    let value = workspace_field_value(Some("/w/a"), Some("/w/b"));
    assert_eq!(value, "/w/b (differs from yours)");
}

#[test]
fn unknown_target_workspace_is_reported_as_unknown_not_as_same() {
    // A missing workspace must never collapse to `same`: that would tell the
    // agent relative paths are safe when we do not know that.
    assert_eq!(
        workspace_field_value(Some("/w/a"), None),
        "unknown (differs from yours)"
    );
}

#[test]
fn sessions_block_is_delimited_and_tab_separated_one_target_per_line() {
    let block = build_sessions_block(
        Some("/w/a"),
        &[
            SessionMentionTargetInfo {
                id: "conv_1".to_owned(),
                name: "重构-鉴权模块".to_owned(),
                workspace: Some("/w/a".to_owned()),
            },
            SessionMentionTargetInfo {
                id: "conv_2".to_owned(),
                name: "文档站改版".to_owned(),
                workspace: Some("/w/docs".to_owned()),
            },
        ],
    );
    assert_eq!(
        block,
        "[[AION_SESSIONS]]\n\
         To deliver to the conversations below, use the session-message skill (address by conversation id); if it is unavailable, run `\"$AIONUI_HELPER_BIN\" session capabilities` for the delivery contract.\n\
         重构-鉴权模块\tconv_1\tworkspace: same\n\
         文档站改版\tconv_2\tworkspace: /w/docs (differs from yours)\n\
         [[/AION_SESSIONS]]"
    );
}

#[test]
fn sessions_block_routes_to_the_session_message_skill_on_its_first_line() {
    // The routing hint must be the FIRST in-marker line and have no tab, so the
    // frontend drops it from the sender's bubble and the chip parser skips it
    // (see `build_sessions_block`). The agent still receives it in the content.
    let block = build_sessions_block(
        Some("/w/a"),
        &[SessionMentionTargetInfo {
            id: "conv_1".to_owned(),
            name: "x".to_owned(),
            workspace: Some("/w/a".to_owned()),
        }],
    );
    let first_inner_line = block.lines().nth(1).expect("a line after the opening marker");
    assert_eq!(
        first_inner_line,
        "To deliver to the conversations below, use the session-message skill (address by conversation id); if it is unavailable, run `\"$AIONUI_HELPER_BIN\" session capabilities` for the delivery contract."
    );
    assert!(
        !first_inner_line.contains('\t'),
        "the hint must not look like a target row: {first_inner_line}"
    );
}

#[test]
fn sessions_block_carries_the_capabilities_fallback_but_no_send_message_payload_template() {
    // spec §8.3 (revised): the block names the `session-message` skill (see
    // `sessions_block_routes_to_the_session_message_skill_on_its_first_line`) and
    // now carries an unconditional `session capabilities` fallback for when that
    // skill is unavailable. The convergence invariant is narrower than "no
    // command at all": what must never appear is a `send-message` PAYLOAD command
    // template (that is the shape that would drift from the skill body). A pointer
    // to the self-describing, descriptor-generated `session capabilities`
    // discovery command is explicitly allowed — it cannot drift.
    let block = build_sessions_block(
        Some("/w/a"),
        &[SessionMentionTargetInfo {
            id: "conv_1".to_owned(),
            name: "x".to_owned(),
            workspace: Some("/w/a".to_owned()),
        }],
    );
    // Still no `send-message` payload template — the skill body owns that shape.
    assert!(!block.contains("send-message"), "{block}");
    // But the capabilities discovery pointer IS present.
    assert!(block.contains("session capabilities"), "{block}");
    assert!(block.contains("$AIONUI_HELPER_BIN"), "{block}");
}

#[test]
fn workspace_is_read_out_of_the_extra_json_and_blank_values_are_ignored() {
    assert_eq!(workspace_from_extra(r#"{"workspace":"/w/a"}"#), Some("/w/a".to_owned()));
    assert_eq!(workspace_from_extra(r#"{"workspace":"  "}"#), None);
    assert_eq!(workspace_from_extra(r#"{}"#), None);
    assert_eq!(workspace_from_extra("not json"), None);
}

#[test]
fn a_team_owned_reference_is_rejected_and_a_self_reference_is_rejected() {
    assert!(reject_unusable_target("conv_a", "conv_b", r#"{}"#).is_ok());
    assert!(matches!(
        reject_unusable_target("conv_a", "conv_a", r#"{}"#),
        Err(ConversationError::BadRequest { .. })
    ));
    assert!(matches!(
        reject_unusable_target("conv_a", "conv_b", r#"{"teamId":"team_1"}"#),
        Err(ConversationError::Forbidden { .. })
    ));
}
