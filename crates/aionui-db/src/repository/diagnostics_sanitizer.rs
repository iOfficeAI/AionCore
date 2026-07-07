use serde_json::{Map, Value};

const REDACTED: &str = "<redacted>";

pub(crate) fn sanitize_mcp_original_json(raw: Option<&str>) -> Option<String> {
    raw.map(|raw| match serde_json::from_str::<Value>(raw) {
        Ok(value) => {
            serde_json::to_string(&sanitize_mcp_value(&value, None)).unwrap_or_else(|_| sanitize_credential_text(raw))
        }
        Err(_) => sanitize_credential_text(raw),
    })
}

fn sanitize_mcp_value(value: &Value, key: Option<&str>) -> Value {
    if key.is_some_and(is_sensitive_key) {
        return sanitize_sensitive_value(value);
    }

    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), sanitize_mcp_value(value, Some(key))))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => sanitize_mcp_array(items),
        Value::String(text) => Value::String(sanitize_credential_text(text)),
        _ => value.clone(),
    }
}

fn sanitize_mcp_array(items: &[Value]) -> Value {
    let mut sanitized = Vec::with_capacity(items.len());
    let mut redact_next_string = false;

    for item in items {
        if redact_next_string && let Some(text) = item.as_str() {
            sanitized.push(Value::String(if text.trim().is_empty() {
                text.to_owned()
            } else {
                REDACTED.to_owned()
            }));
            redact_next_string = false;
            continue;
        }

        if let Some(text) = item.as_str() {
            redact_next_string = is_sensitive_cli_flag(text);
        } else {
            redact_next_string = false;
        }
        sanitized.push(sanitize_mcp_value(item, None));
    }

    Value::Array(sanitized)
}

fn sanitize_sensitive_value(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::Bool(_) | Value::Number(_) => value.clone(),
        _ => Value::String(REDACTED.to_owned()),
    }
}

fn sanitize_credential_text(text: &str) -> String {
    let mut ranges = Vec::new();
    collect_assignment_ranges(text, &mut ranges);
    collect_bearer_ranges(text, &mut ranges);
    collect_known_token_ranges(text, &mut ranges);
    apply_redaction_ranges(text, ranges)
}

fn collect_assignment_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    for marker in [
        "--access-token=",
        "--api-key=",
        "--api_key=",
        "--token=",
        "--secret=",
        "--password=",
        "access_token=",
        "access-token=",
        "api_key=",
        "apikey=",
        "authorization=",
        "token=",
        "secret=",
        "password=",
    ] {
        collect_value_after_marker(text, marker, ranges);
    }
}

fn collect_bearer_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    collect_value_after_marker(text, "bearer ", ranges);
}

fn collect_value_after_marker(text: &str, marker: &str, ranges: &mut Vec<(usize, usize)>) {
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(relative_start) = lower[search_from..].find(marker) {
        let marker_start = search_from + relative_start;
        let value_start = marker_start + marker.len();
        let value_end = credential_value_end(text, value_start);
        if value_end > value_start {
            ranges.push((value_start, value_end));
        }
        search_from = value_end.max(value_start);
    }
}

fn collect_known_token_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    for prefix in ["sk-", "sntryu_", "AIza"] {
        let mut search_from = 0;
        while let Some(relative_start) = text[search_from..].find(prefix) {
            let start = search_from + relative_start;
            let end = credential_value_end(text, start);
            if end > start {
                ranges.push((start, end));
            }
            search_from = end.max(start + prefix.len());
        }
    }
}

fn credential_value_end(text: &str, value_start: usize) -> usize {
    for (offset, ch) in text[value_start..].char_indices() {
        if is_credential_delimiter(ch) {
            return value_start + offset;
        }
    }
    text.len()
}

fn is_credential_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | '}' | ']' | '&' | ';')
}

fn apply_redaction_ranges(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    if ranges.is_empty() {
        return text.to_owned();
    }

    ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
    let mut out = String::with_capacity(text.len());
    let mut copied_until = 0;
    let mut redacted_until = 0;

    for (start, end) in ranges {
        if end <= redacted_until || start >= end {
            continue;
        }
        let start = start.max(redacted_until);
        out.push_str(&text[copied_until..start]);
        out.push_str(REDACTED);
        copied_until = end;
        redacted_until = end;
    }
    out.push_str(&text[copied_until..]);
    out
}

fn is_sensitive_cli_flag(value: &str) -> bool {
    let flag = value.trim();
    flag.starts_with("--") && !flag.contains('=') && is_sensitive_key(flag.trim_start_matches("--"))
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let normalized = key.replace(['_', '-'], "");
    normalized.contains("apikey")
        || normalized.contains("accesskey")
        || normalized.contains("accesstoken")
        || normalized.contains("authorization")
        || normalized.contains("credential")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
}
