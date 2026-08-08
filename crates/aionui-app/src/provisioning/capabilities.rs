//! Capability advertisement for `aioncore provision capabilities`.

use serde_json::{Value, json};

use aionui_api_types::{PROVISION_CONTRACT, PROVISION_PROTOCOL_VERSION, PROVISION_SCHEMA_VERSION, ProvisionScope};

/// Agent/provisioner-readable contract document for the provision domain.
pub fn capability_contract() -> Value {
    json!({
        "schema_version": PROVISION_SCHEMA_VERSION,
        "protocol_version": PROVISION_PROTOCOL_VERSION,
        "contract": PROVISION_CONTRACT,
        "stability": "experimental",
        "entrypoint": "aioncore provision capabilities",
        "purpose": "Conversation-independent trusted local provisioning for adopted principals (A0 assistants/MCP/skills, A1 team_definition).",
        "parent_issues": {
            "a0": "iOfficeAI/AionCore#795",
            "a1": "iOfficeAI/AionCore#798",
            "program": "sparkfn/pc-client#1082"
        },
        "discovery": {
            "caller_port_required": false,
            "method": "data_dir_endpoint_file",
            "path": "runtime/local-provision-endpoint.json",
            "resolved_via": ["--data-dir", "default data directory"],
            "forbidden": [
                "caller-provided port as authority",
                "port scanning",
                "cookie or CSRF extraction",
                "conversation runtime token reuse",
                "direct SQLite or filesystem mutation",
                "--local / system_default_user fallback"
            ]
        },
        "authorization": {
            "model": "short_lived_scoped_grant",
            "conversation_independent": true,
            "subject_selection": "installation_attested_only",
            "scopes": scopes_document(),
            "scope_separation": "possession of one scope never authorizes another"
        },
        "attestation_fields": [
            "installation_id",
            "profile_id",
            "subject",
            "protocol_version",
            "aioncore_version",
            "aionui_version",
            "identity_mode",
            "backend.state"
        ],
        "commands": [
            command(&["capabilities"], "Print this contract.", false, &[]),
            command(&["discover"], "Resolve installation endpoint without caller port.", false, &[]),
            command(&["attest"], "Read attested installation/profile/subject before any write.", false, &[]),
            command(
                &["authorize"],
                "Mint a short-lived least-privilege grant for requested scopes.",
                true,
                &["protocol_version", "installation_id", "profile_id", "scopes"]
            ),
            command(
                &["assistants", "reconcile"],
                "Conditional whole-assistant reconcile with expected_revision and exact readback.",
                true,
                &["auth", "logical_id", "desired"]
            ),
            command(&["assistants", "get"], "Read managed assistant by logical id.", true, &["auth", "logical_id"]),
            command(&["assistants", "delete"], "Delete managed assistant when not Team-referenced.", true, &["auth", "logical_id"]),
            command(
                &["mcp", "reconcile"],
                "Conditional MCP reconcile; preserves foreign/user MCP resources.",
                true,
                &["auth", "logical_id", "desired"]
            ),
            command(&["mcp", "get"], "Read managed MCP by logical id.", true, &["auth", "logical_id"]),
            command(&["mcp", "delete"], "Delete managed MCP when unreferenced.", true, &["auth", "logical_id"]),
            command(
                &["skills", "reconcile"],
                "Conditional skill registration/activation.",
                true,
                &["auth", "logical_id", "desired"]
            ),
            command(&["skills", "get"], "Read managed skill by logical id.", true, &["auth", "logical_id"]),
            command(&["skills", "delete"], "Delete managed skill when unreferenced.", true, &["auth", "logical_id"]),
            command(
                &["teams", "create"],
                "Create Team definition (A1); no runtime start.",
                true,
                &["auth", "logical_id", "desired"]
            ),
            command(
                &["teams", "update"],
                "Conditional whole-Team definition update with exact readback.",
                true,
                &["auth", "logical_id", "desired"]
            ),
            command(&["teams", "get"], "Read Team definition by logical id.", true, &["auth", "logical_id"]),
            command(
                &["teams", "delete"],
                "Delete Team definition with exact resource disposition.",
                true,
                &["auth", "logical_id"]
            ),
            command(&["revoke"], "Revoke a grant (account-switch / explicit revoke).", true, &["grant_id"])
        ],
        "stable_errors": [
            "PROVISION_WRONG_PROFILE",
            "PROVISION_AUTHORITY_EXPIRED",
            "PROVISION_AUTHORITY_REVOKED",
            "PROVISION_CONCURRENT_CONFLICT",
            "PROVISION_RUNTIME_BUSY",
            "PROVISION_SCOPE_MISSING",
            "PROVISION_BACKEND_CLOSED",
            "PROVISION_SUBJECT_MISMATCH",
            "PROVISION_TEAM_REFERENCED_ASSISTANT",
            "PROVISION_INVALID_LEADER",
            "PROVISION_INVALID_MEMBER_KEY"
        ],
        "managed_provenance": {
            "fields": ["logical_id", "native_id", "revision", "managed_by", "created_at_ms", "updated_at_ms"],
            "survives_restart": "required (persistence wiring tracked for A0-AC5 / A1-AC10)"
        },
        "notes": {
            "skeleton": "Mutation path is an in-process protocol engine skeleton with exact shapes and fail-closed authority. Durable adopted-principal persistence and native macOS/Windows black-box qualification remain open ACs.",
            "does_not_authorize_fleet_release": true
        }
    })
}

fn scopes_document() -> Value {
    json!([
        {
            "name": ProvisionScope::AssistantManagement.as_str(),
            "program": "A0",
            "description": "Assistant definition/rule/default/placement management."
        },
        {
            "name": ProvisionScope::McpConfiguration.as_str(),
            "program": "A0",
            "description": "MCP registration/configuration management."
        },
        {
            "name": ProvisionScope::SkillRegistration.as_str(),
            "program": "A0",
            "description": "Skill registration/activation management."
        },
        {
            "name": ProvisionScope::TeamDefinition.as_str(),
            "program": "A1",
            "description": "Unattended Team definition create/read/update/delete without runtime ops."
        }
    ])
}

fn command(path: &[&str], description: &str, stdin_json: bool, required_fields: &[&str]) -> Value {
    json!({
        "path": path,
        "invocation": format!("aioncore provision {}", path.join(" ")),
        "description": description,
        "stdin_json": stdin_json,
        "required_fields": required_fields
    })
}
