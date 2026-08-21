use super::*;

#[test]
fn same_workspace_renders_the_literal_same() {
    assert_eq!(workspace_field_value(Some("/w/a"), Some("/w/a")), "same");
}

#[test]
fn different_workspace_renders_target_path_with_the_warning_copy() {
    let value = workspace_field_value(Some("/w/a"), Some("/w/b"));
    assert_eq!(value, "/w/b（与你不同）");
}

#[test]
fn unknown_target_workspace_is_reported_as_unknown_not_as_same() {
    // A missing workspace must never collapse to `same`: that would tell the
    // agent relative paths are safe when we do not know that.
    assert_eq!(workspace_field_value(Some("/w/a"), None), "unknown（与你不同）");
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
         重构-鉴权模块\tconv_1\tworkspace: same\n\
         文档站改版\tconv_2\tworkspace: /w/docs（与你不同）\n\
         [[/AION_SESSIONS]]"
    );
}

#[test]
fn sessions_block_carries_no_usage_instructions() {
    // spec §8.3: the sender-side block deliberately carries no command
    // template — the skill covers sending.
    let block = build_sessions_block(
        Some("/w/a"),
        &[SessionMentionTargetInfo {
            id: "conv_1".to_owned(),
            name: "x".to_owned(),
            workspace: Some("/w/a".to_owned()),
        }],
    );
    assert!(!block.contains("send-message"), "{block}");
    assert!(!block.contains("AIONUI_HELPER_BIN"), "{block}");
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
