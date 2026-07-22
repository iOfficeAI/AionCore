use std::sync::LazyLock;

use regex::Regex;

/// Version carried by evidence-producing operations so future reprocessing can be explicit.
pub const SANITIZER_VERSION: &str = "memory-sanitizer-v1";
/// Version of deterministic retrieval behavior.
pub const RETRIEVAL_POLICY_VERSION: &str = "memory-retrieval-v1";
/// Version of the durable Memory operation.
pub const OPERATION_VERSION: &str = "memory-operation-v1";
/// Maximum number of selected turns supplied to one task invocation.
pub const MAX_EVIDENCE_TURNS: usize = 32;
/// Maximum number of visible text messages supplied to one task invocation.
pub const MAX_EVIDENCE_MESSAGES: usize = 128;
/// Maximum UTF-8 bytes supplied as sanitized turn evidence.
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;
/// Maximum current entries supplied for reconciliation.
pub const MAX_EXISTING_ENTRIES: usize = 64;
/// Maximum mutations accepted from a single task output.
pub const MAX_MUTATION_COUNT: usize = 32;
/// Maximum UTF-8 bytes accepted for an individual textual field.
pub const MAX_STRING_LENGTH: usize = 8 * 1024;
/// Maximum retained summary values across all summary sections.
pub const MAX_SUMMARY_ITEMS: usize = 64;
/// Maximum UTF-8 bytes retained across a sanitized summary.
pub const MAX_SUMMARY_BYTES: usize = 64 * 1024;

static PRIVATE_KEY_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN(?: [A-Z0-9]+)? PRIVATE KEY-----.*?-----END(?: [A-Z0-9]+)? PRIVATE KEY-----")
        .expect("static private-key pattern is valid")
});
static AUTHORIZATION_BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?im)^(\s*(?:authorization|proxy-authorization)\s*:\s*bearer\s+)(?:"(?:\\.|[^"])*"|'(?:\\.|[^'])*'|[^\r\n]+)$"#)
        .expect("static authorization bearer pattern is valid")
});
static QUOTED_BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bbearer\s+(?:"(?:\\.|[^"])*"|'(?:\\.|[^'])*'|(?:sk-(?:proj-)?[A-Za-z0-9_-]{20,})|(?:ghp_[A-Za-z0-9]{30,})|(?:github_pat_[A-Za-z0-9_]{20,})|(?:[A-Za-z0-9_-]{24,}))"#)
        .expect("static bearer token pattern is valid")
});
static COOKIE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^(cookie|set-cookie)\s*:\s*[^\r\n]+$").expect("static cookie pattern is valid"));
static SENSITIVE_DOUBLE_QUOTED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)((?:["'](?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*["']|\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*\b)\s*[:=]\s*)"(?:\\.|[^"])*""#,
    )
    .expect("static quoted sensitive assignment pattern is valid")
});
static SENSITIVE_SINGLE_QUOTED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)((?:["'](?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*["']|\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*\b)\s*[:=]\s*)'(?:\\.|[^'])*'"#,
    )
    .expect("static single-quoted sensitive assignment pattern is valid")
});
static SENSITIVE_UNQUOTED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)((?:["'](?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*["']|\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*\b)\s*[:=]\s*)([^\r\n,;}\]]+)"#,
    )
    .expect("static unquoted sensitive assignment pattern is valid")
});
static SECRET_ENVIRONMENT_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)^(\s*(?:export\s+)?[A-Z_][A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|PASSWD|API_KEY|APIKEY|COOKIE|CREDENTIAL)[A-Z0-9_]*\s*=\s*)(?:"(?:\\.|[^"])*"|'(?:\\.|[^'])*'|[^\r\n]+)$"#,
    )
    .expect("static secret environment assignment pattern is valid")
});
static RECOGNIZED_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:sk-(?:proj-)?[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|AKIA[A-Z0-9]{16})\b")
        .expect("static recognized token pattern is valid")
});
static USER_CONTEXT_SENTENCE_BOUNDARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[!?;]+|\.(?:\s+|$)").expect("static sentence boundary pattern is valid"));

/// Redacts recognized secret material using stable application-owned rules.
pub fn sanitize_text(value: &str) -> String {
    let private_keys = PRIVATE_KEY_BLOCK.replace_all(value, "[REDACTED PRIVATE KEY]");
    let authorization = AUTHORIZATION_BEARER.replace_all(&private_keys, "$1[REDACTED]");
    let bearer_tokens = QUOTED_BEARER_TOKEN.replace_all(&authorization, "Bearer [REDACTED]");
    let cookie_headers = COOKIE_HEADER.replace_all(&bearer_tokens, "$1: [REDACTED]");
    let environments = SECRET_ENVIRONMENT_ASSIGNMENT.replace_all(&cookie_headers, "$1[REDACTED]");
    let double_quoted = SENSITIVE_DOUBLE_QUOTED_VALUE.replace_all(&environments, "$1\"[REDACTED]\"");
    let single_quoted = SENSITIVE_SINGLE_QUOTED_VALUE.replace_all(&double_quoted, "$1'[REDACTED]'");
    let assignments = SENSITIVE_UNQUOTED_VALUE.replace_all(&single_quoted, "$1[REDACTED]");
    RECOGNIZED_TOKEN.replace_all(&assignments, "[REDACTED]").into_owned()
}

/// Removes User Context sentences while retaining work-local evidence in the same message.
pub fn strip_user_context_sentences(value: &str) -> String {
    USER_CONTEXT_SENTENCE_BOUNDARY
        .split(value)
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty() && !is_user_context_sentence(sentence))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns whether visible conversation text contains only User Context content.
pub fn is_user_context_content(value: &str) -> bool {
    !value.trim().is_empty() && strip_user_context_sentences(value).is_empty()
}

fn is_user_context_sentence(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    if is_work_local_response(&normalized) {
        return false;
    }
    [
        "my name is ",
        "call me ",
        "my profile is ",
        "i prefer ",
        "my favorite ",
        "my preference is ",
        "respond in ",
        "reply in ",
        "please respond in ",
        "always respond ",
        "always reply ",
        "my standing instruction",
        "standing instruction:",
    ]
    .iter()
    .any(|marker| normalized.starts_with(marker))
}

fn is_work_local_response(value: &str) -> bool {
    value.contains("http ") || value.contains("status code") || value.contains("response code")
}

#[cfg(test)]
mod tests {
    use super::{SANITIZER_VERSION, sanitize_text};

    #[test]
    fn redacts_recognized_secrets_deterministically() {
        let raw = concat!(
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345\n",
            "api_key=abcdefghijklmnopqrstuvwxyz012345\n",
            "password: hunter2\n",
            "Cookie: session=abcdefgh\n",
            "export APP_SECRET=top-secret-value\n",
            "-----BEGIN PRIVATE KEY-----\n",
            "private-key-material\n",
            "-----END PRIVATE KEY-----"
        );

        let first = sanitize_text(raw);
        let second = sanitize_text(raw);

        assert_eq!(SANITIZER_VERSION, "memory-sanitizer-v1");
        assert_eq!(first, second);
        for secret in [
            "abcdefghijklmnopqrstuvwxyz012345",
            "hunter2",
            "session=abcdefgh",
            "top-secret-value",
            "private-key-material",
        ] {
            assert!(!first.contains(secret));
        }
        assert!(first.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_structured_and_quoted_secret_values_without_suffix_leakage() {
        let raw = concat!(
            r#"{"api_key":"secret value with spaces and suffix","password":"quoted password suffix","cookie":"session=quoted cookie suffix"}"#,
            "\nexport APP_SECRET='environment secret with suffix'\n",
            "Authorization: Bearer \"bearer secret with suffix\"\n",
            "Cookie: session=secret-cookie-suffix; Path=/\n",
            "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789\n",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        );

        let sanitized = sanitize_text(raw);

        for secret in [
            "secret value with spaces and suffix",
            "quoted password suffix",
            "quoted cookie suffix",
            "environment secret with suffix",
            "bearer secret with suffix",
            "secret-cookie-suffix",
            "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            assert!(!sanitized.contains(secret));
        }
        assert!(sanitized.contains("[REDACTED]"));
    }

    #[test]
    fn preserves_ordinary_bearer_prose_and_safe_sk_identifiers() {
        let raw = "She is a bearer of bad news; retain sk_catalog and sk-not-a-secret.";

        assert_eq!(sanitize_text(raw), raw);
    }
}
