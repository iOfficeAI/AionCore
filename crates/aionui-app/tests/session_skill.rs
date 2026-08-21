//! Content guard for the `session-message` auto-inject skill.
//!
//! This skill is the ONLY verified mechanism by which an ordinary conversation's
//! agent learns cross-session messaging exists (`inject_skills` is a dead field
//! on the claude/codex direct paths). If its body drifts from the wired CLI, the
//! feature is unreachable in practice while every unit test stays green — so the
//! commands it documents are asserted here against the registry.

use std::path::PathBuf;

fn skill_body() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/auto-inject/session-message/SKILL.md");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The body with runs of whitespace collapsed, so prose assertions test what the
/// skill SAYS rather than where the author happened to wrap a line.
fn skill_prose() -> String {
    skill_body().split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_skill_declares_the_name_the_resolver_will_discover() {
    let body = skill_body();
    assert!(body.starts_with("---\n"), "front matter is required");
    assert!(body.contains("name: session-message"), "{body}");
    assert!(body.contains("description:"), "{body}");
}

/// Every subcommand the registry advertises must appear in the skill body,
/// because the skill has to be self-contained enough to send a first message
/// without running `capabilities` first (spec §8.5).
#[test]
fn the_skill_documents_every_wired_session_subcommand() {
    let body = skill_body();
    for descriptor in aionui_api_types::session_tool_descriptors() {
        let command = format!("\"$AIONUI_HELPER_BIN\" session {}", descriptor.cli_command.join(" "));
        assert!(
            body.contains(&command),
            "the skill must document `{command}` (advertised by {})",
            descriptor.name
        );
    }
}

#[test]
fn the_skill_carries_a_copyable_send_and_reply_example() {
    let body = skill_body();
    // A heredoc with both required fields — the minimum for the agent to send
    // without a capabilities round trip.
    assert!(body.contains("<<'JSON'"), "{body}");
    assert!(body.contains("\"to\":"), "{body}");
    assert!(body.contains("\"message\":"), "{body}");
    // Replying must be spelled out, since the recipient's user is not present.
    assert!(body.contains("reply_to"), "{body}");
}

#[test]
fn the_skill_teaches_how_to_read_both_marker_blocks() {
    let body = skill_body();
    assert!(body.contains("[[AION_SESSIONS]]"), "sender-side block: {body}");
    assert!(
        body.contains("[[AION_SESSION_MESSAGE]]"),
        "recipient-side block: {body}"
    );
}

#[test]
fn the_skill_states_the_non_negotiable_rules() {
    let body = skill_prose();
    // Ids only, no broadcast (spec §6.1).
    assert!(body.contains("`to` must be a conversation id"), "{body}");
    assert!(body.contains("Names are not addresses"), "{body}");
    assert!(body.contains("there is no broadcast"), "{body}");
    // Never touch AIONUI_* env vars.
    assert!(
        body.contains("Never pass, inline, export, echo, or set any `AIONUI_...` environment variable"),
        "{body}"
    );
    // Team conversations use the team surface instead (spec §9.2).
    assert!(body.contains("Use `team send-message` instead"), "{body}");
    // rate_limited means stop, not retry (spec §6.7).
    assert!(body.contains("On `rate_limited`, STOP delivering"), "{body}");
    assert!(body.contains("Do not retry"), "{body}");
    // Wording discipline for `queued` (spec §8.6 rule 11).
    assert!(body.contains("Never claim a message was read"), "{body}");
    assert!(
        body.contains("it does NOT mean \"they received it\""),
        "the skill must spell out what `queued` is not: {body}"
    );
    // Cross-workspace constraint (spec §8.6 rule 9).
    assert!(body.contains("Do NOT use relative paths"), "{body}");
    assert!(
        body.contains("Do NOT assume the recipient can read your files"),
        "{body}"
    );
}

#[test]
fn the_skill_points_at_capabilities_only_as_a_fallback() {
    let body = skill_body();
    let capabilities_at = body
        .find("session capabilities")
        .expect("the skill must name the capabilities fallback");
    let send_at = body
        .find("session send-message")
        .expect("the skill must document sending");
    assert!(
        send_at < capabilities_at,
        "the copyable send example must come before the capabilities fallback, \
         so the agent never needs a round trip to send its first message"
    );
}

/// The skill must not tell the agent to write payload files or shell out to
/// unrelated tooling — the same discipline `config_cli_e2e` enforces for the
/// config skill.
#[test]
fn the_skill_does_not_suggest_writing_payload_files_or_foreign_tooling() {
    let body = skill_body();
    for forbidden in ["python3", "aionui_api.py", "curl", "lsof", "netstat"] {
        assert!(!body.contains(forbidden), "the skill must not mention {forbidden}");
    }
}
