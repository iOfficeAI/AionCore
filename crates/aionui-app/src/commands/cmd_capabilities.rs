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
                "name": "skills",
                "mode": "read-only",
                "description": "Read the skills enabled in THIS conversation: list them, get a skill's full body plus its absolute directory, and read its supplementary files.",
                "contract": "agent-facing-skills-cli",
                "contract_command": "skills capabilities",
                "invocation": "aioncore skills capabilities",
                "runtime_required": ["AIONUI_BASE_URL", "AIONUI_CONVERSATION_ID", "AIONUI_USER_ID", "AIONUI_RUNTIME_TOKEN"],
                "runtime_free_commands": ["skills capabilities"],
                "safety": {
                    "can_write": false,
                    "read_only": true,
                    "scoped_to_conversation_snapshot": true
                }
            }
        ],
        "non_agent_subcommands": [
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
    use super::*;

    fn domain_names() -> Vec<String> {
        data()["domains"]
            .as_array()
            .expect("domains must be an array")
            .iter()
            .map(|domain| domain["name"].as_str().expect("every domain has a name").to_owned())
            .collect()
    }

    /// The `skills` domain must be discoverable from the top-level index.
    ///
    /// This is guarding against a defect that already happened once: `session`
    /// shipped a complete `session capabilities` contract but was never listed
    /// here, so an agent running `aioncore capabilities` had no way to learn the
    /// domain existed. Channel A depends on exactly that discovery step — an
    /// unlisted `skills` domain would silently push every agent onto the
    /// `[LOAD_SKILL]` fallback.
    ///
    /// NB: `session` is deliberately not asserted here. Registering it is a
    /// separate in-flight change touching this same array; if that change is
    /// dropped, the gap it fixes is still open and this comment is the pointer.
    #[test]
    fn the_domain_index_lists_the_skills_domain() {
        assert!(
            domain_names().iter().any(|name| name == "skills"),
            "skills is missing from the domain index: {:?}",
            domain_names()
        );
    }

    /// Every listed domain must tell the agent how to fetch its own contract.
    /// A domain in the index with no `contract_command` is discoverable but
    /// unusable, which is the same dead end from the other direction.
    #[test]
    fn every_listed_domain_advertises_a_contract_command() {
        for domain in data()["domains"].as_array().unwrap() {
            let name = domain["name"].as_str().unwrap();
            let contract = domain["contract_command"].as_str().unwrap_or_default();
            assert!(
                !contract.is_empty(),
                "domain {name:?} is listed without a contract_command"
            );
            let invocation = domain["invocation"].as_str().unwrap_or_default();
            assert!(
                invocation.contains(contract),
                "domain {name:?} invocation {invocation:?} must invoke its own contract_command {contract:?}"
            );
        }
    }

    /// A read-only domain must not advertise write authority: the agent reads
    /// `safety.can_write` to decide whether a command is safe to try.
    #[test]
    fn the_skills_domain_is_declared_read_only() {
        let skills = data()["domains"]
            .as_array()
            .unwrap()
            .iter()
            .find(|domain| domain["name"] == "skills")
            .expect("skills domain")
            .clone();
        assert_eq!(skills["mode"], "read-only");
        assert_eq!(skills["safety"]["can_write"], false);
        assert_eq!(skills["safety"]["scoped_to_conversation_snapshot"], true);
    }
}
