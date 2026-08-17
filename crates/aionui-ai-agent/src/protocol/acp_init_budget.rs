//! Budget for the ACP `initialize` handshake.
//!
//! A bridge-launched row (`npx -y <package> …`) pays a package install on its
//! FIRST spawn, and that install happens inside the handshake window: npx
//! downloads before the agent process can answer `initialize`. Measured cold
//! runs on a fast link already exceed the steady-state budget by a wide margin
//! (`@oh-my-pi/pi-coding-agent`: 81s cold vs 10s warm), so a single flat budget
//! either fails every first run or lets a genuinely hung agent idle for minutes.
//!
//! So the budget is picked per spawn: a launch whose npx package is not yet in
//! the cache gets the cold-start budget, everything else keeps the steady-state
//! one. "Not yet in the cache" is decided by the same `_npx/<hash>` entry
//! `npx_cache_repair` computes, so both features agree on which directory npm
//! would use for this exact package set.

use std::time::Duration;

use aionui_common::CommandSpec;

use super::npx_cache_repair::computed_npx_cache_entry;

/// Steady-state `initialize` budget (seconds), used once the launch has
/// everything it needs on disk.
pub(crate) const INIT_TIMEOUT_SECS: u64 = 30;

/// `initialize` budget (seconds) for a launch that still has to install its
/// package. Sized off the slowest measured cold run (81s on a fast link) with
/// headroom for connections several times slower.
pub(crate) const COLD_START_INIT_TIMEOUT_SECS: u64 = 300;

/// Overrides [`COLD_START_INIT_TIMEOUT_SECS`] for environments whose first-run
/// install is slower still. Mirrors `AIONUI_HANDSHAKE_TIMEOUT_SECS`.
const COLD_START_TIMEOUT_ENV: &str = "AIONUI_ACP_INIT_TIMEOUT_SECS";

/// The chosen budget plus the reason, so the caller can log which one applied
/// instead of leaving a 300s wait unexplained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InitBudget {
    pub(crate) timeout: Duration,
    pub(crate) cold_start: bool,
}

/// Pick the `initialize` budget for `command_spec`.
pub(crate) fn init_budget(command_spec: &CommandSpec) -> InitBudget {
    init_budget_with(command_spec, cold_start_timeout_secs())
}

/// Budget selection without the environment read, so tests stay deterministic
/// (`std::env` is process-global and would race across parallel tests).
fn init_budget_with(command_spec: &CommandSpec, cold_start_secs: u64) -> InitBudget {
    if awaiting_first_install(command_spec) {
        return InitBudget {
            timeout: Duration::from_secs(cold_start_secs),
            cold_start: true,
        };
    }
    InitBudget {
        timeout: Duration::from_secs(INIT_TIMEOUT_SECS),
        cold_start: false,
    }
}

/// Whether this launch still has to install its npx package.
///
/// False for a direct-CLI launch (nothing to install) and false when the cache
/// location cannot be computed — an unknown cache state is not evidence of a
/// first run, and guessing "cold" there would hand every unresolvable launch
/// the long budget.
fn awaiting_first_install(command_spec: &CommandSpec) -> bool {
    computed_npx_cache_entry(command_spec).is_some_and(|entry| !entry.exists())
}

fn cold_start_timeout_secs() -> u64 {
    parse_cold_start_secs(std::env::var(COLD_START_TIMEOUT_ENV).ok().as_deref())
}

/// Parse the override, ignoring absent, unparsable, and zero values.
fn parse_cold_start_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(COLD_START_INIT_TIMEOUT_SECS)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use aionui_common::{CommandSpec, EnvVar};

    use super::{COLD_START_INIT_TIMEOUT_SECS, INIT_TIMEOUT_SECS, init_budget_with, parse_cold_start_secs};

    /// Package set and cache hash are the pair `npx_cache_repair` already pins
    /// in `computes_npx_cache_entry_from_direct_package_argument`, so these
    /// tests anchor on an independently verified hash rather than on the
    /// function under test.
    const PINNED_ARGS: &[&str] = &["-y", "@xai-official/grok@0.2.102", "agent", "stdio"];
    const PINNED_CACHE_HASH: &str = "c16927192d2e8dc3";

    fn npx_spec(cache: &Path) -> CommandSpec {
        CommandSpec {
            command: PathBuf::from("npx"),
            args: PINNED_ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
            env: vec![EnvVar {
                name: "npm_config_cache".to_owned(),
                value: cache.display().to_string(),
            }],
            cwd: None,
        }
    }

    #[test]
    fn npx_launch_whose_package_is_not_installed_yet_gets_the_cold_start_budget() {
        let temp = tempfile::tempdir().unwrap();
        let spec = npx_spec(temp.path());

        let budget = init_budget_with(&spec, 300);

        assert!(budget.cold_start, "a missing npx cache entry is a first run");
        assert_eq!(budget.timeout.as_secs(), 300);
    }

    #[test]
    fn npx_launch_whose_package_is_already_installed_keeps_the_steady_state_budget() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("_npx").join(PINNED_CACHE_HASH)).unwrap();
        let spec = npx_spec(temp.path());

        let budget = init_budget_with(&spec, 300);

        assert!(!budget.cold_start, "a populated cache entry is not a first run");
        assert_eq!(budget.timeout.as_secs(), INIT_TIMEOUT_SECS);
    }

    #[test]
    fn direct_cli_launch_keeps_the_steady_state_budget() {
        let spec = CommandSpec {
            command: PathBuf::from("omp"),
            args: vec!["acp".to_owned()],
            env: vec![],
            cwd: None,
        };

        let budget = init_budget_with(&spec, 300);

        assert!(!budget.cold_start, "a direct CLI has no package to install");
        assert_eq!(budget.timeout.as_secs(), INIT_TIMEOUT_SECS);
    }

    #[test]
    fn npx_launch_without_a_cache_location_keeps_the_steady_state_budget() {
        let spec = CommandSpec {
            command: PathBuf::from("npx"),
            args: PINNED_ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
            env: vec![],
            cwd: None,
        };

        let budget = init_budget_with(&spec, 300);

        assert!(
            !budget.cold_start,
            "an uncomputable cache location is not evidence of a first run"
        );
        assert_eq!(budget.timeout.as_secs(), INIT_TIMEOUT_SECS);
    }

    #[test]
    fn cold_start_override_is_read_from_the_environment_value() {
        assert_eq!(parse_cold_start_secs(Some("900")), 900);
        assert_eq!(parse_cold_start_secs(Some(" 900 ")), 900);
    }

    #[test]
    fn absent_unparsable_and_zero_overrides_fall_back_to_the_default() {
        assert_eq!(parse_cold_start_secs(None), COLD_START_INIT_TIMEOUT_SECS);
        assert_eq!(parse_cold_start_secs(Some("soon")), COLD_START_INIT_TIMEOUT_SECS);
        assert_eq!(parse_cold_start_secs(Some("0")), COLD_START_INIT_TIMEOUT_SECS);
    }
}
