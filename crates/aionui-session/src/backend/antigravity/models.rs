//! Model discovery for agy (`agy models`).
//!
//! agy prints one model id per line. Those ids ALREADY encode reasoning effort
//! (`-high` / `-medium` / `-low`), and agy only accepts a complete id: a
//! stripped one is silently dropped and it falls back to another model without
//! reporting anything. So the ids are surfaced verbatim and no separate effort
//! axis is advertised.

use std::sync::Arc;

use aionui_common::CommandSpec;
use aionui_process::Spawner;

use crate::capability::ModelInfo;

/// A model id is a single lowercase-ish token: no spaces, no punctuation beyond
/// `-`/`.`/`_`. Used to tell ids apart from the human-readable errors agy
/// prints to stdout (e.g. the signed-out notice).
fn looks_like_model_id(line: &str) -> bool {
    !line.is_empty()
        && line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

pub(crate) fn parse_agy_models(stdout: &str) -> Vec<ModelInfo> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| looks_like_model_id(l))
        .map(|id| ModelInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            // Deliberately empty — see the module docs.
            reasoning_efforts: Vec::new(),
        })
        .collect()
}

/// Ask agy which models this account can use.
///
/// Best-effort by contract: any failure (agy missing, signed out, slow) yields
/// an empty list rather than an error, because a model picker that cannot be
/// populated must not stop the user from opening a session.
pub(crate) async fn probe_models(
    spawner: &Arc<dyn Spawner>,
    program: &std::path::Path,
    owner_tag: &str,
) -> Vec<ModelInfo> {
    let spec = CommandSpec {
        command: program.to_path_buf(),
        args: vec!["models".to_owned()],
        env: Vec::new(),
        cwd: None,
    };
    let Ok(proc) = spawner.spawn(spec, &[], owner_tag).await else {
        return Vec::new();
    };
    let Some((_stdin, stdout)) = proc.take_stdio().await else {
        return Vec::new();
    };

    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    let mut out = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        out.push_str(&line);
        out.push('\n');
    }
    parse_agy_models(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_id_per_line() {
        // Real `agy models` output (2026-07-31).
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
}
