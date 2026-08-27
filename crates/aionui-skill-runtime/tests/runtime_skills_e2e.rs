//! Channel A end-to-end.
//!
//! This is a NEW surface reachable with a conversation-scoped runtime token, so
//! the tests lead with the three security constraints rather than the happy path:
//! snapshot scoping, cross-conversation refusal, and traversal containment.

mod common;

use axum::http::StatusCode;
use common::TestHarness;

// ── Security ────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_only_the_skills_in_this_conversations_snapshot() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    h.seed_skill("pdf");
    let conv = h.create_conversation("user_a", &["cron"]).await;

    let body = h.get_json("user_a", &conv, "/api/runtime/skills").await;
    let names: Vec<&str> = body["data"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["cron"], "pdf exists on disk but is not enabled here");
}

/// Without this filter the agent bypasses the skill allow-list entirely: a
/// conversation-scoped token could read any skill on the installation.
#[tokio::test]
async fn a_skill_enabled_in_another_conversation_is_refused_here() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    h.seed_skill("pdf");
    let conv_a = h.create_conversation("user_a", &["cron"]).await;
    let _conv_b = h.create_conversation("user_a", &["pdf"]).await;

    let (status, body) = h.get_raw("user_a", &conv_a, "/api/runtime/skills/pdf").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "enabled in a sibling conversation is still not enabled here: {body}"
    );
    assert_eq!(body["error"]["code"], "skill_not_enabled");
    assert_eq!(body["success"], false);
}

/// A token minted for conversation B must not read conversation A, even for the
/// same user: the token is validated against the conversation, not just the user.
#[tokio::test]
async fn a_token_for_another_conversation_is_rejected() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    let conv_a = h.create_conversation("user_a", &["cron"]).await;
    let conv_b = h.create_conversation("user_a", &["cron"]).await;

    let (status, body) = h
        .get_with_foreign_token("user_a", &conv_a, &conv_b, "/api/runtime/skills")
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "runtime_auth_failed");
}

#[tokio::test]
async fn an_unauthenticated_request_is_rejected() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    let conv = h.create_conversation("user_a", &["cron"]).await;

    let (status, body) = h.get_raw_without_token("user_a", &conv, "/api/runtime/skills").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "runtime_auth_failed");
}

/// A conversation belonging to someone else must not be readable even with a
/// token that validates for the caller's own id.
#[tokio::test]
async fn another_users_conversation_is_not_found() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    let conv_b = h.create_conversation("user_b", &["cron"]).await;

    let (status, body) = h.get_raw("user_a", &conv_b, "/api/runtime/skills").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "conversation_not_found");
}

#[tokio::test]
async fn cat_refuses_every_traversal_shape() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    let conv = h.create_conversation("user_a", &["cron"]).await;

    for bad in [
        "../../../.ssh/id_rsa",
        "references/../../escape.md",
        "/etc/passwd",
        "references/../../../etc/passwd",
        "..",
    ] {
        let uri = format!("/api/runtime/skills/cron/file?path={}", urlencode_for_test(bad));
        let (status, body) = h.get_raw("user_a", &conv, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "path {bad:?} must be refused: {body}");
        assert_eq!(body["error"]["code"], "invalid_path", "path {bad:?}: {body}");
    }
}

/// Nothing about `escape.md` looks suspicious, which is exactly why lexical
/// checks are not enough.
#[cfg(unix)]
#[tokio::test]
async fn cat_does_not_follow_a_symlink_out_of_the_skill_directory() {
    let h = TestHarness::new().await;
    let root = h.seed_skill("cron");
    let outside = h.data_dir().join("outside-secret.md");
    std::fs::write(&outside, "OUTSIDE").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape.md")).unwrap();
    let conv = h.create_conversation("user_a", &["cron"]).await;

    let (status, body) = h
        .get_raw("user_a", &conv, "/api/runtime/skills/cron/file?path=escape.md")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_path");
}

/// The error message must not leak where the escape pointed.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_path_does_not_echo_the_resolved_target() {
    let h = TestHarness::new().await;
    let root = h.seed_skill("cron");
    let outside = h.data_dir().join("outside-secret.md");
    std::fs::write(&outside, "OUTSIDE").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape.md")).unwrap();
    let conv = h.create_conversation("user_a", &["cron"]).await;

    let (_status, body) = h
        .get_raw("user_a", &conv, "/api/runtime/skills/cron/file?path=escape.md")
        .await;
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("outside-secret"),
        "the refusal must not name the escape target: {message}"
    );
}

// ── Functionality ───────────────────────────────────────────────────

/// `show` hands back BOTH the body and the absolute root: a read-only agent
/// needs the content, one that can run commands needs the path.
#[tokio::test]
async fn show_returns_the_body_and_the_absolute_skill_root() {
    let h = TestHarness::new().await;
    let root = h.seed_skill("cron");
    let conv = h.create_conversation("user_a", &["cron"]).await;

    let body = h.get_json("user_a", &conv, "/api/runtime/skills/cron").await;
    assert_eq!(body["data"]["name"], "cron");
    let rendered = body["data"]["body"].as_str().unwrap();
    assert!(rendered.contains("cron body text"));
    assert!(
        !rendered.starts_with("---"),
        "frontmatter is stripped, matching the LOAD_SKILL channel: {rendered:?}"
    );
    assert_eq!(body["data"]["path"], root.display().to_string());
}

#[tokio::test]
async fn cat_reads_a_supplementary_reference_file() {
    let h = TestHarness::new().await;
    h.seed_skill_with_reference("cron", "references/notes.md", "REFTOKEN-4417");
    let conv = h.create_conversation("user_a", &["cron"]).await;

    let body = h
        .get_json(
            "user_a",
            &conv,
            "/api/runtime/skills/cron/file?path=references%2Fnotes.md",
        )
        .await;
    assert_eq!(body["data"]["content"], "REFTOKEN-4417");
    assert_eq!(body["data"]["path"], "references/notes.md", "echoed for correlation");
}

#[tokio::test]
async fn an_empty_snapshot_lists_nothing_rather_than_everything() {
    let h = TestHarness::new().await;
    h.seed_skill("cron");
    let conv = h.create_conversation("user_a", &[]).await;

    let body = h.get_json("user_a", &conv, "/api/runtime/skills").await;
    assert_eq!(body["data"]["skills"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_is_sorted_so_repeated_calls_do_not_reshuffle() {
    let h = TestHarness::new().await;
    for name in ["zeta", "alpha", "middle"] {
        h.seed_skill(name);
    }
    let conv = h.create_conversation("user_a", &["zeta", "alpha", "middle"]).await;

    let body = h.get_json("user_a", &conv, "/api/runtime/skills").await;
    let names: Vec<&str> = body["data"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "middle", "zeta"]);
}

/// A snapshot naming a skill that is not on disk is a broken install, not a
/// permission problem, and the two codes must stay distinguishable.
#[tokio::test]
async fn an_enabled_but_missing_skill_reports_not_found_not_not_enabled() {
    let h = TestHarness::new().await;
    let conv = h.create_conversation("user_a", &["ghost"]).await;

    let (status, body) = h.get_raw("user_a", &conv, "/api/runtime/skills/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "skill_not_found");
}

/// Minimal percent-encoding for the test's own query values.
fn urlencode_for_test(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
