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
                "name": "session",
                "mode": "cross-session-messaging",
                "description": "Deliver a message to another one of this user's conversations, and list the conversations that can receive one.",
                "contract": "agent-facing-session-cli",
                "contract_command": "session capabilities",
                "invocation": "aioncore session capabilities",
                "runtime_required": ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID", "AIONUI_RUNTIME_TOKEN"],
                "runtime_free_commands": ["session capabilities"],
                "safety": {
                    "can_write": true,
                    "runtime_token_required_for_context_and_call": true,
                    "does_not_accept_identity_authority_from_stdin": true,
                    "per_user_feature_switch": "list and send-message answer feature_disabled while the user has cross-session messaging switched off; capabilities stays available because it reads no conversation data"
                }
            }
        ],
        "non_agent_subcommands": [
            {
                "name": "antigravity-hook",
                "description": "PreToolUse permission gate spawned by the Antigravity CLI (agy) over stdin/stdout, not invoked by agents."
            },
            {
                "name": "doctor",
                "description": "Human/developer self-check for agent backend availability."
            },
            {
                "name": "mcp-team-stdio",
                "description": "Internal team MCP stdio server."
            },
            {
                "name": "prepare-managed-resources",
                "description": "Packaging helper for managed runtime resources."
            },
            {
                "name": "secret",
                "description": "Operator CLI for the storage-encryption root key. Opens the data-dir database directly and exits."
            },
            {
                "name": "user",
                "description": "Operator CLI for local user accounts. Opens the data-dir database directly and exits; bootstrap path for self-hosted deployments."
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::cli::Cli;
    use crate::commands::{config_capabilities, diagnose_capabilities, session_capabilities, team_capabilities};

    /// `capabilities` is its own entrypoint — `data()` declares it under
    /// `entrypoint`, not as one of the domains it indexes.
    const SELF_DECLARED: [&str; 1] = ["capabilities"];

    /// Every name this index advertises, across both buckets.
    fn indexed_names() -> Vec<String> {
        let data = data();
        ["domains", "non_agent_subcommands"]
            .into_iter()
            .flat_map(|bucket| {
                data[bucket]
                    .as_array()
                    .unwrap_or_else(|| panic!("{bucket} should be an array"))
                    .iter()
                    .map(|entry| {
                        entry["name"]
                            .as_str()
                            .unwrap_or_else(|| panic!("{bucket} entry has no name: {entry}"))
                            .to_owned()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn cli_subcommand_names() -> Vec<String> {
        Cli::command()
            .get_subcommands()
            .map(|sub| sub.get_name().to_owned())
            .collect()
    }

    /// An agent that runs `aioncore capabilities` sees only what this index
    /// lists. A wired subcommand missing from both buckets is invisible — the
    /// exact failure that hid `session` and `antigravity-hook` after they
    /// shipped. Classifying every subcommand is what makes the index a
    /// complete answer instead of a partial one.
    #[test]
    fn every_cli_subcommand_is_classified_by_the_index() {
        let indexed = indexed_names();
        let unclassified: Vec<String> = cli_subcommand_names()
            .into_iter()
            .filter(|name| !SELF_DECLARED.contains(&name.as_str()))
            .filter(|name| !indexed.contains(name))
            .collect();
        assert!(
            unclassified.is_empty(),
            "these subcommands exist but appear in neither `domains` nor `non_agent_subcommands`: {unclassified:?}\n\
             add each one to the bucket it belongs to in cmd_capabilities::data()"
        );
    }

    /// The reverse direction: a renamed or removed subcommand leaves the index
    /// advertising a path that can only fail.
    #[test]
    fn every_indexed_name_is_a_wired_cli_subcommand() {
        let actual = cli_subcommand_names();
        let dangling: Vec<String> = indexed_names()
            .into_iter()
            .filter(|name| !actual.contains(name))
            .collect();
        assert!(
            dangling.is_empty(),
            "the index advertises these names, but no such subcommand is wired: {dangling:?}"
        );
    }

    /// Each domain entry points at a contract string that the domain's own
    /// `capabilities` declares. When they drift, the index sends agents to a
    /// contract name that nothing answers to.
    #[test]
    fn each_domain_entry_matches_the_contract_its_own_capabilities_declares() {
        let data = data();
        let domains = data["domains"].as_array().expect("domains should be an array");
        for (name, own) in [
            ("config", config_capabilities::data()),
            ("diagnose", diagnose_capabilities::data()),
            ("team", team_capabilities::data()),
            ("session", session_capabilities::data()),
        ] {
            let entry = domains
                .iter()
                .find(|domain| domain["name"] == json!(name))
                .unwrap_or_else(|| panic!("`{name}` is missing from the top-level domain index"));
            assert_eq!(
                entry["contract"], own["contract"],
                "`{name}` is indexed as {} but its own capabilities declares {}",
                entry["contract"], own["contract"]
            );
            assert_eq!(
                entry["contract_command"],
                json!(format!("{name} capabilities")),
                "`{name}` contract_command should name its own capabilities command"
            );
            assert_eq!(
                entry["invocation"],
                json!(format!("aioncore {name} capabilities")),
                "`{name}` invocation should be runnable verbatim"
            );
        }
    }
}
