use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

/// 033 retires the keys the Registry-sync workflow wrote that never meant what
/// they looked like: `team_capable_override` hard-vetoed team mode with no way to
/// lift it (builtin rows reject metadata edits), and `supports_team: false` read
/// like a denial while being a no-op inside an OR.
#[tokio::test]
async fn retired_team_policy_keys_are_stripped_from_every_seeded_policy() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let rows = repo.list_all().await.unwrap();
    assert!(!rows.is_empty(), "seeded agent metadata is present");

    for row in rows {
        let Some(policy) = row.behavior_policy.as_deref() else {
            continue;
        };
        let policy: serde_json::Value = serde_json::from_str(policy).unwrap();
        let backend = row.backend.as_deref().unwrap_or("<no backend>");
        assert!(
            policy.get("team_capable_override").is_none(),
            "{backend} still carries the retired team_capable_override"
        );
        assert_ne!(
            policy.get("supports_team"),
            Some(&serde_json::Value::Bool(false)),
            "{backend} still carries a no-op supports_team: false"
        );
    }
}

/// The known-good whitelist (migration 014) survives 033. These rows are
/// load-bearing: on a fresh install claude/codex/gemini have NULL capabilities
/// until their first handshake, and aionrs has a NULL backend the capability
/// inference cannot judge at all — without the flag they would not be selectable.
#[tokio::test]
async fn known_good_team_whitelist_survives() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    for backend in ["claude", "codex", "gemini", "codebuddy"] {
        let row = repo
            .find_builtin_by_backend(backend)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{backend} is seeded"));
        let policy: serde_json::Value = serde_json::from_str(row.behavior_policy.as_deref().unwrap()).unwrap();
        assert_eq!(
            policy.get("supports_team"),
            Some(&serde_json::Value::Bool(true)),
            "{backend} keeps its known-good team whitelist entry"
        );
    }

    let aionrs = repo
        .list_all()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.agent_type == "aionrs")
        .expect("aionrs row is seeded");
    let policy: serde_json::Value = serde_json::from_str(aionrs.behavior_policy.as_deref().unwrap()).unwrap();
    assert_eq!(policy.get("supports_team"), Some(&serde_json::Value::Bool(true)));
}

/// `auth_methods` is the other half of the same omission: migration 003 pre-filled
/// it next to `agent_capabilities` so the UI can offer a sign-in before the agent
/// has ever started, and 023 (pi) did too, but 025/029/031 skipped it. Four agents
/// advertise none at all, and junie's embed the probe host's home directory, so
/// those five stay NULL by design rather than carry a doctored blob.
#[tokio::test]
async fn probed_registry_agents_carry_seeded_auth_methods() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let seeded = [
        "autohand",
        "deepagents",
        "dirac",
        "glm-acp-agent",
        "grok",
        "kilo",
        "mimo-code",
        "nova",
        "omp",
        "sigit",
        "amp-acp",
        "corust-agent",
        "devin",
        "harn",
        "stakpak",
    ];
    for backend in seeded {
        let row = repo
            .find_builtin_by_backend(backend)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{backend} is seeded"));
        let raw = row
            .auth_methods
            .as_deref()
            .unwrap_or_else(|| panic!("{backend} carries seeded auth_methods"));
        let methods: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(
            methods.as_array().is_some_and(|m| !m.is_empty()),
            "{backend} auth_methods is a non-empty array"
        );
        // A seeded blob must never carry the probe host's install layout.
        assert!(!raw.contains("/Users/"), "{backend} auth_methods leaks a host path");
        assert!(!raw.contains("/home/"), "{backend} auth_methods leaks a host path");
    }

    for backend in ["cortex-code", "dimcode", "poolside", "vtcode", "junie"] {
        let row = repo
            .find_builtin_by_backend(backend)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{backend} is seeded"));
        assert!(
            row.auth_methods.is_none(),
            "{backend} advertises no host-independent auth methods and stays NULL"
        );
    }
}

/// 033 also seeds the handshake capabilities the Registry-sync probe captured
/// from ACP `initialize` but never persisted, so Team can pick a transport before
/// the agent has ever connected. Values are LIVE-probed (2026-07-30) at the npx
/// versions pinned in acp-registry-npx-lock.json, and against the installed
/// product CLIs for binary distributions.
#[tokio::test]
async fn probed_registry_agents_carry_seeded_mcp_capabilities() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    // (backend, http, sse) — `None` means the agent advertises no usable
    // mcp_capabilities (absent or empty object), which keeps Team on the CLI
    // transport for it. Covers EVERY agent added by migrations 025/029/031.
    let cases: [(&str, Option<(bool, bool)>); 20] = [
        // npx distributions (025 + 029 + 031)
        ("autohand", Some((true, true))),
        ("deepagents", Some((false, false))),
        ("dimcode", Some((true, false))),
        ("dirac", None),
        ("glm-acp-agent", Some((true, false))),
        ("grok", Some((true, true))),
        ("kilo", Some((true, true))),
        ("mimo-code", Some((true, true))),
        ("nova", Some((true, true))),
        ("sigit", Some((false, false))),
        // direct CLI launch (031 seeded it on npx; 039 moved it off the bridge)
        ("omp", Some((true, true))),
        // binary distributions (025)
        ("amp-acp", Some((true, true))),
        ("cortex-code", None),
        ("corust-agent", Some((false, false))),
        ("devin", Some((false, false))),
        ("harn", Some((true, true))),
        ("junie", Some((true, true))),
        ("poolside", None),
        ("stakpak", Some((true, true))),
        ("vtcode", Some((true, false))),
    ];

    for (backend, expected) in cases {
        let row = repo
            .find_builtin_by_backend(backend)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{backend} is seeded"));
        let capabilities: serde_json::Value =
            serde_json::from_str(row.agent_capabilities.as_deref().unwrap_or_else(|| {
                panic!("{backend} carries seeded agent_capabilities");
            }))
            .unwrap();

        match expected {
            Some((http, sse)) => {
                let mcp = capabilities
                    .get("mcp_capabilities")
                    .unwrap_or_else(|| panic!("{backend} advertises mcp_capabilities"));
                assert_eq!(
                    mcp.get("http").and_then(serde_json::Value::as_bool),
                    Some(http),
                    "{backend} http"
                );
                assert_eq!(
                    mcp.get("sse").and_then(serde_json::Value::as_bool).unwrap_or(false),
                    sse,
                    "{backend} sse"
                );
            }
            // Either the object is absent (cortex-code, dirac) or present but
            // empty (poolside); both mean no usable transport was advertised.
            None => {
                let advertises_transport = capabilities.get("mcp_capabilities").is_some_and(|mcp| {
                    ["http", "sse"]
                        .iter()
                        .any(|k| mcp.get(k).and_then(serde_json::Value::as_bool) == Some(true))
                });
                assert!(!advertises_transport, "{backend} advertises no usable MCP transport");
            }
        }
    }
}
