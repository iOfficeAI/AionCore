//! Content guard for the `conversation-create` auto-inject skill.
//!
//! The skill is the only channel through which an ordinary conversation's agent
//! learns `aioncore conversation` exists. If its body drifts from the wired CLI
//! the feature is unreachable while every unit test stays green — so the
//! commands and rules it documents are asserted here.

use std::path::PathBuf;

fn skill_body() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets/builtin-skills/auto-inject/conversation-create/SKILL.md");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Whitespace-collapsed body, so prose assertions test what the skill SAYS
/// rather than where a line happened to wrap.
fn skill_prose() -> String {
    skill_body().split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_skill_declares_the_name_the_resolver_will_discover() {
    let body = skill_body();
    assert!(body.starts_with("---\n"), "front matter is required");
    assert!(body.contains("name: conversation-create"), "{body}");
    assert!(body.contains("description:"), "{body}");
}

#[test]
fn the_skill_stays_within_sixty_lines() {
    let lines = skill_body().lines().count();
    assert!(lines <= 60, "skill is {lines} lines; the design caps it at 60");
}

/// Every subcommand the registry advertises must appear in the body: the skill
/// has to be self-contained enough to create without running `capabilities`.
#[test]
fn the_skill_documents_every_wired_conversation_subcommand() {
    let body = skill_body();
    for descriptor in aionui_api_types::conversation_tool_descriptors() {
        let command = format!(
            "\"$AIONUI_HELPER_BIN\" conversation {}",
            descriptor.cli_command.join(" ")
        );
        assert!(
            body.contains(&command),
            "the skill must document `{command}` (advertised by {})",
            descriptor.name
        );
    }
}

#[test]
fn the_skill_carries_a_copyable_create_example_with_every_field() {
    let body = skill_body();
    assert!(body.contains("<<'JSON'"), "{body}");
    assert!(body.contains("\"name\":"), "{body}");
    assert!(body.contains("\"workspace\":"), "{body}");
    assert!(body.contains("\"assistant_id\":"), "{body}");
}

#[test]
fn the_skill_states_the_non_negotiable_rules() {
    let body = skill_prose();
    // Rule 1 — only on request.
    assert!(body.contains("Never create one on your own initiative"), "{body}");
    // Rule 2 — name required, in the user's language.
    assert!(body.contains("`name` is required"), "{body}");
    assert!(body.contains("in the user's language"), "{body}");
    // Rules 3/4 — omit to inherit; assistant ids come from the config family.
    assert!(
        body.contains("Omit `workspace` to reuse this conversation's directory"),
        "{body}"
    );
    assert!(
        body.contains("Omit `assistant_id` to reuse this conversation's assistant"),
        "{body}"
    );
    assert!(body.contains("\"$AIONUI_HELPER_BIN\" config assistants list"), "{body}");
    // Rule 5 — never touch AIONUI_* env vars.
    assert!(
        body.contains("Never pass, inline, export, echo, or set any `AIONUI_...` environment variable"),
        "{body}"
    );
    // Rule 6 — team conversations are out of scope.
    assert!(
        body.contains("If this conversation belongs to a team, do not use this skill"),
        "{body}"
    );
    // Rule 7 — creating is only creating.
    assert!(
        body.contains("does NOT send anything to the new conversation"),
        "{body}"
    );
    assert!(body.contains("does NOT open it"), "{body}");
    assert!(
        body.contains("does NOT switch the user's current conversation"),
        "{body}"
    );
    // Rule 8 — honest failure reporting.
    assert!(body.contains("never claim the conversation was created"), "{body}");
}

/// The two skills must not reference each other; composition is the agent's
/// own reasoning.
#[test]
fn the_skill_does_not_reference_the_session_message_family() {
    let body = skill_body();
    for forbidden in [
        "session send-message",
        "session-message",
        "session capabilities",
        "@@",
        "[[AION_SESSION",
    ] {
        assert!(!body.contains(forbidden), "the skill must not mention {forbidden}");
    }
}

#[test]
fn the_skill_points_at_capabilities_only_as_a_fallback() {
    let body = skill_body();
    let create_at = body
        .find("\n\"$AIONUI_HELPER_BIN\" conversation create")
        .expect("the skill must carry a copyable create command");
    let capabilities_at = body
        .find("\n\"$AIONUI_HELPER_BIN\" conversation capabilities")
        .expect("the skill must carry a copyable capabilities fallback command");
    assert!(
        create_at < capabilities_at,
        "create must precede the capabilities fallback"
    );
}

#[test]
fn the_skill_does_not_suggest_writing_payload_files_or_foreign_tooling() {
    let body = skill_body();
    for forbidden in ["python3", "aionui_api.py", "curl", "lsof", "netstat"] {
        assert!(!body.contains(forbidden), "the skill must not mention {forbidden}");
    }
}

/// The session-message skill is deliberately frozen: this feature must not
/// have touched it.
#[test]
fn the_session_message_skill_still_does_not_mention_conversation_create() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/auto-inject/session-message/SKILL.md");
    let body = std::fs::read_to_string(path).unwrap();
    assert!(
        !body.contains("conversation create"),
        "session-message/SKILL.md must stay untouched"
    );
    assert!(
        !body.contains("conversation-create"),
        "session-message/SKILL.md must stay untouched"
    );
}
