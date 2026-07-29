//! T2 (spec §10.7, plan §5): `SessionBackend::terminate` force-kill delegation +
//! isolation. The claude/codex direct-CLI backends override `terminate` to drive
//! `SuspendController::terminate` (abort reader → group-kill the process tree via
//! `AgentIo::terminate`); the ACP-backed `AcpSessionBackend` deliberately does NOT
//! override it, so its default trait `terminate` is a no-op and never tears down
//! its suspend controller. Each backend is built over a terminate-counting
//! `AgentIo` so the delegation is observable as an exact call count.

use std::sync::atomic::Ordering;

use aionui_session::testing::CountingTerminateIo;
use aionui_session::{AcpSessionBackend, ClaudeSessionBackend, CodexSessionBackend, SessionBackend};

#[tokio::test]
async fn claude_terminate_group_kills_via_suspend_once() {
    let io = CountingTerminateIo::new();
    let counter = io.terminate_counter();
    let backend = ClaudeSessionBackend::build_with_io("claude-1", Box::new(io)).await;

    backend.terminate().await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "claude terminate() must delegate to suspend.terminate() → io.terminate() exactly once"
    );
}

#[tokio::test]
async fn codex_terminate_group_kills_via_suspend_once() {
    let io = CountingTerminateIo::new();
    let counter = io.terminate_counter();
    let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(io)).await;

    backend.terminate().await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "codex terminate() must delegate to suspend.terminate() → io.terminate() exactly once"
    );
}

#[tokio::test]
async fn acp_terminate_is_default_noop_isolation() {
    let io = CountingTerminateIo::new();
    let counter = io.terminate_counter();
    let backend = AcpSessionBackend::build_with_io("acp-1", Box::new(io)).await;

    // AcpSessionBackend does NOT override terminate → the default trait no-op runs
    // and must NOT tear down its suspend controller (zero io.terminate calls).
    backend.terminate().await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "AcpSessionBackend must not override terminate — isolation regression (spec §10.7)"
    );
}

/// T3 (plan §5): the Layer A default. `FakeAgentIo` (and every non-process
/// `AgentIo`) inherits the trait's default `terminate` no-op — it completes
/// without panicking and does nothing. Only `ManagedProcessIo` overrides it to
/// group-kill via the already-tested `ManagedProcess::kill` (that primitive is
/// covered in `aionui-process`; it cannot be unit-constructed here without a
/// real OS process).
#[tokio::test]
async fn fake_agent_io_terminate_is_default_noop() {
    use aionui_session::AgentIo;
    let io = aionui_session::testing::FakeAgentIo::never_exits(Vec::new());
    io.terminate().await; // default trait no-op — must complete without panic
}
