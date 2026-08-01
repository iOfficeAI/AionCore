//! Command-line construction for one agy turn. Pure: no IO, no spawning.

/// AionUi's sentinel mode id meaning "run without approval prompts".
///
/// Not one of agy's modes (default / accept-edits / plan) and never sent to it.
pub(crate) const FULL_AUTO_SENTINEL: &str = "yolo";

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
    /// `default` | `accept-edits` | `plan`.
    pub mode: Option<String>,
}

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

    if let Some(id) = non_blank(&input.resume_conversation_id) {
        a.push("--conversation".into());
        a.push(id.to_owned());
    }
    if let Some(w) = non_blank(&input.workspace) {
        a.push("--add-dir".into());
        a.push(w.to_owned());
    }
    // No `--effort`: effort lives inside the model id, and a stripped id is
    // silently ignored by agy.
    // `yolo` is AionUi's sentinel for "no approval prompts", not one of agy's
    // modes (default / accept-edits / plan). agy tolerates unknown values
    // silently, so forwarding it would look fine and quietly leave the session
    // on whatever mode agy defaults to.
    let mode = input
        .mode
        .as_deref()
        .filter(|m| !m.eq_ignore_ascii_case(FULL_AUTO_SENTINEL));
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
    }

    #[test]
    fn the_full_auto_sentinel_is_never_sent_as_agys_mode() {
        // `yolo` is AionUi's marker, not one of agy's modes. agy accepts unknown
        // --mode values silently (verified: `--mode default` exits 0 even though
        // --help lists only accept-edits/plan), so forwarding it would look fine
        // and quietly leave the session on agy's default.
        let mut i = base();
        i.mode = Some("yolo".to_owned());
        let a = build_argv(&i);
        assert!(!a.contains(&"--mode".to_string()), "argv={a:?}");
        assert!(!a.iter().any(|x| x == "yolo"), "argv={a:?}");
    }

    #[test]
    fn agys_real_modes_are_forwarded() {
        for mode in ["default", "accept-edits", "plan"] {
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
