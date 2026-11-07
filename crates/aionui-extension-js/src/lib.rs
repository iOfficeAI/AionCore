//! Explicitly opt-in Linux pure-JSON computation helper. Not an OS sandbox.
//! No production extension route, identity resolution or process supervisor is wired here.
use std::io::{self, Write};
use std::process::ExitCode;

#[cfg(all(feature = "engine", target_os = "linux"))]
mod engine;

/// Read one bounded request and emit one response. A supervising parent must
/// enforce wall-clock timeout, bounded pipes, kill+wait, cancellation and parent death.
pub fn run_stdio() -> ExitCode {
    #[cfg(all(feature = "engine", target_os = "linux"))]
    let (response, code) = (engine::response(io::stdin().lock()), ExitCode::SUCCESS);
    #[cfg(not(all(feature = "engine", target_os = "linux")))]
    let (response, code) = (
        serde_json::json!({"ok": false, "error": "runner_unavailable"}),
        ExitCode::from(2),
    );
    let mut output = io::stdout().lock();
    if writeln!(output, "{response}").and_then(|()| output.flush()).is_err() {
        return ExitCode::FAILURE;
    }
    code
}
