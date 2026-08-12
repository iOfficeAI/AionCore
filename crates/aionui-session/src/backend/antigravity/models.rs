//! Model discovery for agy (`agy models`).
//!
//! agy model ids ALREADY encode reasoning effort (`-high` / `-medium` /
//! `-low`), and agy only accepts a complete id: a stripped one is silently
//! dropped and it falls back to another model without reporting anything. So
//! the ids are surfaced verbatim and no separate effort axis is advertised.
//!
//! Output formats observed in the wild:
//! - bare id per line (`gemini-3.6-flash-high`) — older builds
//! - TSV `id<TAB>display name` — newer builds (e.g. current `agy models`)
//!
//! Both are accepted. Status / error lines that are not model ids are dropped.

use std::sync::Arc;

use aionui_common::{CommandSpec, EnvVar};
use aionui_process::Spawner;

use crate::capability::ModelInfo;

/// A model id is a single lowercase-ish token: no spaces, no punctuation beyond
/// `-`/`.`/`_`. Used to tell ids apart from the human-readable errors agy
/// prints to stdout (e.g. the signed-out notice).
fn looks_like_model_id(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

pub(crate) fn parse_agy_models(stdout: &str) -> Vec<ModelInfo> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // Status / progress chatter some builds emit before the list.
        .filter(|line| !line.eq_ignore_ascii_case("Fetching available models..."))
        .filter_map(|line| {
            // Newer agy builds print `id<TAB>display name`. Older builds print
            // the bare id alone. Split on the first tab only so a display name
            // may itself contain tabs without shifting the id.
            let mut fields = line.splitn(2, '\t');
            let id = fields.next().unwrap_or("").trim();
            if !looks_like_model_id(id) {
                return None;
            }
            let display = fields.next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(id);
            Some(ModelInfo {
                id: id.to_owned(),
                name: display.to_owned(),
                description: None,
                // Deliberately empty — see the module docs.
                reasoning_efforts: Vec::new(),
            })
        })
        .collect()
}

/// Ask agy which models this account can use.
///
/// Best-effort by contract: any failure (agy missing, signed out, slow) yields
/// an empty list rather than an error, because a model picker that cannot be
/// populated must not stop the user from opening a session.
///
/// `spawn_env` is the same env conversation turns receive (agent
/// `env_override` / proxy settings). The model-registry call needs network
/// access under the same conditions as a real turn; probing without it is a
/// common reason the picker stays empty while chat still works.
pub(crate) async fn probe_models(
    spawner: &Arc<dyn Spawner>,
    program: &std::path::Path,
    owner_tag: &str,
    spawn_env: &[EnvVar],
) -> Vec<ModelInfo> {
    let spec = CommandSpec {
        command: program.to_path_buf(),
        args: vec!["models".to_owned()],
        env: spawn_env.to_vec(),
        cwd: None,
    };
    let Ok(proc) = spawner.spawn(spec, &[], owner_tag).await else {
        return Vec::new();
    };
    let Some((stdin, stdout)) = proc.take_stdio().await else {
        return Vec::new();
    };

    // `agy models` does NOT exit while its stdin is open — it prints the list
    // and then keeps waiting, so stdout never reaches EOF and the read below
    // would hang forever. Verified against agy 1.1.9: stdin left open = still
    // running after 40 minutes; stdin closed = exits in ~3s. Dropping the
    // handle closes the pipe and delivers the EOF that lets agy finish.
    drop(stdin);

    let text = match tokio::time::timeout(PROBE_TIMEOUT, read_to_end(stdout)).await {
        Ok(text) => text,
        Err(_) => {
            // Never leave the child behind: this task is detached, so a hung
            // agy would leak a process for the lifetime of the app.
            tracing::warn!("antigravity: `agy models` did not finish in time; model list left empty");
            let _ = proc.kill(std::time::Duration::from_secs(1)).await;
            return Vec::new();
        }
    };
    parse_agy_models(&text)
}

/// How long to wait for `agy models` before giving up on the model list.
/// Generous: a cold agy takes ~3s, but a slow network sign-in check is slower.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn read_to_end(stdout: aionui_process::BoxedStdout) -> String {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    let mut out = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_id_per_line() {
        // Real bare-id `agy models` output (2026-07-31).
        let out = "gemini-3.6-flash-high\ngemini-3.6-flash-low\ngemini-3.1-pro-high\nclaude-sonnet-4-6\ngpt-oss-120b-medium\n";
        let models = parse_agy_models(out);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![
                "gemini-3.6-flash-high",
                "gemini-3.6-flash-low",
                "gemini-3.1-pro-high",
                "claude-sonnet-4-6",
                "gpt-oss-120b-medium",
            ]
        );
        // Bare-id format: name falls back to the id.
        assert_eq!(models[0].name, "gemini-3.6-flash-high");
    }

    #[test]
    fn parses_tsv_id_and_display_name() {
        // Real current `agy models` output: id + tab + display label.
        // Without TSV support every row is discarded (display names contain
        // spaces), so the picker stays empty while chat still works.
        let out = "\
gemini-3.1-pro-high\tGemini 3.1 Pro (High) · Advanced reasoning
claude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking) · Most capable
gpt-oss-120b-medium\tGPT OSS 120B (Medium)
";
        let models = parse_agy_models(out);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gemini-3.1-pro-high", "claude-opus-4-6-thinking", "gpt-oss-120b-medium",]
        );
        assert_eq!(models[0].name, "Gemini 3.1 Pro (High) · Advanced reasoning");
        assert_eq!(models[1].name, "Claude Opus 4.6 (Thinking) · Most capable");
        assert_eq!(models[2].name, "GPT OSS 120B (Medium)");
        assert!(models.iter().all(|m| m.reasoning_efforts.is_empty()));
    }

    #[test]
    fn tsv_with_status_line_still_parses_models() {
        let out = "\
Fetching available models...
gemini-3.1-pro-high\tGemini 3.1 Pro (High)
";
        let models = parse_agy_models(out);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gemini-3.1-pro-high"]
        );
    }

    #[test]
    fn model_ids_keep_their_effort_suffix_and_expose_no_effort_axis() {
        // agy's ids already encode effort. Exposing a separate effort picker
        // would let the UI build a stripped id, which agy silently ignores
        // while falling back to another model — with no error anywhere.
        let models = parse_agy_models("gemini-3.6-flash-high\n");
        assert_eq!(models[0].id, "gemini-3.6-flash-high");
        assert!(models.iter().all(|m| m.reasoning_efforts.is_empty()));
    }

    #[test]
    fn blank_lines_and_padding_are_ignored() {
        let models = parse_agy_models("\n  gemini-3.6-flash-low  \n\n\tclaude-sonnet-4-6\n   \n");
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gemini-3.6-flash-low", "claude-sonnet-4-6"]
        );
    }

    #[test]
    fn a_sign_in_error_yields_no_models_rather_than_a_bogus_one() {
        // Logged out, `agy models` prints this on stdout and exits 1. Treating
        // it as a model id would put a sentence in the model picker.
        let models = parse_agy_models(
            "Error: Please sign in to view available models. Launch the CLI without arguments to sign in.\n",
        );
        assert!(models.is_empty(), "got {models:?}");
    }

    #[test]
    fn empty_output_is_not_an_error() {
        // Probing must never block session creation.
        assert!(parse_agy_models("").is_empty());
    }

    /// Build a stand-in for agy that mirrors the ONE behaviour this test is
    /// about: print the model list, then stay alive until stdin reaches EOF.
    #[cfg(unix)]
    fn fake_agy_that_waits_on_stdin(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-agy");
        std::fs::write(&script, "#!/bin/sh\necho gemini-3.6-flash-high\ncat > /dev/null\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probing_closes_stdin_so_agy_can_exit() {
        // Regression: `agy models` prints its list and then waits on stdin.
        // Holding the stdin handle open meant stdout never reached EOF, so the
        // probe hung forever and the model picker stayed silently empty — the
        // failure showed up only as `available_models: None`, with no error.
        let tmp = tempfile::tempdir().unwrap();
        let program = fake_agy_that_waits_on_stdin(tmp.path());
        let spawner: Arc<dyn Spawner> = Arc::new(aionui_process::RealSpawner::new(
            Arc::new(aionui_process::FileRegistryStore::new(tmp.path())),
            uuid::Uuid::now_v7(),
            "test-machine",
        ));

        let models = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            probe_models(&spawner, &program, "probe-test", &[]),
        )
        .await
        .expect("probe hung — stdin was left open, so stdout never reached EOF");

        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gemini-3.6-flash-high"]
        );
    }
}
