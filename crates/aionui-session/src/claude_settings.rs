//! Resolve claude's settings-file default permission mode (ELECTRON-3R4, feature 015 F2).
//!
//! A conversation with no explicit mode choice used to spawn hard-coded
//! `--permission-mode default`, silently overriding the user's machine-level
//! `permissions.defaultMode` in the claude settings files — a behavior
//! regression against the legacy ACP path, whose adapter resolved that setting
//! (`@agentclientprotocol/claude-agent-acp` `settings.js`/`resolvePermissionMode`).
//! This module restores the settings-derived default WITHOUT giving up the
//! explicit flag: the caller resolves here, then still passes `--permission-mode
//! <resolved>` (fail-closed, deterministic, UI-visible).
//!
//! Scope is intentionally ONE field: `permissions.defaultMode`. Precedence and
//! alias table are copied verbatim from the official adapter (0.29.2/0.33.1) —
//! managed policy > `<workspace>/.claude/settings.local.json` >
//! `<workspace>/.claude/settings.json` > `$CLAUDE_CONFIG_DIR/settings.json`
//! (default `~/.claude`). The highest-precedence file that HAS the key decides;
//! an unreadable/malformed file is treated as absent (the adapter's read
//! failure behaves the same); a present-but-invalid value resolves to
//! `"default"` (the adapter's `resolvePermissionMode` fallback), it does NOT
//! fall through to a lower layer.

use std::path::{Path, PathBuf};

/// Which settings layer produced the resolved mode. `Fallback` = no layer had
/// `permissions.defaultMode` (or the winning layer's value was invalid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultModeSource {
    Managed,
    LocalProject,
    Project,
    User,
    Fallback,
}

/// The resolved spawn-time default mode. `mode` is always one of claude's
/// accepted `--permission-mode` wire values (the alias table maps onto them),
/// so it passes `is_valid_claude_permission_mode` by construction.
#[derive(Debug, Clone)]
pub struct ResolvedDefaultMode {
    pub mode: String,
    pub source: DefaultModeSource,
}

const FALLBACK_MODE: &str = "default";

/// Verbatim from the official adapter's `PERMISSION_MODE_ALIASES` (0.33.1):
/// lowercase alias → claude wire value. Input is trimmed + lowercased first
/// (the adapter does `trim().toLowerCase()`).
fn normalize_mode_alias(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some("auto"),
        "default" => Some("default"),
        "acceptedits" => Some("acceptEdits"),
        "dontask" => Some("dontAsk"),
        "plan" => Some("plan"),
        "bypasspermissions" | "bypass" => Some("bypassPermissions"),
        _ => None,
    }
}

/// Pure resolution over already-loaded settings layers, ordered highest
/// precedence first. Split from the file IO so the precedence/alias matrix is
/// unit-testable without a filesystem.
pub fn resolve_from_layers(layers: &[(DefaultModeSource, Option<serde_json::Value>)]) -> ResolvedDefaultMode {
    for (source, value) in layers {
        let Some(v) = value else { continue };
        let Some(dm) = v.get("permissions").and_then(|p| p.get("defaultMode")) else {
            continue;
        };
        // The highest-precedence PRESENT key decides. Mirror the adapter's
        // resolve-once semantics: a non-string or unknown value falls back to
        // "default" instead of consulting lower layers.
        let normalized = dm.as_str().and_then(normalize_mode_alias);
        return match normalized {
            Some(mode) => ResolvedDefaultMode {
                mode: mode.to_string(),
                source: *source,
            },
            None => {
                tracing::warn!(
                    source = ?source,
                    value = %dm,
                    "claude settings permissions.defaultMode is not a recognized mode; using \"default\""
                );
                ResolvedDefaultMode {
                    mode: FALLBACK_MODE.to_string(),
                    source: DefaultModeSource::Fallback,
                }
            }
        };
    }
    ResolvedDefaultMode {
        mode: FALLBACK_MODE.to_string(),
        source: DefaultModeSource::Fallback,
    }
}

/// Fully injectable variant (tests pass temp dirs). `workspace` scopes the two
/// project layers; `None` (no workspace) skips them.
pub fn resolve_claude_default_mode_at(
    workspace: Option<&Path>,
    user_config_dir: &Path,
    managed_settings_path: &Path,
) -> ResolvedDefaultMode {
    let layers = vec![
        (DefaultModeSource::Managed, read_settings_json(managed_settings_path)),
        (
            DefaultModeSource::LocalProject,
            workspace.and_then(|w| read_settings_json(&w.join(".claude").join("settings.local.json"))),
        ),
        (
            DefaultModeSource::Project,
            workspace.and_then(|w| read_settings_json(&w.join(".claude").join("settings.json"))),
        ),
        (
            DefaultModeSource::User,
            read_settings_json(&user_config_dir.join("settings.json")),
        ),
    ];
    resolve_from_layers(&layers)
}

/// Production entry: user layer honors `CLAUDE_CONFIG_DIR` (else `~/.claude`),
/// managed layer uses the per-OS policy path (verbatim from the adapter).
pub fn resolve_claude_default_mode(workspace: Option<&Path>) -> ResolvedDefaultMode {
    resolve_claude_default_mode_at(workspace, &claude_user_config_dir(), &managed_settings_path())
}

/// `$CLAUDE_CONFIG_DIR`, else `<home>/.claude` — the same lookup the CLI and
/// the official adapter use for user settings.
pub fn claude_user_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home_dir().join(".claude")
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var(var)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Enterprise managed-policy path per OS — verbatim from the adapter's
/// `settings.js` (macOS `/Library/Application Support/ClaudeCode/…`,
/// Windows `C:\Program Files\ClaudeCode\…`, else `/etc/claude-code/…`).
fn managed_settings_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    }
}

/// Read + parse one settings file. Missing file → `None` silently (the normal
/// case); unreadable/malformed → `None` with a WARN (treated as absent, the
/// layer falls through — same effective behavior as the adapter's failed read).
fn read_settings_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "claude settings file is not valid JSON; ignoring this layer"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layer(src: DefaultModeSource, mode: &str) -> (DefaultModeSource, Option<serde_json::Value>) {
        (src, Some(json!({ "permissions": { "defaultMode": mode } })))
    }

    #[test]
    fn no_layer_present_falls_back_to_default() {
        let r = resolve_from_layers(&[(DefaultModeSource::User, None)]);
        assert_eq!(r.mode, "default");
        assert_eq!(r.source, DefaultModeSource::Fallback);
    }

    #[test]
    fn user_bypass_resolves() {
        let r = resolve_from_layers(&[layer(DefaultModeSource::User, "bypassPermissions")]);
        assert_eq!(r.mode, "bypassPermissions");
        assert_eq!(r.source, DefaultModeSource::User);
    }

    #[test]
    fn precedence_managed_over_local_over_project_over_user() {
        let r = resolve_from_layers(&[
            layer(DefaultModeSource::Managed, "default"),
            layer(DefaultModeSource::LocalProject, "plan"),
            layer(DefaultModeSource::Project, "acceptEdits"),
            layer(DefaultModeSource::User, "bypassPermissions"),
        ]);
        assert_eq!((r.mode.as_str(), r.source), ("default", DefaultModeSource::Managed));

        let r = resolve_from_layers(&[
            (DefaultModeSource::Managed, None),
            layer(DefaultModeSource::LocalProject, "plan"),
            layer(DefaultModeSource::Project, "acceptEdits"),
            layer(DefaultModeSource::User, "bypassPermissions"),
        ]);
        assert_eq!((r.mode.as_str(), r.source), ("plan", DefaultModeSource::LocalProject));

        let r = resolve_from_layers(&[
            (DefaultModeSource::Managed, None),
            (DefaultModeSource::LocalProject, None),
            layer(DefaultModeSource::Project, "acceptEdits"),
            layer(DefaultModeSource::User, "bypassPermissions"),
        ]);
        assert_eq!((r.mode.as_str(), r.source), ("acceptEdits", DefaultModeSource::Project));
    }

    #[test]
    fn aliases_normalize_like_the_adapter() {
        for (raw, want) in [
            ("bypass", "bypassPermissions"),
            ("BypassPermissions", "bypassPermissions"),
            ("  acceptedits  ", "acceptEdits"),
            ("DontAsk", "dontAsk"),
            ("auto", "auto"),
        ] {
            let r = resolve_from_layers(&[layer(DefaultModeSource::User, raw)]);
            assert_eq!(r.mode, want, "alias {raw:?}");
        }
    }

    #[test]
    fn unknown_or_nonstring_value_falls_back_without_consulting_lower_layers() {
        let r = resolve_from_layers(&[
            layer(DefaultModeSource::Project, "yolo"),
            layer(DefaultModeSource::User, "bypassPermissions"),
        ]);
        assert_eq!((r.mode.as_str(), r.source), ("default", DefaultModeSource::Fallback));

        let r = resolve_from_layers(&[
            (
                DefaultModeSource::Project,
                Some(json!({ "permissions": { "defaultMode": 3 } })),
            ),
            layer(DefaultModeSource::User, "bypassPermissions"),
        ]);
        assert_eq!((r.mode.as_str(), r.source), ("default", DefaultModeSource::Fallback));
    }

    #[test]
    fn resolved_mode_always_passes_the_spawn_whitelist() {
        for raw in ["auto", "default", "acceptedits", "dontask", "plan", "bypass", "junk"] {
            let r = resolve_from_layers(&[layer(DefaultModeSource::User, raw)]);
            assert!(
                crate::is_valid_claude_permission_mode(&r.mode),
                "resolver output {:?} must be spawn-safe",
                r.mode
            );
        }
    }

    #[test]
    fn file_layers_read_from_disk_with_precedence_and_bad_json_falls_through() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        let cfg = tmp.path().join("cfg");
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let managed = tmp.path().join("managed-settings.json"); // absent

        // user says bypass
        std::fs::write(
            cfg.join("settings.json"),
            r#"{ "permissions": { "defaultMode": "bypassPermissions" } }"#,
        )
        .unwrap();
        let r = resolve_claude_default_mode_at(Some(&ws), &cfg, &managed);
        assert_eq!(
            (r.mode.as_str(), r.source),
            ("bypassPermissions", DefaultModeSource::User)
        );

        // project overrides user
        std::fs::write(
            ws.join(".claude").join("settings.json"),
            r#"{ "permissions": { "defaultMode": "plan" } }"#,
        )
        .unwrap();
        let r = resolve_claude_default_mode_at(Some(&ws), &cfg, &managed);
        assert_eq!((r.mode.as_str(), r.source), ("plan", DefaultModeSource::Project));

        // corrupt local layer is treated as absent → project still wins
        std::fs::write(ws.join(".claude").join("settings.local.json"), "{ not json").unwrap();
        let r = resolve_claude_default_mode_at(Some(&ws), &cfg, &managed);
        assert_eq!((r.mode.as_str(), r.source), ("plan", DefaultModeSource::Project));

        // no workspace → project layers skipped, user wins again
        let r = resolve_claude_default_mode_at(None, &cfg, &managed);
        assert_eq!(
            (r.mode.as_str(), r.source),
            ("bypassPermissions", DefaultModeSource::User)
        );
    }
}
