//! Agent-readable capability contract for `aioncore diagnose`.

use serde_json::{Value, json};

const RUNTIME_ENV: [&str; 3] = ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID"];

pub(crate) fn data() -> Value {
    json!({
        "schema_version": 1,
        "contract": "agent-facing-diagnose-cli",
        "stability": "stable",
        "input": {
            "default_mode": "stdin_json",
            "business_flags": false,
            "selectors": {
                "conversation_id": {
                    "current": "resolve from AIONUI_CONVERSATION_ID",
                    "literal": "treat as conversation id"
                }
            }
        },
        "output": {
            "stdout": "JSON envelope",
            "stderr": "single stable DIAGNOSE_... error line",
            "success_shape": {
                "success": true,
                "data": {},
                "meta": {
                    "schema_version": 1
                }
            }
        },
        "runtime_context": {
            "primary": "AIONUI_CONVERSATION_ID",
            "environment": RUNTIME_ENV,
            "optional_environment": ["AIONUI_LOG_DIR"]
        },
        "safety": {
            "read_only": true,
            "redacted_by_default": [
                "provider api keys",
                "Authorization headers",
                "MCP headers",
                "environment variables",
                "tokens",
                "passwords",
                "secrets"
            ],
            "http_escape_hatch": {
                "command": "diagnose http get",
                "method": "GET only",
                "allowed_paths": ["/health", "/api/..."],
                "prefer_named_commands": true
            }
        },
        "domains": [
            domain("core", &[
                no_input(&["capabilities"], "Print this agent-readable capability contract."),
                no_input(&["context"], "Read current runtime context."),
                no_input(&["health"], "Read backend health."),
                no_input(&["overview"], "Read a cross-domain diagnostic snapshot."),
            ]),
            domain("conversations", &[
                optional_stdin(&["conversations", "list"], "List conversations with runtime summary.", &["limit"]),
                stdin(&["conversations", "get"], "Read one conversation and stuck/waiting hints.", &["conversation_id"], &["conversation_id"]),
                stdin(&["conversations", "messages"], "Read conversation messages.", &["conversation_id", "limit", "errors_only"], &["conversation_id"]),
            ]),
            domain("providers", &[
                no_input(&["providers", "summary"], "Summarize provider model health."),
            ]),
            domain("mcp", &[
                no_input(&["mcp", "summary"], "Summarize MCP servers and enabled servers with zero tools."),
            ]),
            domain("cron", &[
                no_input(&["cron", "summary"], "Summarize scheduled jobs and failing last run state."),
            ]),
            domain("teams", &[
                no_input(&["teams", "summary"], "Summarize teams and member conversation states."),
            ]),
            domain("logs", &[
                command(CommandDescriptor {
                    path: &["logs", "tail"],
                    description: "Tail aioncore logs from AIONUI_LOG_DIR or stdin log_dir.",
                    input: "stdin_json",
                    stdin_fields: &["log_dir", "lines", "errors_only", "conversation_id"],
                    selectors: &["conversation_id"],
                    escape_hatch: false,
                    requires_context: &[],
                    redacted_fields: &["Authorization", "token", "secret", "password"],
                }),
            ]),
            domain("http", &[
                command(CommandDescriptor {
                    path: &["http", "get"],
                    description: "Controlled GET escape hatch for uncovered diagnostic reads.",
                    input: "stdin_json",
                    stdin_fields: &["path", "reason"],
                    selectors: &[],
                    escape_hatch: true,
                    requires_context: &RUNTIME_ENV,
                    redacted_fields: &["api_key", "headers", "env", "token", "secret", "password"],
                }),
            ]),
        ]
    })
}

fn domain(name: &str, commands: &[Value]) -> Value {
    json!({
        "name": name,
        "commands": commands,
    })
}

fn no_input(path: &[&str], description: &str) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "none",
        stdin_fields: &[],
        selectors: &[],
        escape_hatch: false,
        requires_context: if path == ["capabilities"] { &[] } else { &RUNTIME_ENV },
        redacted_fields: &[],
    })
}

fn optional_stdin(path: &[&str], description: &str, stdin_fields: &[&str]) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "optional_stdin_json",
        stdin_fields,
        selectors: &[],
        escape_hatch: false,
        requires_context: &RUNTIME_ENV,
        redacted_fields: &[],
    })
}

fn stdin(path: &[&str], description: &str, stdin_fields: &[&str], selectors: &[&str]) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "stdin_json",
        stdin_fields,
        selectors,
        escape_hatch: false,
        requires_context: &RUNTIME_ENV,
        redacted_fields: &[],
    })
}

struct CommandDescriptor<'a> {
    path: &'a [&'a str],
    description: &'a str,
    input: &'a str,
    stdin_fields: &'a [&'a str],
    selectors: &'a [&'a str],
    escape_hatch: bool,
    requires_context: &'a [&'a str],
    redacted_fields: &'a [&'a str],
}

fn command(spec: CommandDescriptor<'_>) -> Value {
    json!({
        "path": spec.path,
        "command": format!("diagnose {}", spec.path.join(" ")),
        "description": spec.description,
        "input": spec.input,
        "stdin_fields": spec.stdin_fields,
        "selectors": spec.selectors,
        "readback": false,
        "destructive": false,
        "escape_hatch": spec.escape_hatch,
        "requires_context": spec.requires_context,
        "redacted_fields": spec.redacted_fields,
    })
}
