//! Version-gated claude CLI flags.
//!
//! Some flags we want are only understood by newer claude binaries, and the CLI
//! rejects an unknown option at ARGUMENT-PARSE time (`error: unknown option
//! '--x'`, verified live on 2.1.0) — it does not ignore it. Passing a flag blindly
//! therefore turns "a display feature is missing" into "every session dies during
//! spawn", so anything not universally supported has to be gated on the binary we
//! are actually about to run.
//!
//! The packaged app runs the bundled CLI pinned by `CLAUDE_CLI_VERSION`, but dev
//! builds, `Download` mode, and a missing bundle all fall back to whatever
//! `claude` is on PATH (`resolve_bundled_cli` → `None`), whose version we do not
//! control. Hence a real probe rather than a compile-time assumption.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aionui_runtime::Builder;

/// Budget for the one-shot `--version` probe. Generous relative to the ~130ms a
/// warm claude takes; a timeout is treated as "unsupported" (fail safe: we drop
/// the flag rather than risk an unspawnable session).
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Lowest claude version VERIFIED to accept `--thinking-display`.
///
/// This is a verified floor, NOT the introduction version: 2.1.0 hard-errors with
/// `unknown option '--thinking-display'`, while 2.1.191 / 2.1.215 (our bundled
/// pin) / 2.1.218 / 2.1.220 all accept it. Everything between 2.1.0 and 2.1.191 is
/// untested and is treated as unsupported, because the failure mode of guessing
/// wrong is a dead session rather than a missing thinking card.
const THINKING_DISPLAY_MIN_VERSION: (u32, u32, u32) = (2, 1, 191);

/// Probed support keyed by the resolved binary path, so the `--version` spawn
/// happens once per binary per process rather than once per session.
fn cache() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parse the leading `X.Y.Z` out of `claude --version` output
/// (e.g. `"2.1.220 (Claude Code)"`). Returns `None` on any shape we do not
/// recognise, which the caller treats as unsupported.
fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let token = raw.split_whitespace().next()?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // A trailing pre-release/build suffix ("191-beta") still yields the patch.
    let patch_token = parts.next()?;
    let patch = patch_token.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `program` accepts `--thinking-display`. Probes `--version` once per
/// binary and caches the verdict; any failure (spawn error, timeout, exit != 0,
/// unparseable output) resolves to `false`.
pub(crate) async fn supports_thinking_display(program: &Path) -> bool {
    if let Some(hit) = cache().lock().expect("claude flag cache poisoned").get(program) {
        return *hit;
    }

    let mut command = Builder::clean_cli(program);
    command.arg("--version");
    let supported = match tokio::time::timeout(VERSION_PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            match parse_version(&raw) {
                Some(version) => version >= THINKING_DISPLAY_MIN_VERSION,
                None => {
                    tracing::warn!(
                        program = %program.display(),
                        "claude --version output not recognised; omitting --thinking-display"
                    );
                    false
                }
            }
        }
        other => {
            tracing::warn!(
                program = %program.display(),
                failed = matches!(other, Ok(Err(_)) | Ok(Ok(_))),
                "claude --version probe did not succeed; omitting --thinking-display"
            );
            false
        }
    };

    cache()
        .lock()
        .expect("claude flag cache poisoned")
        .insert(program.to_path_buf(), supported);
    supported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_claude_version_banner() {
        // Verbatim `claude --version` output (live: 2.1.220).
        assert_eq!(parse_version("2.1.220 (Claude Code)\n"), Some((2, 1, 220)));
    }

    #[test]
    fn parses_bare_and_suffixed_versions() {
        assert_eq!(parse_version("2.1.191"), Some((2, 1, 191)));
        assert_eq!(parse_version("2.1.191-beta.3 (Claude Code)"), Some((2, 1, 191)));
    }

    #[test]
    fn rejects_unrecognised_output() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("Claude Code"), None);
        assert_eq!(parse_version("2.1"), None);
    }

    /// The gate must answer the versions we actually live-probed: 2.1.0 rejects
    /// the flag (`unknown option`), 2.1.191 and everything above accepts it.
    #[test]
    fn floor_matches_live_probed_versions() {
        let supported = |v: &str| parse_version(v).is_some_and(|p| p >= THINKING_DISPLAY_MIN_VERSION);
        assert!(!supported("2.1.0"), "2.1.0 hard-errors on --thinking-display");
        assert!(!supported("2.1.190"), "below the verified floor stays off");
        assert!(supported("2.1.191"), "lowest live-probed OK version");
        assert!(supported("2.1.215"), "our bundled CLAUDE_CLI_VERSION pin");
        assert!(supported("2.1.220"));
        assert!(supported("3.0.0"), "a future major must not regress the gate");
    }

    /// The bundled pin must never drift below the floor — that would silently turn
    /// the packaged app's thinking display off.
    #[test]
    fn bundled_pin_is_at_or_above_the_floor() {
        let pin = parse_version(aionui_runtime::CLAUDE_CLI_VERSION).expect("pin must parse");
        assert!(
            pin >= THINKING_DISPLAY_MIN_VERSION,
            "CLAUDE_CLI_VERSION {} is below the --thinking-display floor {:?}",
            aionui_runtime::CLAUDE_CLI_VERSION,
            THINKING_DISPLAY_MIN_VERSION
        );
    }
}
