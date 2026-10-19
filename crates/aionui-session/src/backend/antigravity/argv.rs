//! Command-line construction for one agy turn. Pure: no IO, no spawning.

/// AionUi's sentinel mode id meaning "run without approval prompts" (verified:
/// `crates/aionui-ai-agent/src/factory/antigravity.rs:36-70`).
///
/// Absent from agy's captured 1.4.2 mode list and never sent to it; the factory
/// answers it by removing the approval hook (verified: `~/aion/protocols/samples/agy/1.4.2/root-probe-help.txt:14`
/// and `crates/aionui-ai-agent/src/factory/antigravity.rs:36-70`).
pub(crate) const FULL_AUTO_SENTINEL: &str = "yolo";

/// AionUi's mode id meaning "ask before sensitive things" — its counterpart to
/// [`FULL_AUTO_SENTINEL`], answered by KEEPING the approval hook installed.
///
/// Not among the native values in the captured 1.4.2 help (verified:
/// `~/aion/protocols/samples/agy/1.4.2/root-probe-help.txt:14`, sha256
/// `a12355e8fc064b330eeb25ba52de596406582752442e293083bcda6388a63b78`). The argv
/// guard forwards only values advertised by that captured help.
///
/// The guard omits the value because the current captured help does not advertise it.
pub(crate) const HOST_DEFAULT_MODE: &str = "default";

// Captured from agy's own help (verified: ~/aion/protocols/samples/agy/1.4.2/root-probe-help.txt:14,
// sha256 a12355e8fc064b330eeb25ba52de596406582752442e293083bcda6388a63b78).
const HELP_ADVERTISED_AGY_MODES: &[&str] = &["accept-edits", "auto", "plan"];
// Help documents syntax; it does not guarantee account availability. The
// 1.4.2 `auto` probe warned it was unavailable for that account (verified:
// `~/aion/protocols/samples/agy/1.4.2/print_stream-json_mode-auto.stderr.txt:1`).

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
    /// An AionUi mode id, not necessarily an agy one. Only values advertised by agy's
    /// captured help reach `--mode`; `yolo` and `default` are host-side and filtered out.
    pub mode: Option<String>,
    /// The composed `[Assistant Rules]` block (preset context + skills index +
    /// dual-channel instructions), prepended to the prompt on the FIRST
    /// invocation only.
    ///
    /// agy is a layer-2 vendor with no prompt pipeline of its own, and until this
    /// existed the backend consumed only `init.mcp_servers` — so both the
    /// assistant's preset context AND its skills index were dropped, leaving an
    /// `injected`-mode agy session with no skills at all.
    pub injected_prefix: Option<String>,
    /// Skill-delivery args (allow-list entries). agy ignored `extra_args`
    /// entirely before this.
    pub extra_args: Vec<String>,
}

/// Host limit on waiting for agy's print result.
///
/// The captured 1.4.2 help reports a default of `0` (wait until completion;
/// verified:
/// `~/aion/protocols/samples/agy/1.4.2/root-probe-help.txt:20`). The host keeps
/// its existing one-hour policy so scheduled turns have a finite wait.
/// `1h` was accepted by the real live gates (verified:
/// `~/aion/protocols/samples/agy/1.4.2/live-ws-http-suite-5345050a.log`).
///
/// A short cap does not necessarily produce an error: the captured 8 s and
/// 12 s invocations returned SUCCESS, partial output, and exit 0 (verified:
/// `~/aion/protocols/samples/agy/1.4.2/flags-stream-json-print-timeout-8s.stdout.txt`,
/// `flags-stream-json-print-timeout-12s.stdout.txt`, and their stderr captures).
/// Those captures establish a partial return while the turn was in progress;
/// they do not establish cancellation of the underlying work.
const PRINT_TIMEOUT: &str = "1h";

fn non_blank(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn agy_mode_for_argv(mode: Option<&str>) -> Option<&str> {
    let mode = mode.map(str::trim).filter(|mode| !mode.is_empty())?;
    if mode.eq_ignore_ascii_case(FULL_AUTO_SENTINEL) || mode.eq_ignore_ascii_case(HOST_DEFAULT_MODE) {
        return None;
    }
    if HELP_ADVERTISED_AGY_MODES.contains(&mode) {
        return Some(mode);
    }

    tracing::warn!("dropping agy --mode value absent from captured --help");
    None
}

fn extra_args_without_mode(extra_args: &[String]) -> Vec<String> {
    let mut filtered = Vec::with_capacity(extra_args.len());
    let mut index = 0;
    while index < extra_args.len() {
        let argument = &extra_args[index];
        if argument == "--mode" {
            tracing::warn!("dropping --mode from agy extra_args; use the validated mode field");
            index += 1;
            if extra_args.get(index).is_some_and(|value| !value.starts_with('-')) {
                index += 1;
            }
            continue;
        }
        if argument.starts_with("--mode=") {
            tracing::warn!("dropping --mode from agy extra_args; use the validated mode field");
            index += 1;
            continue;
        }
        filtered.push(argument.clone());
        index += 1;
    }
    filtered
}

pub(crate) fn build_argv(input: &ArgvInput) -> Vec<String> {
    let mut a: Vec<String> = Vec::with_capacity(12);
    a.push("-p".into());
    // First invocation only. A resumed run carries agy's own history, so
    // re-injecting would repeat the whole rules block on every single turn.
    a.push(
        match (
            non_blank(&input.injected_prefix),
            non_blank(&input.resume_conversation_id),
        ) {
            (Some(prefix), None) => format!("{prefix}\n\n{}", input.prompt),
            _ => input.prompt.clone(),
        },
    );
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
    // `yolo` and `default` are host-side concepts expressed through the approval
    // hook. Forward only the native values documented by captured help; unknown
    // raw values are warned and dropped instead of silently changing CLI behavior.
    let mode = agy_mode_for_argv(non_blank(&input.mode)).map(str::to_owned);
    for (flag, value) in [("--model", &input.model), ("--mode", &mode)] {
        if let Some(v) = non_blank(value) {
            a.push(flag.into());
            a.push(v.to_owned());
        }
    }
    // Skill-delivery args last. They cannot provide a second, unvalidated --mode.
    a.extend(extra_args_without_mode(&input.extra_args));
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
            injected_prefix: None,
            extra_args: Vec::new(),
        }
    }

    /// agy spawns a fresh `-p` per turn and resumes with `--conversation`, so the
    /// injected rules belong on the FIRST invocation only — afterwards agy carries
    /// its own history and re-injecting would repeat the block every turn.
    #[test]
    fn the_first_invocation_prepends_the_injected_rules_to_the_prompt() {
        let mut input = base();
        input.injected_prefix =
            Some("[Assistant Rules]\n## Available Skills\n- **cron**: d\n[/Assistant Rules]".into());
        input.resume_conversation_id = None;

        let prompt = flag_value(&build_argv(&input), "-p").unwrap().to_owned();
        assert!(prompt.starts_with("[Assistant Rules]"), "{prompt}");
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.ends_with("hello"), "the user's own text stays last: {prompt}");
    }

    #[test]
    fn a_resumed_invocation_does_not_re_inject_the_rules() {
        let mut input = base();
        input.injected_prefix = Some("[Assistant Rules]\nrules\n[/Assistant Rules]".into());
        input.resume_conversation_id = Some("conv-1".into());

        assert_eq!(flag_value(&build_argv(&input), "-p").unwrap(), "hello");
    }

    #[test]
    fn a_blank_prefix_is_treated_as_absent() {
        let mut input = base();
        input.injected_prefix = Some("   \n".into());
        assert_eq!(flag_value(&build_argv(&input), "-p").unwrap(), "hello");
    }

    /// Skill allow-listing rides on `extra_args`, which this backend previously
    /// ignored. The workspace `--add-dir` must survive alongside it.
    #[test]
    fn caller_extra_args_reach_the_spawn_alongside_the_workspace_add_dir() {
        let mut input = base();
        input.extra_args = vec!["--add-dir".into(), "/src/cron".into()];
        let a = build_argv(&input);

        assert_eq!(
            a.iter().filter(|arg| *arg == "--add-dir").count(),
            2,
            "the workspace entry plus the allow-listed skill source: {a:?}"
        );
        assert!(a.windows(2).any(|w| w == ["--add-dir", "/w"]));
        assert!(a.windows(2).any(|w| w == ["--add-dir", "/src/cron"]));
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
    fn every_turn_sets_the_explicit_host_print_limit() {
        // Keep the host's finite wait on every invocation. The captured CLI
        // default and partial-return behavior are documented beside the constant.
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
    /// The 1.4.2 captured help advertises `accept-edits`, `auto`, and `plan`, not
    /// `default` (verified:
    /// `~/aion/protocols/samples/agy/1.4.2/root-probe-help.txt:14`, sha256
    /// `a12355e8fc064b330eeb25ba52de596406582752442e293083bcda6388a63b78`).
    ///
    /// Like `yolo`, this mode is expressed by the host — `yolo` by removing the approval
    /// hook, `default` by keeping it — so neither belongs on agy's command line. Omitting
    /// the flag keeps the host-side choice out of the native value list.
    #[test]
    fn the_default_mode_is_host_side_and_never_sent_to_agy() {
        let mut i = base();
        i.mode = Some("default".to_owned());
        let a = build_argv(&i);
        assert!(
            !a.contains(&"--mode".to_string()),
            "unadvertised mode must be omitted, argv={a:?}"
        );
        assert!(!a.iter().any(|x| x == "default"), "argv={a:?}");
    }

    #[test]
    /// Only the values agy advertises in the captured help are forwarded.
    fn captured_help_modes_are_forwarded_to_argv() {
        for &mode in HELP_ADVERTISED_AGY_MODES {
            let mut i = base();
            i.mode = Some(mode.to_owned());
            let a = build_argv(&i);
            let at = a.iter().position(|x| x == "--mode").expect("mode flag missing");
            assert_eq!(a[at + 1], mode);
        }
    }

    #[test]
    fn unknown_agy_mode_values_are_dropped() {
        let mut i = base();
        i.mode = Some("future-mode".to_owned());
        let a = build_argv(&i);
        assert!(!a.contains(&"--mode".to_string()), "argv={a:?}");
        assert!(!a.iter().any(|arg| arg == "future-mode"), "argv={a:?}");
    }

    #[test]
    fn mode_cannot_be_smuggled_through_skill_extra_args() {
        let mut i = base();
        i.extra_args = vec![
            "--mode".into(),
            "future-mode".into(),
            "--add-dir".into(),
            "/src/skill".into(),
        ];
        let a = build_argv(&i);
        assert!(!a.contains(&"--mode".to_string()), "argv={a:?}");
        assert!(a.windows(2).any(|pair| pair == ["--add-dir", "/src/skill"]));
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
