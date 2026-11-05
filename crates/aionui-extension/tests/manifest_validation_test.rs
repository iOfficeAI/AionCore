//! Integration tests for manifest validation (test-plan MV-1 through MV-4).
//!
//! These test the public API surface of `aionui_extension::manifest`.

use aionui_extension::{ExtensionError, ExtensionManifest, parse_manifest, validate_manifest};

// -- MV-1: valid manifest loads successfully --

#[test]
fn mv1_valid_manifest_parses_and_validates() {
    let json = serde_json::json!({
        "name": "my-cool-extension",
        "version": "1.0.0",
        "display_name": "My Cool Extension",
        "description": "A test extension",
        "contributes": {
            "skills": [{ "name": "test-skill" }]
        }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    let manifest = parse_manifest(&bytes).unwrap();
    assert_eq!(manifest.name, "my-cool-extension");
    assert_eq!(manifest.version, "1.0.0");
    assert!(manifest.contributes.is_some());
}

// -- MV-2: reserved name prefix rejected --

#[test]
fn mv2_reserved_prefix_aion_rejected() {
    let json = serde_json::json!({"name": "aion-my-ext", "version": "1.0.0"});
    let bytes = serde_json::to_vec(&json).unwrap();
    let err = parse_manifest(&bytes).unwrap_err();
    assert!(matches!(err, ExtensionError::ReservedNamePrefix { ref prefix, .. } if prefix == "aion-"));
}

#[test]
fn mv2_reserved_prefix_internal_rejected() {
    let json = serde_json::json!({"name": "internal-utils", "version": "1.0.0"});
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(parse_manifest(&bytes).is_err());
}

#[test]
fn mv2_reserved_prefix_builtin_rejected() {
    let json = serde_json::json!({"name": "builtin-theme", "version": "1.0.0"});
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(parse_manifest(&bytes).is_err());
}

#[test]
fn mv2_reserved_prefix_system_rejected() {
    let json = serde_json::json!({"name": "system-core", "version": "1.0.0"});
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(parse_manifest(&bytes).is_err());
}

// -- MV-3: missing required fields --

#[test]
fn mv3_missing_version_rejected() {
    let json = serde_json::json!({"name": "my-ext"});
    let bytes = serde_json::to_vec(&json).unwrap();
    // serde will fail because `version` is a required field
    assert!(parse_manifest(&bytes).is_err());
}

#[test]
fn mv3_missing_name_rejected() {
    let json = serde_json::json!({"version": "1.0.0"});
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(parse_manifest(&bytes).is_err());
}

// -- MV-4: invalid version rejected --

#[test]
fn mv4_invalid_version_rejected() {
    let json = serde_json::json!({"name": "my-ext", "version": "not-semver"});
    let bytes = serde_json::to_vec(&json).unwrap();
    let err = parse_manifest(&bytes).unwrap_err();
    assert!(matches!(err, ExtensionError::InvalidVersion { .. }));
}

#[test]
fn mv4_partial_version_rejected() {
    let json = serde_json::json!({"name": "my-ext", "version": "1.0"});
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(parse_manifest(&bytes).is_err());
}

// -- Edge cases --

#[test]
fn manifest_with_all_optional_fields() {
    let json = serde_json::json!({
        "name": "full-ext",
        "version": "2.0.0",
        "display_name": "Full Extension",
        "description": "Has everything",
        "author": "Test",
        "license": "MIT",
        "homepage": "https://example.com",
        "icon": "icon.png",
        "engine": { "aionui": "^1.0.0" },
        "api_version": "1.0.0",
        "dependencies": { "other-ext": "^1.0.0" },
        "entry_point": "main.js",
        "permissions": { "storage": true, "events": true },
        "contributes": {},
        "lifecycle": {
            "on_install": "scripts/install.sh",
            "on_activate": "scripts/activate.sh"
        },
        "i18n": { "locales": ["en", "zh-CN"] }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    let manifest = parse_manifest(&bytes).unwrap();
    assert_eq!(manifest.display_name.as_deref(), Some("Full Extension"));
    assert!(manifest.engine.is_some());
    assert!(manifest.lifecycle.is_some());
    assert!(manifest.i18n.is_some());
}

#[test]
fn validate_manifest_directly() {
    let manifest = ExtensionManifest {
        name: "valid-ext".into(),
        version: "1.0.0-beta.1".into(),
        display_name: None,
        description: None,
        author: None,
        license: None,
        homepage: None,
        icon: None,
        engine: None,
        api_version: None,
        dependencies: Default::default(),
        entry_point: None,
        permissions: None,
        contributes: None,
        lifecycle: None,
        i18n: None,
    };
    assert!(validate_manifest(&manifest).is_ok());
}

// ---------------------------------------------------------------------------
// Plugin Contract v1 contribution points
// ---------------------------------------------------------------------------

#[test]
fn contract_v1_parses_new_contribution_points_camel_case() {
    let json = r#"{
        "name": "contract-demo",
        "version": "1.0.0",
        "contributes": {
            "commands": [
                { "id": "demo.hello", "title": "Hello", "keybinding": "ctrl+h" }
            ],
            "panels": [
                { "id": "demo.panel", "title": "Demo", "entryPoint": "panels/main.html" }
            ],
            "workPanelViews": [
                { "id": "demo.work", "title": "Work", "entryPoint": "views/work.html", "placement": "secondary" }
            ],
            "agentTools": [
                {
                    "id": "demo.summarize",
                    "name": "Summarize",
                    "description": "Read-only",
                    "planSafeActions": true,
                    "handler": "tools/summarize.js"
                },
                {
                    "id": "demo.write",
                    "name": "Write",
                    "planSafeActions": false
                }
            ],
            "backgroundServices": [
                { "id": "demo.sync", "entryPoint": "services/sync.js", "description": "Sync" }
            ]
        }
    }"#;

    let manifest = parse_manifest(json.as_bytes()).expect("parse");
    let c = manifest.contributes.expect("contributes");
    assert_eq!(c.commands.len(), 1);
    assert_eq!(c.commands[0].id, "demo.hello");
    assert_eq!(c.commands[0].keybinding.as_deref(), Some("ctrl+h"));
    assert_eq!(c.panels.len(), 1);
    assert_eq!(c.panels[0].entry_point, "panels/main.html");
    assert_eq!(c.work_panel_views.len(), 1);
    assert_eq!(c.work_panel_views[0].placement.as_deref(), Some("secondary"));
    assert_eq!(c.agent_tools.len(), 2);
    assert!(c.agent_tools[0].plan_safe_actions);
    assert!(!c.agent_tools[1].plan_safe_actions);
    assert_eq!(c.background_services.len(), 1);
    assert_eq!(c.background_services[0].entry_point, "services/sync.js");
}

#[test]
fn contract_v1_agent_tool_defaults_plan_safe_actions_to_false() {
    let json = r#"{
        "name": "contract-demo",
        "version": "1.0.0",
        "contributes": {
            "agent_tools": [
                { "id": "demo.x", "name": "X" }
            ]
        }
    }"#;
    let manifest = parse_manifest(json.as_bytes()).expect("parse");
    let tool = &manifest.contributes.unwrap().agent_tools[0];
    assert!(!tool.plan_safe_actions);
}

#[test]
fn contract_v1_unknown_contribute_keys_are_ignored() {
    let json = r#"{
        "name": "contract-demo",
        "version": "1.0.0",
        "contributes": {
            "futureThing": [{ "id": "x" }],
            "commands": [{ "id": "demo.ok", "title": "Ok" }]
        }
    }"#;
    let manifest = parse_manifest(json.as_bytes()).expect("parse");
    let c = manifest.contributes.unwrap();
    assert_eq!(c.commands.len(), 1);
}
