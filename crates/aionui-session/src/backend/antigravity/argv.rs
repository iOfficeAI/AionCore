//! Command-line construction for one agy turn. Pure: no IO, no spawning.

/// AionUi's sentinel mode id meaning "run without approval prompts".
///
/// Not one of agy's modes and never sent to it; answered by removing the approval hook.
pub(crate) const FULL_AUTO_SENTINEL: &str = "yolo";

/// AionUi's mode id meaning "ask before sensitive things" — its counterpart to
/// [`FULL_AUTO_SENTINEL`], answered by KEEPING the approval hook installed.
///
/// Also not an agy value: `agy --help` lists exactly two (verified:
/// samples/antigravity-cli/1.1.8/agy_--help.txt:12 — "Set the agent execution mode for
/// this session (accept-edits, plan)"). Passing `--mode default` makes agy print
/// `warning: unrecognized --mode value "default" (valid: accept-edits, plan)` and drop
/// the flag, leaving the session on whatever agy defaults to while the UI reports the
/// switch as applied.
///
/// Omitting the flag is the correct translation anyway: agy's own default IS this mode.
pub(crate) const HOST_DEFAULT_MODE: &str = "default";

/// Everything one `agy` invocation needs. There is deliberately no `effort`
/// field: agy's model ids already carry the effort suffix (see `model`).
#[derive(Debug, Clone, Default)]
pub(crate) struct ArgvInput {
    pub prompt: String,
    /// `Some` => resume that agy conversation; `None` => start a fresh one.
    pub resume_conversation_id: Option<String>,
    /// Conversation workspace. Without it agy runs tools in its own scratch
    /// directory, where none of the user's files exist.
    pub workspace: Option<String>,
    /// MUST be a complete id as listed by `agy models`, effort suffix included.
    /// agy silently ignores anything else and falls back to its default model
    /// without reporting an error.
    pub model: Option<String>,
    /// An AionUi mode id, not necessarily an agy one. `accept-edits` / `plan` reach agy;
    /// `yolo` and `default` are host-side and filtered out (see the constants above).
    pub mode: Option<String>,
}

/// How long agy may wait in print mode before abandoning the turn.
///
/// agy's own default is 5m0s (`agy --help`, 1.1.9) and it is a wall-clock cap
/// on the WHOLE turn, not an idle timeout: a long build, a big refactor or a
/// slow tool run trips it while agy is still making steady progress. Measured
/// on 1.1.9, the cap fires as a clean terminal frame — `result` with
/// `status:"ERROR"`, `error:"timeout waiting for response"`, exit 1 — so the
/// turn dies with an error the user cannot act on.
///
/// It is raised, but NOT removed, because it is the only automatic recovery
/// this integration has. There is no in-turn silence watchdog anywhere in the
/// stack: `suspend.rs`'s `last_activity` only moves on dispatch (a session
/// dormancy TTL), and `collect_idle` skips tasks that are still running. So if
/// agy goes quiet while alive, the only other ways out are the user's Cancel —
/// which nobody is there to press on a scheduled run — and this cap.
///
/// One hour is therefore the trade: 12× agy's default, comfortably past any
/// legitimate turn, while still bounding a silent hang instead of parking a
/// cron conversation for a day. Go duration syntax; verified accepted by agy
/// 1.1.9 (an unparseable value is a hard flag error).
const PRINT_TIMEOUT: &str = "1h";

fn non_blank(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub(crate) fn build_argv(input: &ArgvInput) -> Vec<String> {
    let mut a: Vec<String> = Vec::with_capacity(12);
    a.push("-p".into());
    a.push(input.prompt.clone());
    a.push("--output-format".into());
    a.push("stream-json".into());
    // agy cannot prompt for permission in headless mode; with the gate shut it
    // soft-denies every confirmable tool, and a PreToolUse hook returning
    // "allow" cannot override that (hooks can tighten, not loosen). AionUi
    // opens the gate here and gates each call in its own hook bridge instead.
    a.push("--dangerously-skip-permissions".into());
    a.push("--print-timeout".into());
    a.push(PRINT_TIMEOUT.into());

    if let Some(id) = non_blank(&input.resume_conversation_id) {
        a.push("--conversation".into());
        a.push(id.to_owned());
    } else {
        // A fresh print invocation otherwise reuses agy's globally active
        // project, so workspace plugins can be scanned from an unrelated cwd.
        // `agy --help` 1.1.13 defines this flag as creating a project for the
        // current CLI session; a real probe bound the primary workspace to cwd
        // (verified: ~/.gemini/antigravity-cli/log/cli-20260814_144215.log:52,54,66).
        a.push("--new-project".into());
    }
    if let Some(w) = non_blank(&input.workspace) {
        a.push("--add-dir".into());
        a.push(w.to_owned());
    }
    // No `--effort`: effort lives inside the model id, and a stripped id is
    // silently ignored by agy.
    // agy takes exactly two values here: `accept-edits` and `plan`. AionUi's other two
    // modes are host-side concepts expressed through the approval hook — `yolo` by
    // removing it, `default` by keeping it — and agy rejects both by name, warning and
    // discarding the flag. Sending either would look fine (exit 0) while silently
    // leaving the session on agy's default. See the two constants above.
    let mode = input
        .mode
        .as_deref()
        .filter(|m| !m.eq_ignore_ascii_case(FULL_AUTO_SENTINEL) && !m.eq_ignore_ascii_case(HOST_DEFAULT_MODE));
    let mode = mode.map(str::to_owned);
    for (flag, value) in [("--model", &input.model), ("--mode", &mode)] {
        if let Some(v) = non_blank(value) {
            a.push(flag.into());
            a.push(v.to_owned());
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ArgvInput {
        ArgvInput {
            prompt: "hello".into(),
            resume_conversation_id: None,
            workspace: Some("/w".into()),
            model: None,
            mode: None,
        }
    }

    fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.windows(2).find(|w| w[0] == flag).map(|w| w[1].as_str())
    }

    #[test]
    fn fresh_turn_uses_print_and_stream_json() {
        let a = build_argv(&base());
        assert!(a.contains(&"-p".to_string()));
        assert!(a.contains(&"hello".to_string()));
        assert_eq!(flag_value(&a, "--output-format"), Some("stream-json"));
        assert!(!a.contains(&"--conversation".to_string()));
        assert!(
            a.contains(&"--new-project".to_string()),
            "a fresh agy turn must bind its primary customization workspace to the session cwd"
        );
    }

    #[test]
    fn every_turn_lifts_agys_five_minute_wall_clock_cap() {
        // Left at agy's 5m0s default, a long-but-healthy turn (big refactor,
        // slow build) is killed by agy itself while this layer — which has no
        // wall-clock cap and detects hangs by dead silence — sees nothing wrong.
        for input in [
            base(),
            ArgvInput {
                mode: Some("plan".into()),
                ..base()
            },
        ] {
            let a = build_argv(&input);
            assert_eq!(flag_value(&a, "--print-timeout"), Some(PRINT_TIMEOUT), "argv={a:?}");
        }
    }

    #[test]
    fn the_print_timeout_is_a_duration_agy_can_parse() {
        // agy rejects an unparseable duration at flag-parse time, so a typo here
        // would break EVERY turn, not just long ones. Go duration syntax:
        // a number followed by a unit.
        let (digits, unit) = PRINT_TIMEOUT.split_at(PRINT_TIMEOUT.find(|c: char| !c.is_ascii_digit()).unwrap_or(0));
        assert!(!digits.is_empty(), "no leading number in {PRINT_TIMEOUT:?}");
        assert!(
            matches!(unit, "h" | "m" | "s" | "ms"),
            "unit {unit:?} is not a Go duration unit"
        );
    }

    #[test]
    fn the_full_auto_sentinel_is_never_sent_as_agys_mode() {
        // `yolo` is AionUi's marker, not one of agy's modes. It is answered by NOT
        // installing the approval hook, never by a `--mode` value.
        let mut i = base();
        i.mode = Some("yolo".to_owned());
        let a = build_argv(&i);
        assert!(!a.contains(&"--mode".to_string()), "argv={a:?}");
        assert!(!a.iter().any(|x| x == "yolo"), "argv={a:?}");
    }

    /// `default` is AionUi's "ask before sensitive things" mode, NOT an agy value.
    ///
    /// agy takes only `accept-edits` and `plan` (verified:
    /// samples/antigravity-cli/1.1.8/agy_--help.txt:12 — "Set the agent execution mode
    /// for this session (accept-edits, plan)"). Sending `--mode default` makes agy print
    /// `warning: unrecognized --mode value "default" (valid: accept-edits, plan)` and
    /// DROP the flag, so the session silently stays on whatever agy defaults to while the
    /// UI reports the switch as done.
    ///
    /// An earlier note claimed agy "accepts unknown --mode values silently (exits 0)".
    /// Exit 0 is not acceptance: the observed run warns and discards the argument.
    ///
    /// Like `yolo`, this mode is expressed by the host — `yolo` by removing the approval
    /// hook, `default` by keeping it — so neither belongs on agy's command line. Omitting
    /// the flag is also exactly right semantically: agy's own default IS "default".
    #[test]
    fn the_default_mode_is_host_side_and_never_sent_to_agy() {
        let mut i = base();
        i.mode = Some("default".to_owned());
        let a = build_argv(&i);
        assert!(
            !a.contains(&"--mode".to_string()),
            "agy rejects `--mode default`; the flag must be omitted, argv={a:?}"
        );
        assert!(!a.iter().any(|x| x == "default"), "argv={a:?}");
    }

    #[test]
    /// Only the two values agy actually documents are forwarded.
    ///
    /// `default` used to be in this list, which is what put an unrecognized value on
    /// agy's command line; it is host-side and covered by
    /// `the_default_mode_is_host_side_and_never_sent_to_agy`.
    fn agys_real_modes_are_forwarded() {
        for mode in ["accept-edits", "plan"] {
            let mut i = base();
            i.mode = Some(mode.to_owned());
            let a = build_argv(&i);
            let at = a.iter().position(|x| x == "--mode").expect("mode flag missing");
            assert_eq!(a[at + 1], mode);
        }
    }

    #[test]
    fn permission_gate_is_opened_for_the_hook_bridge() {
        // agy cannot prompt for permission in headless mode: with the gate shut
        // it soft-denies every confirmable tool, and a PreToolUse hook returning
        // "allow" cannot override that. AionUi opens the gate here and makes its
        // own hook the sole gatekeeper.
        let a = build_argv(&base());
        assert!(a.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn workspace_is_always_passed_as_add_dir() {
        // Without --add-dir agy runs tools in its own scratch directory and the
        // agent cannot see the conversation's files at all.
        let a = build_argv(&base());
        assert_eq!(flag_value(&a, "--add-dir"), Some("/w"));
    }

    #[test]
    fn resume_turn_passes_conversation_id() {
        let mut i = base();
        i.resume_conversation_id = Some("conv-9".into());
        let a = build_argv(&i);
        assert_eq!(flag_value(&a, "--conversation"), Some("conv-9"));
        assert!(!a.contains(&"--new-project".to_string()));
    }

    #[test]
    fn optional_axes_are_omitted_when_unset() {
        let a = build_argv(&base());
        assert!(!a.contains(&"--model".to_string()));
        assert!(!a.contains(&"--mode".to_string()));
    }

    #[test]
    fn optional_axes_are_forwarded_when_set() {
        let mut i = base();
        i.model = Some("gemini-3.1-pro-high".into());
        i.mode = Some("plan".into());
        let a = build_argv(&i);
        assert_eq!(flag_value(&a, "--model"), Some("gemini-3.1-pro-high"));
        assert_eq!(flag_value(&a, "--mode"), Some("plan"));
    }

    #[test]
    fn effort_is_never_sent_as_a_separate_axis() {
        // agy's model ids already encode effort (`-high` / `-medium` / `-low`).
        // Sending --effort invites a stripped model id, which agy silently
        // ignores while falling back to its default model, with no error.
        let mut i = base();
        i.model = Some("gemini-3.6-flash-high".into());
        let a = build_argv(&i);
        assert!(!a.contains(&"--effort".to_string()));
    }

    #[test]
    fn blank_optional_values_are_treated_as_unset() {
        let mut i = base();
        i.model = Some("   ".into());
        i.mode = Some(String::new());
        i.resume_conversation_id = Some(String::new());
        let a = build_argv(&i);
        assert!(!a.contains(&"--model".to_string()));
        assert!(!a.contains(&"--mode".to_string()));
        assert!(!a.contains(&"--conversation".to_string()));
    }
}
