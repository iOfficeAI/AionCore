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

const SENSITIVE_KEY_PATTERN: &str = r"(?:api[_-]?(?:key|token)|access[_-]?token|auth[_-]?token|bearer[_-]?token|refresh[_-]?token|session[_-]?token|client[_-]?secret|password|passwd|pwd|secret|cookie|credential|token)";

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
    Regex::new(&format!(
        r#"(?is)((?:["']{SENSITIVE_KEY_PATTERN}["']|\b{SENSITIVE_KEY_PATTERN}\b)\s*[:=]\s*)"(?:\\.|[^"])*""#,
    ))
    .expect("static quoted sensitive assignment pattern is valid")
});
static SENSITIVE_SINGLE_QUOTED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?is)((?:["']{SENSITIVE_KEY_PATTERN}["']|\b{SENSITIVE_KEY_PATTERN}\b)\s*[:=]\s*)'(?:\\.|[^'])*'"#,
    ))
    .expect("static single-quoted sensitive assignment pattern is valid")
});
static SENSITIVE_UNQUOTED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        &format!(
            r#"(?im)((?:["']{SENSITIVE_KEY_PATTERN}["']|\b{SENSITIVE_KEY_PATTERN}\b)\s*=\s*|(?:^\s*|[{{,;]\s*)(?:["']{SENSITIVE_KEY_PATTERN}["']|\b{SENSITIVE_KEY_PATTERN}\b)\s*:\s*)([^\s,;}}\]]+)"#,
        ),
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
    let mut retained = String::with_capacity(value.len());
    let mut start = 0;

    for (index, character) in value.char_indices() {
        let end = index + character.len_utf8();
        let is_boundary = matches!(character, '!' | '?' | ';')
            || (character == '.' && value[end..].chars().next().is_none_or(char::is_whitespace));
        if !is_boundary {
            continue;
        }

        let sentence = &value[start..end];
        if !is_user_context_sentence(sentence) {
            retained.push_str(sentence);
        }
        start = end;
    }

    if start < value.len() {
        let sentence = &value[start..];
        if !is_user_context_sentence(sentence) {
            retained.push_str(sentence);
        }
    }

    retained
}

/// Returns whether visible conversation text contains only User Context content.
pub fn is_user_context_content(value: &str) -> bool {
    !value.trim().is_empty() && strip_user_context_sentences(value).is_empty()
}

fn is_user_context_sentence(value: &str) -> bool {
    let normalized = value
        .trim()
        .trim_end_matches(['.', '!', '?', ';'])
        .trim()
        .to_ascii_lowercase();
    if is_work_local_response(&normalized) {
        return false;
    }
    normalized.contains("my name is ")
        || normalized.contains("call me ")
        || normalized.contains("my profile is ")
        || normalized.starts_with("my favorite ")
        || normalized.starts_with("my preference is ")
        || is_response_preference(&normalized)
        || normalized.starts_with("always respond ")
        || normalized.starts_with("always reply ")
        || normalized.contains("standing instruction")
}

fn is_work_local_response(value: &str) -> bool {
    value.contains("http ")
        || value.contains("status code")
        || value.contains("response code")
        || value.contains("for this endpoint")
        || value.contains("for the endpoint")
        || value.contains("for deployment")
        || value.contains("for this project")
        || value.contains("for the project")
}

fn is_response_preference(value: &str) -> bool {
    let is_preference = value.starts_with("i prefer ") || value.starts_with("my preference is ");
    is_preference
        && [
            "response",
            "responses",
            "reply",
            "replies",
            "concise",
            "verbose",
            "tone",
            "language",
        ]
        .iter()
        .any(|marker| value.contains(marker))
        || value.starts_with("respond in ")
        || value.starts_with("reply in ")
        || value.starts_with("please respond in ")
}

#[cfg(test)]
mod tests {
    use super::{SANITIZER_VERSION, sanitize_text, strip_user_context_sentences};

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
            assert!(!sanitized.contains(secret), "leaked secret: {secret}");
        }
        assert!(sanitized.contains("[REDACTED]"));
    }

    #[test]
    fn preserves_ordinary_bearer_prose_and_safe_sk_identifiers() {
        let raw = "She is a bearer of bad news; retain sk_catalog and sk-not-a-secret.";

        assert_eq!(sanitize_text(raw), raw);
    }

    #[test]
    fn redacts_normalized_secret_keys_in_json_environments_and_assignments() {
        let raw = concat!(
            r#"{"api_token":"api token with suffix","refresh_token":"refresh token with suffix","session_token":"session token with suffix","client_secret":"client secret with suffix","token":"plain token with suffix"}"#,
            "\nAPI_TOKEN=\"environment token with suffix\"\n",
            "refresh_token=refresh-assignment-suffix\n",
            "session_token: session-assignment-suffix\n",
            "client_secret = 'client assignment with suffix'\n",
            "token=plain-assignment-suffix"
        );

        let sanitized = sanitize_text(raw);

        for secret in [
            "api token with suffix",
            "refresh token with suffix",
            "session token with suffix",
            "client secret with suffix",
            "plain token with suffix",
            "environment token with suffix",
            "refresh-assignment-suffix",
            "session-assignment-suffix",
            "client assignment with suffix",
            "plain-assignment-suffix",
        ] {
            assert!(!sanitized.contains(secret), "leaked secret: {secret}");
        }
    }

    #[test]
    fn preserves_documentation_and_retained_user_context_punctuation() {
        let raw = concat!(
            "The password: must contain 12 characters. ",
            "password=hunter2; client_secret: \"client secret with spaces suffix\"; token='token with spaces suffix'."
        );
        let sanitized = sanitize_text(raw);

        assert!(sanitized.contains("The password: must contain 12 characters."));
        for secret in [
            "hunter2",
            "client secret with spaces suffix",
            "token with spaces suffix",
        ] {
            assert!(!sanitized.contains(secret));
        }

        let context = concat!(
            "Hi, my name is Ada. Please call me Ada! I prefer concise responses; ",
            "Please respond in Vietnamese. Always reply in Vietnamese. Always reply with JSON for this endpoint! ",
            "I prefer option B for deployment; Keep the HTTP 503 response code? Document /work/report.md."
        );
        let retained = strip_user_context_sentences(context);

        for excluded in [
            "my name is Ada",
            "call me Ada",
            "I prefer concise responses",
            "Please respond in Vietnamese",
            "Always reply in Vietnamese",
        ] {
            assert!(!retained.contains(excluded));
        }
        for preserved in [
            "Always reply with JSON for this endpoint!",
            "I prefer option B for deployment;",
            "Keep the HTTP 503 response code?",
            "Document /work/report.md.",
        ] {
            assert!(retained.contains(preserved));
        }
    }
}
