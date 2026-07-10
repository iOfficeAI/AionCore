//! Agent-readable capability contract for `aioncore config`.

use serde_json::{Value, json};

const RUNTIME_ENV: [&str; 3] = ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID"];

pub(crate) fn data() -> Value {
    json!({
        "schema_version": 1,
        "contract": "agent-facing-config-cli",
        "stability": "stable",
        "input": {
            "default_mode": "stdin_json",
            "business_flags": false,
            "selectors": {
                "assistant_id": {
                    "current": "resolve via AIONUI_CONVERSATION_ID",
                    "literal": "treat as assistant id"
                },
                "conversation_id": {
                    "current": "resolve from AIONUI_CONVERSATION_ID",
                    "literal": "treat as conversation id"
                },
                "user_id": {
                    "current": "resolve from AIONUI_USER_ID",
                    "literal": "treat as user id"
                }
            }
        },
        "output": {
            "stdout": "JSON envelope",
            "stderr": "single stable CONFIG_... error line",
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
            "environment": RUNTIME_ENV
        },
        "safety": {
            "redacted_by_default": [
                "provider api keys",
                "Bedrock access keys and secrets",
                "MCP headers",
                "MCP stdio env values",
                "agent env values",
                "prompt and rule content outside explicit read commands"
            ],
            "read_before_write": true
        },
        "domains": [
            domain("core", &[
                no_input(&["capabilities"], "Print this agent-readable capability contract.", false),
                no_input(&["context"], "Read the current runtime context and current conversation assistant.", false),
            ]),
            domain("assistants", &[
                no_input(&["assistants", "list"], "List assistants.", false),
                stdin(&["assistants", "get"], "Read one assistant.", &["assistant_id", "locale"], &["assistant_id"], false, false),
                stdin(&["assistants", "create"], "Create an assistant.", &["name", "description", "agent_id", "prompts", "enabled_skills"], &[], true, false),
                stdin(&["assistants", "update"], "Update assistant metadata, defaults, or enabled skills.", &["assistant_id", "locale"], &["assistant_id"], true, false),
                stdin(&["assistants", "delete"], "Delete an assistant.", &["assistant_id"], &["assistant_id"], true, true),
                stdin(&["assistants", "import"], "Import assistants.", &["items"], &[], true, false),
                stdin(&["assistants", "state"], "Enable, disable, or reorder an assistant.", &["assistant_id", "enabled", "sort_order"], &["assistant_id"], true, false),
                stdin_redacted(&["assistants", "rule", "read"], "Read an assistant rule.", &["assistant_id", "locale"], &["assistant_id"], false, false, &[]),
                stdin_redacted(&["assistants", "rule", "write"], "Write an assistant rule.", &["assistant_id", "locale", "content"], &["assistant_id"], true, false, &["content"]),
                stdin_redacted(&["assistants", "rule", "delete"], "Delete an assistant rule.", &["assistant_id", "locale"], &["assistant_id"], true, true, &["content"]),
                stdin_redacted(&["assistants", "skill", "read"], "Read assistant skill content.", &["assistant_id", "locale"], &["assistant_id"], false, false, &[]),
                stdin_redacted(&["assistants", "skill", "write"], "Write assistant skill content.", &["assistant_id", "locale", "content"], &["assistant_id"], true, false, &["content"]),
                stdin_redacted(&["assistants", "skill", "delete"], "Delete assistant skill content.", &["assistant_id", "locale"], &["assistant_id"], true, true, &["content"]),
            ]),
            domain("skills", &[
                no_input(&["skills", "list"], "List available skills.", false),
                stdin(&["skills", "info"], "Inspect a skill path.", &["skill_path"], &[], false, false),
                no_input(&["skills", "paths"], "List configured skill paths.", false),
                stdin(&["skills", "import"], "Import a skill.", &["skill_path"], &[], true, false),
                stdin(&["skills", "delete"], "Delete a skill.", &["skill_name"], &[], true, true),
                stdin(&["skills", "scan"], "Scan for importable skills.", &["path"], &[], false, false),
                no_input(&["skills", "external-paths", "list"], "List external skill paths.", false),
                stdin(&["skills", "external-paths", "add"], "Add an external skill path.", &["name", "path"], &[], true, false),
                stdin(&["skills", "external-paths", "remove"], "Remove an external skill path.", &["path"], &[], true, true),
                no_input(&["skills", "market", "enable"], "Enable the skill market.", true),
                no_input(&["skills", "market", "disable"], "Disable the skill market.", true),
            ]),
            domain("mcp", &[
                no_input_redacted(&["mcp", "servers", "list"], "List MCP servers.", false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "servers", "get"], "Read one MCP server.", &["server_id"], &[], false, false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "servers", "create"], "Create an MCP server.", &["name", "transport"], &[], true, false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "servers", "update"], "Update an MCP server.", &["server_id", "transport"], &[], true, false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "servers", "delete"], "Delete an MCP server.", &["server_id"], &[], true, true, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "servers", "toggle"], "Toggle an MCP server.", &["server_id"], &[], true, false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "servers", "import"], "Import MCP servers.", &["servers"], &[], true, false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "test-connection"], "Test an MCP server configuration.", &["name", "transport"], &[], false, false, &["transport.headers", "transport.env"]),
                no_input_redacted(&["mcp", "agent-configs"], "List agent MCP config state.", false, &["transport.headers", "transport.env"]),
                stdin_redacted(&["mcp", "oauth", "check-status"], "Check MCP OAuth status.", &["server_url"], &[], false, false, &[]),
                stdin_redacted(&["mcp", "oauth", "login"], "Start MCP OAuth login.", &["server_url"], &[], true, false, &[]),
                stdin_redacted(&["mcp", "oauth", "logout"], "Logout MCP OAuth.", &["server_url"], &[], true, false, &[]),
                no_input_redacted(&["mcp", "oauth", "authenticated"], "List authenticated MCP servers.", false, &[]),
            ]),
            domain("providers", &[
                no_input_redacted(&["providers", "list"], "List model providers.", false, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "create"], "Create a model provider.", &["name", "platform", "base_url", "api_key"], &[], true, false, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "update"], "Update a model provider.", &["provider_id"], &[], true, false, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "delete"], "Delete a model provider.", &["provider_id"], &[], true, true, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "detect-protocol"], "Detect provider protocol.", &["base_url", "api_key"], &[], false, false, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "fetch-models"], "Fetch provider models from a raw provider config.", &["base_url", "api_key"], &[], false, false, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "models", "fetch"], "Fetch and save models for a configured provider.", &["provider_id"], &[], true, false, &["api_key", "access_key", "secret_key"]),
                stdin_redacted(&["providers", "health-check"], "Run a provider health check.", &["provider_id", "model"], &[], false, false, &["api_key", "access_key", "secret_key"]),
            ]),
            domain("settings", &[
                no_input(&["settings", "get"], "Read backend settings.", false),
                stdin(&["settings", "patch"], "Patch backend settings.", &["language", "theme", "notification_enabled"], &["user_id"], true, false),
                no_input_redacted(&["settings", "client", "get"], "Read client preferences.", false, &["secrets"]),
                stdin_redacted(&["settings", "client", "put"], "Replace client preferences.", &["theme", "language", "font_size", "zoom"], &["user_id"], true, false, &["secrets"]),
            ]),
            domain("agents", &[
                no_input_redacted(&["agents", "list"], "List agent catalog and custom agents.", false, &["env"]),
                stdin_redacted(&["agents", "enable"], "Enable or disable an agent.", &["agent_id", "enabled"], &[], true, false, &["env"]),
                stdin_redacted(&["agents", "overrides", "get"], "Read agent overrides.", &["agent_id"], &[], false, false, &["env", "secret overrides"]),
                stdin_redacted(&["agents", "overrides", "set"], "Set agent overrides.", &["agent_id"], &[], true, false, &["env", "secret overrides"]),
                stdin_redacted(&["agents", "custom", "create"], "Create a custom agent.", &["name", "command"], &[], true, false, &["env"]),
                stdin_redacted(&["agents", "custom", "update"], "Update a custom agent.", &["agent_id", "name", "command"], &[], true, false, &["env"]),
                stdin_redacted(&["agents", "custom", "delete"], "Delete a custom agent.", &["agent_id"], &[], true, true, &["env"]),
                stdin_redacted(&["agents", "custom", "try-connect"], "Test a custom agent connection.", &["command"], &[], false, false, &["env"]),
            ]),
            domain("cron", &[
                no_input(&["cron", "jobs", "list"], "List cron jobs.", false),
                stdin(&["cron", "jobs", "get"], "Read one cron job.", &["job_id"], &[], false, false),
                stdin(&["cron", "jobs", "create"], "Create a cron job.", &["name", "schedule_type", "schedule", "timezone", "task_description"], &["conversation_id", "user_id"], true, false),
                stdin(&["cron", "jobs", "update"], "Update a cron job.", &["job_id"], &["conversation_id", "user_id"], true, false),
                stdin(&["cron", "jobs", "delete"], "Delete a cron job.", &["job_id"], &[], true, true),
                stdin(&["cron", "jobs", "run"], "Run a cron job immediately.", &["job_id"], &[], false, false),
                stdin(&["cron", "jobs", "skill", "get"], "Read cron job skill state.", &["job_id"], &[], false, false),
                stdin_redacted(&["cron", "jobs", "skill", "save"], "Save cron job skill content.", &["job_id", "content"], &[], true, false, &["content"]),
                stdin_redacted(&["cron", "jobs", "skill", "delete"], "Delete cron job skill content.", &["job_id"], &[], true, true, &["content"]),
                no_input(&["cron", "current", "list"], "List scheduled tasks for the current conversation.", false),
                stdin(&["cron", "current", "create"], "Create the scheduled task for the current conversation.", &["name", "schedule", "schedule_description", "message"], &["conversation_id"], true, false),
                stdin(&["cron", "current", "update"], "Update the scheduled task for the current conversation.", &["job_id"], &["conversation_id"], true, false),
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

fn no_input(path: &[&str], description: &str, readback: bool) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "none",
        stdin_fields: &[],
        selectors: &[],
        readback,
        destructive: false,
        redacted_fields: &[],
    })
}

fn no_input_redacted(path: &[&str], description: &str, readback: bool, redacted_fields: &[&str]) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "none",
        stdin_fields: &[],
        selectors: &[],
        readback,
        destructive: false,
        redacted_fields,
    })
}

fn stdin(
    path: &[&str],
    description: &str,
    stdin_fields: &[&str],
    selectors: &[&str],
    readback: bool,
    destructive: bool,
) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "stdin_json",
        stdin_fields,
        selectors,
        readback,
        destructive,
        redacted_fields: &[],
    })
}

fn stdin_redacted(
    path: &[&str],
    description: &str,
    stdin_fields: &[&str],
    selectors: &[&str],
    readback: bool,
    destructive: bool,
    redacted_fields: &[&str],
) -> Value {
    command(CommandDescriptor {
        path,
        description,
        input: "stdin_json",
        stdin_fields,
        selectors,
        readback,
        destructive,
        redacted_fields,
    })
}

struct CommandDescriptor<'a> {
    path: &'a [&'a str],
    description: &'a str,
    input: &'a str,
    stdin_fields: &'a [&'a str],
    selectors: &'a [&'a str],
    readback: bool,
    destructive: bool,
    redacted_fields: &'a [&'a str],
}

fn command(spec: CommandDescriptor<'_>) -> Value {
    let requires_context: &[&str] = if spec.path == ["capabilities"] {
        &[]
    } else {
        &RUNTIME_ENV
    };

    json!({
        "path": spec.path,
        "command": format!("config {}", spec.path.join(" ")),
        "description": spec.description,
        "input": spec.input,
        "stdin_fields": spec.stdin_fields,
        "selectors": spec.selectors,
        "readback": spec.readback,
        "destructive": spec.destructive,
        "requires_context": requires_context,
        "redacted_fields": spec.redacted_fields,
    })
}
