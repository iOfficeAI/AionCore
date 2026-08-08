//! Top-level agent-readable capability index for the `aioncore` binary.

use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

const RUNTIME_ENV: [&str; 4] = [
    "AIONUI_HELPER_BIN",
    "AIONUI_BASE_URL",
    "AIONUI_CONVERSATION_ID",
    "AIONUI_USER_ID",
];

pub(crate) fn run_capabilities() -> ExitCode {
    match print_envelope(data()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => {
            eprintln!("CAPABILITIES_STDOUT_WRITE_FAILED command=\"capabilities\": failed to write JSON output");
            ExitCode::from(1)
        }
    }
}

fn data() -> Value {
    json!({
        "schema_version": 1,
        "contract": "agent-facing-aioncore-cli",
        "stability": "stable",
        "entrypoint": "aioncore capabilities",
        "purpose": "Top-level index for agent-facing AionCore CLI domains.",
        "output": {
            "stdout": "JSON envelope",
            "stderr": "single stable ..._FAILED error line when output cannot be written",
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
            "selectors": {
                "conversation_id": {
                    "current": "resolve from AIONUI_CONVERSATION_ID"
                },
                "assistant_id": {
                    "current": "resolve via current conversation"
                },
                "user_id": {
                    "current": "resolve from AIONUI_USER_ID"
                }
            }
        },
        "input": {
            "default_mode": "stdin_json",
            "business_flags": false,
            "domain_contracts": "Use each domain's capabilities command for exact stdin fields and safety metadata."
        },
        "domains": [
            {
                "name": "config",
                "mode": "read-write",
                "description": "Manage AionUi configuration: assistants, assistant rules, skills, MCP servers, providers, settings, agents, and scheduled tasks.",
                "contract": "agent-facing-config-cli",
                "contract_command": "config capabilities",
                "invocation": "aioncore config capabilities",
                "runtime_required": ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID"],
                "safety": {
                    "can_write": true,
                    "read_before_write": true,
                    "redacted_by_default": true
                }
            },
            {
                "name": "diagnose",
                "mode": "read-only",
                "description": "Diagnose a running AionUi installation: backend health, conversations, provider health, MCP, cron, teams, logs, and controlled GET reads.",
                "contract": "agent-facing-diagnose-cli",
                "contract_command": "diagnose capabilities",
                "invocation": "aioncore diagnose capabilities",
                "runtime_required": ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID"],
                "optional_runtime": ["AIONUI_LOG_DIR"],
                "safety": {
                    "can_write": false,
                    "read_only": true,
                    "redacted_by_default": true,
                    "escape_hatch": "diagnose http get"
                }
            },
            {
                "name": "team",
                "mode": "team-collaboration",
                "description": "Agent-facing Team collaboration CLI fallback for agents without MCP injection.",
                "contract": "agent-facing-team-cli",
                "contract_command": "team capabilities",
                "invocation": "aioncore team capabilities",
                "runtime_required": ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID", "AIONUI_RUNTIME_TOKEN"],
                "runtime_free_commands": ["team capabilities", "team help"],
                "safety": {
                    "can_write": true,
                    "runtime_token_required_for_context_and_call": true,
                    "does_not_accept_identity_authority_from_stdin": true
                }
            },
            {
                "name": "provision",
                "mode": "trusted-local-provisioning",
                "description": "Conversation-independent trusted local provisioning for adopted principals (A0 assistants/MCP/skills, A1 team_definition).",
                "contract": "trusted-local-provisioning",
                "contract_command": "provision capabilities",
                "invocation": "aioncore provision capabilities",
                "runtime_required": [],
                "runtime_free_commands": ["provision capabilities", "provision discover", "provision attest"],
                "discovery": {
                    "caller_port_required": false,
                    "method": "data_dir_endpoint_file",
                    "resolved_via": ["--data-dir"]
                },
                "safety": {
                    "can_write": true,
                    "conversation_independent": true,
                    "does_not_use_runtime_token": true,
                    "does_not_use_local_default_identity": true,
                    "scopes": [
                        "assistant_management",
                        "mcp_configuration",
                        "skill_registration",
                        "team_definition"
                    ]
                }
            }
        ],
        "non_agent_subcommands": [
            {
                "name": "doctor",
                "description": "Human/developer self-check for agent backend availability."
            },
            {
                "name": "mcp-bridge",
                "description": "Internal stdio to TCP bridge for team MCP."
            },
            {
                "name": "mcp-team-stdio",
                "description": "Internal team MCP stdio server."
            },
            {
                "name": "prepare-managed-resources",
                "description": "Packaging helper for managed runtime resources."
            }
        ]
    })
}

fn print_envelope(data: Value) -> Result<(), ()> {
    let rendered = serde_json::to_string_pretty(&json!({
        "success": true,
        "data": data,
        "meta": {
            "schema_version": 1
        }
    }))
    .map_err(|_| ())?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| ())
}
