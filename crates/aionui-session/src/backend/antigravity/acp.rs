//! Antigravity (`agy acp`) stdio server helpers (`agy >= 1.5.1`).
//!
//! Primary-source evidence (`agy 1.5.8` live CLI + `agy changelog` + `agy acp --help`):
//! - `agy acp` speaks ACP protocolVersion `1` over newline-delimited JSON-RPC stdio
//!   (`--stdio` defaults to `true`).
//! - `session/resume` restores a persisted session without replaying historical
//!   `session/update` chunks (`agy changelog` `1.5.0`, live-verified on `1.5.8`).
//! - Sending a second `session/prompt` mid-turn steers the active turn cleanly,
//!   settling the superseded prompt RPC with `stopReason: "end_turn"` and finishing
//!   under the new prompt RPC (`agy changelog` `1.5.0`, live-verified on `1.5.8`).
//! - `agy acp` advertises `default`, `accept-edits`, and `plan` modes; AionUi's
//!   unattended `"yolo"` sentinel maps to `--dangerously-skip-permissions` at spawn
//!   plus wire mode `"accept-edits"` and reverse-RPC permission auto-approval.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_common::{CommandSpec, EnvVar};
use aionui_process::Spawner;

use super::argv::FULL_AUTO_SENTINEL;
use crate::backend::{AcpConnection, AcpDialectOptions, acp_capabilities_for_dialect};
use crate::capability::{Capabilities, ModeInfo};

const HTTP2_CLIENT_DISABLE_TOKEN: &str = "http2client=0";

fn has_http2client_token(raw: &str) -> bool {
    raw.split(',').map(str::trim).any(|part| {
        part.len() >= "http2client=".len() && part[.."http2client=".len()].eq_ignore_ascii_case("http2client=")
    })
}

fn find_env_mut<'a>(env: &'a mut [EnvVar], key: &str) -> Option<&'a mut EnvVar> {
    env.iter_mut().find(|entry| entry.name.eq_ignore_ascii_case(key))
}

/// Normalize `spawn_env` for an Antigravity (`agy`) child process.
///
/// 1. Pins `PWD` to `cwd` when non-blank so `agy` resolves workspace customizations
///    against the conversation directory instead of a stale parent `PWD`.
/// 2. Merges `GODEBUG=http2client=0` without clobbering any explicit `http2client=`
///    directive or existing `GODEBUG` tokens (`agy` is a Go binary whose long-lived
///    HTTP/2 streams over local TLS/HTTP proxies can drop with `unexpected EOF`).
/// 3. Supplies `PYTHONUTF8=1` and `PYTHONIOENCODING=utf-8` defaults when unset so
///    Python helper scripts invoked by `agy` on Windows consoles do not fail with
///    CP936/CP1252 `UnicodeEncodeError`.
pub fn prepare_antigravity_spawn_env(spawn_env: &mut Vec<EnvVar>, cwd: Option<&str>) {
    prepare_antigravity_spawn_env_with(spawn_env, cwd, |key| std::env::var(key).ok());
}

fn prepare_antigravity_spawn_env_with<F>(spawn_env: &mut Vec<EnvVar>, cwd: Option<&str>, ambient_lookup: F)
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = cwd.map(str::trim).filter(|s| !s.is_empty()) {
        spawn_env.retain(|entry| !entry.name.eq_ignore_ascii_case("PWD"));
        spawn_env.push(EnvVar {
            name: "PWD".to_owned(),
            value: dir.to_owned(),
        });
    }

    // Merge GODEBUG=http2client=0 non-clobberingly.
    if let Some(existing) = find_env_mut(spawn_env, "GODEBUG") {
        let trimmed = existing.value.trim();
        if trimmed.is_empty() {
            existing.value = HTTP2_CLIENT_DISABLE_TOKEN.to_owned();
        } else if !has_http2client_token(trimmed) {
            existing.value = format!("{trimmed},{HTTP2_CLIENT_DISABLE_TOKEN}");
        }
    } else {
        match ambient_lookup("GODEBUG") {
            Some(ambient) if has_http2client_token(&ambient) => {}
            Some(ambient) if !ambient.trim().is_empty() => {
                spawn_env.push(EnvVar {
                    name: "GODEBUG".to_owned(),
                    value: format!("{},{HTTP2_CLIENT_DISABLE_TOKEN}", ambient.trim()),
                });
            }
            _ => {
                spawn_env.push(EnvVar {
                    name: "GODEBUG".to_owned(),
                    value: HTTP2_CLIENT_DISABLE_TOKEN.to_owned(),
                });
            }
        }
    }

    for (key, default_val) in [("PYTHONUTF8", "1"), ("PYTHONIOENCODING", "utf-8")] {
        let present_in_spawn = spawn_env
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(key) && !e.value.trim().is_empty());
        if present_in_spawn {
            continue;
        }
        let present_in_ambient = ambient_lookup(key).is_some_and(|v| !v.trim().is_empty());
        if !present_in_ambient {
            spawn_env.retain(|e| !e.name.eq_ignore_ascii_case(key));
            spawn_env.push(EnvVar {
                name: key.to_owned(),
                value: default_val.to_owned(),
            });
        }
    }
}

/// Build argv for `agy acp` (`agy >= 1.5.1`).
///
/// Verified against `agy 1.5.8 acp --help`:
/// - `acp` starts the stdio JSON-RPC server (`--stdio` defaults to `true`).
/// - `--add-dir <workspace>` grants tool access to the conversation workspace.
/// - `--dangerously-skip-permissions` opens `agy`'s internal permission gate when
///   the conversation opens in unattended full-auto (`yolo`) mode.
/// - `extra_args` appends skill allow-list `--add-dir` flags last.
pub fn build_antigravity_acp_argv(workspace: Option<&str>, full_auto: bool, extra_args: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(4 + extra_args.len());
    args.push("acp".to_owned());
    if let Some(w) = workspace.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--add-dir".to_owned());
        args.push(w.to_owned());
    }
    if full_auto {
        args.push("--dangerously-skip-permissions".to_owned());
    }
    args.extend(extra_args.iter().cloned());
    args
}

/// Dialect configuration for `agy acp` (`>= 1.5.1`, verified live on `1.5.8`).
pub fn antigravity_acp_dialect() -> AcpDialectOptions {
    AcpDialectOptions {
        supports_prompt_steering: true,
        use_session_resume: true,
        host_full_auto_mode: Some(ModeInfo {
            id: FULL_AUTO_SENTINEL.to_owned(),
            name: "YOLO".to_owned(),
            description: None,
        }),
        full_auto_wire_mode: Some("accept-edits".to_owned()),
    }
}

/// Static capability snapshot for the `agy acp` backend.
pub fn antigravity_acp_capabilities() -> Capabilities {
    acp_capabilities_for_dialect(&antigravity_acp_dialect())
}

/// Construct an [`AcpConnection`] configured for `agy acp`.
///
/// Note: per-session `extra_args` (such as enabled skill `--add-dir` entries) and
/// `spawn_env` are merged from [`crate::backend::SessionConfig`] by
/// `AcpConnection::open_session` and `AcpSessionBackend::wake_handle`, so passing
/// `&[]` here avoids duplicating `extra_args` on spawn.
pub fn antigravity_acp_connection(
    spawner: Arc<dyn Spawner>,
    cli_program: Option<PathBuf>,
    workspace: Option<&str>,
    full_auto: bool,
) -> AcpConnection {
    let spec = CommandSpec {
        command: cli_program.unwrap_or_else(|| PathBuf::from("agy")),
        args: build_antigravity_acp_argv(workspace, full_auto, &[]),
        env: Vec::new(),
        cwd: workspace.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned),
    };
    AcpConnection::new(spawner, spec).with_dialect(antigravity_acp_dialect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_antigravity_acp_argv_handles_workspace_full_auto_and_extra_args() {
        let normal = build_antigravity_acp_argv(
            Some("G:/AgentWorkspace/work"),
            false,
            &["--add-dir".into(), "G:/skills/cron".into()],
        );
        assert_eq!(
            normal,
            vec![
                "acp",
                "--add-dir",
                "G:/AgentWorkspace/work",
                "--add-dir",
                "G:/skills/cron",
            ]
        );

        let yolo = build_antigravity_acp_argv(Some("G:/w"), true, &[]);
        assert_eq!(yolo, vec!["acp", "--add-dir", "G:/w", "--dangerously-skip-permissions"]);

        let blank_ws = build_antigravity_acp_argv(Some("   "), false, &[]);
        assert_eq!(blank_ws, vec!["acp"]);
    }

    #[test]
    fn prepare_antigravity_spawn_env_injects_defaults_without_clobbering_overrides() {
        let mut env = Vec::new();
        prepare_antigravity_spawn_env_with(&mut env, Some("/workspace/demo"), |_| None);
        assert!(env.iter().any(|e| e.name == "PWD" && e.value == "/workspace/demo"));
        assert!(env.iter().any(|e| e.name == "GODEBUG" && e.value == "http2client=0"));
        assert!(env.iter().any(|e| e.name == "PYTHONUTF8" && e.value == "1"));
        assert!(env.iter().any(|e| e.name == "PYTHONIOENCODING" && e.value == "utf-8"));

        // Explicit user overrides and existing GODEBUG tokens are preserved.
        let mut custom = vec![
            EnvVar {
                name: "PWD".into(),
                value: "/stale".into(),
            },
            EnvVar {
                name: "GODEBUG".into(),
                value: "netdns=go".into(),
            },
            EnvVar {
                name: "PYTHONUTF8".into(),
                value: "0".into(),
            },
        ];
        prepare_antigravity_spawn_env_with(&mut custom, Some("/fresh"), |k| {
            (k == "PYTHONIOENCODING").then(|| "gbk".to_owned())
        });
        assert!(custom.iter().any(|e| e.name == "PWD" && e.value == "/fresh"));
        assert!(
            custom
                .iter()
                .any(|e| e.name == "GODEBUG" && e.value == "netdns=go,http2client=0")
        );
        assert!(custom.iter().any(|e| e.name == "PYTHONUTF8" && e.value == "0"));
        assert!(
            !custom.iter().any(|e| e.name == "PYTHONIOENCODING"),
            "non-empty ambient PYTHONIOENCODING must not be overwritten"
        );

        // Explicit `http2client=1` in spawn_env or ambient env is respected verbatim.
        let mut explicit_h2 = vec![EnvVar {
            name: "GODEBUG".into(),
            value: "http2client=1".into(),
        }];
        prepare_antigravity_spawn_env_with(&mut explicit_h2, None, |_| None);
        assert!(
            explicit_h2
                .iter()
                .any(|e| e.name == "GODEBUG" && e.value == "http2client=1")
        );
    }

    #[test]
    fn antigravity_acp_capabilities_enable_midturn_steering_and_images() {
        let caps = antigravity_acp_capabilities();
        assert!(caps.supports_midturn_delivery);
        assert!(caps.supported_commands.steer);
        assert!(caps.prompt_blocks.image);
        assert!(caps.prompt_blocks.resource);
    }
}
