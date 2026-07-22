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

static PRIVATE_KEY_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN(?: [A-Z0-9]+)? PRIVATE KEY-----.*?-----END(?: [A-Z0-9]+)? PRIVATE KEY-----")
        .expect("static private-key pattern is valid")
});
static BEARER_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)bearer[ \t]+[^\s,;]+").expect("static bearer pattern is valid"));
static COOKIE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^(cookie|set-cookie)\s*:\s*[^\r\n]+$").expect("static cookie pattern is valid"));
static SENSITIVE_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)(\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|bearer[_-]?token|password|passwd|pwd|secret|cookie|credential)[a-z0-9_-]*\b\s*[:=]\s*)([^\s,;]+)",
    )
    .expect("static sensitive assignment pattern is valid")
});
static SECRET_ENVIRONMENT_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^(\s*(?:export\s+)?[A-Z_][A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|PASSWD|API_KEY|APIKEY|COOKIE|CREDENTIAL)[A-Z0-9_]*\s*=\s*)([^\r\n]+)$",
    )
    .expect("static secret environment assignment pattern is valid")
});
static RECOGNIZED_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:sk|ghp|github_pat|xoxb|xoxp)-?[A-Za-z0-9_-]{16,}\b")
        .expect("static recognized token pattern is valid")
});

/// Redacts recognized secret material using stable application-owned rules.
pub fn sanitize_text(value: &str) -> String {
    let private_keys = PRIVATE_KEY_BLOCK.replace_all(value, "[REDACTED PRIVATE KEY]");
    let bearer_tokens = BEARER_TOKEN.replace_all(&private_keys, "Bearer [REDACTED]");
    let cookie_headers = COOKIE_HEADER.replace_all(&bearer_tokens, "$1: [REDACTED]");
    let environment_values = SECRET_ENVIRONMENT_ASSIGNMENT.replace_all(&cookie_headers, "$1[REDACTED]");
    let assignments = SENSITIVE_ASSIGNMENT.replace_all(&environment_values, "$1[REDACTED]");
    RECOGNIZED_TOKEN.replace_all(&assignments, "[REDACTED]").into_owned()
}

/// Returns whether visible conversation text belongs to User Context rather than work evidence.
pub fn is_user_context_content(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    [
        "my name is ",
        "call me ",
        "i prefer ",
        "my preference is ",
        "respond in ",
        "reply in ",
        "always respond ",
        "always reply ",
        "my standing instruction",
    ]
    .iter()
    .any(|marker| normalized.starts_with(marker))
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
}
