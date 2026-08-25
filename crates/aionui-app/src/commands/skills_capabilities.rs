//! Self-describing contract for `aioncore skills`.
//!
//! Static data touching no conversation state, so it works with no runtime env
//! at all — which matters because this is the command an agent runs first to
//! learn the domain exists.

use serde_json::{Value, json};

pub(crate) fn data() -> Value {
    json!({
        "schema_version": 1,
        "contract": "agent-facing-skills-cli",
        "stability": "stable",
        "entrypoint": "aioncore skills capabilities",
        "purpose": "Read the skills enabled in THIS conversation: list them, get a skill's \
                    full body plus its absolute directory, and read its supplementary files.",
        "relationship_to_config_skills": "Distinct domain. `config skills *` is the read-write \
             management surface over EVERY importable skill on the installation. This domain is \
             read-only and scoped to this conversation's enabled set; the two are not \
             interchangeable and this one can never write.",
        "output": {
            "stdout": "JSON envelope",
            "stderr": "single stable ..._FAILED error line on failure",
            "success_shape": {
                "success": true,
                "data": {},
                "meta": { "schema_version": 1, "command": "skills list" }
            }
        },
        "runtime_context": {
            "environment": [
                "AIONUI_BASE_URL",
                "AIONUI_CONVERSATION_ID",
                "AIONUI_USER_ID",
                "AIONUI_RUNTIME_TOKEN"
            ],
            "runtime_free_commands": ["skills capabilities"]
        },
        "input": { "default_mode": "stdin_json", "business_flags": false },
        "commands": [
            {
                "name": "list",
                "invocation": "aioncore skills list",
                "stdin": {},
                "returns": { "skills": [{ "name": "string", "description": "string" }] },
                "notes": "Only the skills this conversation enabled. Sorted by name."
            },
            {
                "name": "show",
                "invocation": "aioncore skills show",
                "stdin": { "name": "string (required)" },
                "returns": { "name": "string", "body": "string", "path": "absolute directory" },
                "notes": "`body` has the frontmatter stripped and is identical to what the \
                          `[LOAD_SKILL: name]` protocol injects. Resolve every relative path \
                          inside `body` against `path`, NOT against your working directory."
            },
            {
                "name": "cat",
                "invocation": "aioncore skills cat",
                "stdin": { "path": "string (required), of the form <skill-name>/<relative-path>" },
                "returns": { "name": "string", "path": "string", "content": "string" },
                "notes": "Reads a supplementary file such as references/notes.md. Confined to \
                          the skill's own directory; `..`, absolute paths, and symlinks leaving \
                          the directory are rejected."
            }
        ],
        "errors": {
            "skill_not_enabled": "The skill exists but is not enabled in this conversation. Do \
                 not retry with variations; use `skills list` to see what is available.",
            "skill_not_found": "Enabled here, but no source directory resolves. A broken \
                 install rather than a permission problem.",
            "invalid_path": "The requested path left the skill directory.",
            "runtime_auth_failed": "Missing or wrong runtime token / conversation id."
        },
        "safety": {
            "can_write": false,
            "read_only": true,
            "scoped_to_conversation_snapshot": true
        },
        "fallback": "If you cannot execute commands at all (plan mode, read-only, scheduled \
             runs), output `[LOAD_SKILL: <name>]` in your reply instead; the harness loads the \
             body and feeds it back on the next turn."
    })
}
