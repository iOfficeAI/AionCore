//! Self-suspending process controller (007 §F-4 / P0c F-4).
//!
//! A backend that owns a spawned process can let it go idle: after `idle_ttl`
//! with no dispatch, the process is closed (→ Dormant); the next dispatch
//! re-spawns it (with the backend's own `--resume`/`session/load`/`thread/resume`
//! handshake, so the session is logically continuous). The whole suspend/wake
//! dance happens behind ONE lock (`slot`), so there is a single actor and no
//! TOCTOU between "the idle timer closes it" and "a dispatch wakes it" — the
//! design's load-bearing invariant.
//!
//! This controller holds ONLY the process-bound pair `{reader, io}` (a suspend
//! drops them; a wake recreates them). Everything that must survive a suspend —
//! the shared `stdin`, the `event_tx` broadcast, `turn_gen`, and the backend
//! session-id binding (the resume anchor) — stays on the backend itself; the
//! backend's `wake` closure repopulates the shared `stdin` and spawns a fresh
//! reader on the SAME `event_tx`, so subscribers and the FSM never notice.
//!
//! ## OFF by default (production parity)
//! `idle_ttl_ms = None` means "never suspend": the idle timer is not spawned and
//! the slot stays Active for the backend's whole lifetime. In that mode the only
//! added cost per dispatch is one uncontended async-mutex lock + one atomic store
//! — the wire/parse output is byte-identical to the pre-F-4 backend. Suspension
//! is opt-in (a configured ttl), so the hard "claude parse zero-diff" acceptance
//! is unaffected unless a ttl is explicitly set.
//!
//! 007 impact: zero. This lives entirely inside the backend impls (adapter-private
//! interior mutability); it does not touch the reducer, SessionEvent, or the
//! `SessionBackend` trait. The FSM never sees suspend/wake — `Idle` already
//! absorbs everything, and whether the process sleeps underneath is a
//! resource-layer concern.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::sync::Mutex;
use tokio::task::{AbortHandle, JoinHandle};

use super::types::BackendError;
use crate::adapter::AgentIo;

/// The process-bound pair a suspend drops and a wake recreates: the reader task
/// draining stdout, and the `AgentIo` whose last clone-drop reaps the child
/// (`kill_on_drop`). Aborting the reader releases its `io` clone; dropping the
/// slot's `io` then reaps the process.
pub struct ProcHandle {
    pub reader: JoinHandle<()>,
    /// Held only as a drop-guard: this is the slot's strong `AgentIo` clone. It is
    /// never read directly — its job is to keep (and, when the slot drops/suspends,
    /// release) the last reference so the `ManagedProcess` is reaped (`kill_on_drop`).
    #[allow(dead_code)]
    pub io: Arc<dyn AgentIo>,
}

impl ProcHandle {
    pub fn new(reader: JoinHandle<()>, io: Arc<dyn AgentIo>) -> Self {
        Self { reader, io }
    }
}

/// Active (process live) ⇄ Dormant (closed; respawns on next dispatch).
enum Slot {
    Active(ProcHandle),
    Dormant,
}

impl Slot {
    fn is_active(&self) -> bool {
        matches!(self, Slot::Active(_))
    }
}

/// Self-suspending process lifecycle, shared (`Arc`) between the backend and its
/// idle-timer task. Single-actor: suspend and wake both take `slot`'s lock, so a
/// timer-driven close can never race a dispatch-driven wake (no TOCTOU).
pub struct SuspendController {
    slot: Mutex<Slot>,
    /// Last dispatch time (ms). Read by the idle check, written on every wake/note.
    last_activity: AtomicI64,
    /// None = never suspend (production default). Some(ttl) = close after `ttl` ms idle.
    idle_ttl_ms: Option<i64>,
    /// The current reader's abort handle, mirrored here so the backend's sync
    /// `Drop` can abort the reader WITHOUT awaiting `slot`'s async lock. Updated
    /// under `slot`'s lock on every wake/suspend so it always tracks the live reader.
    current_abort: std::sync::Mutex<Option<AbortHandle>>,
}

impl SuspendController {
    /// Start Active with an already-spawned process pair.
    pub fn active(handle: ProcHandle, idle_ttl_ms: Option<i64>, now_ms: i64) -> Self {
        let abort = handle.reader.abort_handle();
        Self {
            slot: Mutex::new(Slot::Active(handle)),
            last_activity: AtomicI64::new(now_ms),
            idle_ttl_ms,
            current_abort: std::sync::Mutex::new(Some(abort)),
        }
    }

    /// The configured idle TTL (None = never suspend). Test/diagnostic.
    #[cfg(test)]
    pub fn idle_ttl_ms(&self) -> Option<i64> {
        self.idle_ttl_ms
    }

    /// Last dispatch time (ms). Test/diagnostic.
    #[cfg(test)]
    pub fn last_activity(&self) -> i64 {
        self.last_activity.load(Ordering::SeqCst)
    }

    /// Ensure the process is live, spawning (resuming) it under the slot lock if
    /// Dormant, then refresh `last_activity`. `wake` re-runs the backend's own
    /// spawn+handshake (repopulating the shared stdin as a side effect) and
    /// returns the fresh `{reader, io}`. Held across the wake so a concurrent
    /// `suspend_if_idle` cannot interleave (single-actor). A wake `Err` leaves the
    /// slot Dormant (no half-spawned state).
    pub async fn ensure_awake<F, Fut>(&self, now_ms: i64, wake: F) -> Result<(), BackendError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ProcHandle, BackendError>>,
    {
        let mut slot = self.slot.lock().await;
        if !slot.is_active() {
            let handle = wake().await?;
            *self.current_abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.reader.abort_handle());
            *slot = Slot::Active(handle);
        }
        self.last_activity.store(now_ms, Ordering::SeqCst);
        Ok(())
    }

    /// If Active, NOT in a live turn, AND idle past `idle_ttl_ms`, close the
    /// process (abort reader → drop io → `kill_on_drop`) and go Dormant. Returns
    /// whether it suspended. No-op when `idle_ttl_ms` is None.
    ///
    /// `turn_active` is the load-bearing safety gate: a turn can stream/run tools
    /// for longer than the idle ttl (normal for a coding agent) without bumping
    /// `last_activity` (which only moves on dispatch), so without this guard the
    /// idle timer would abort the reader MID-TURN — severing the in-flight turn
    /// with no terminal and stranding the FSM in Running forever. The backend sets
    /// `turn_active` true on `dispatch(Send)` and the reader clears it at the
    /// turn's terminal; the timer passes it here so an in-flight turn is never
    /// suspended. (A hung/never-terminating turn therefore also stays resident —
    /// idle-reap is for IDLE sessions; stuck-Running is an orthogonal concern.)
    ///
    /// Taking the SAME lock as `ensure_awake` is what makes close and wake
    /// mutually exclusive: a dispatch that just refreshed `last_activity` fails the
    /// idle check and nothing is closed.
    pub async fn suspend_if_idle(&self, now_ms: i64, turn_active: bool) -> bool {
        let Some(ttl) = self.idle_ttl_ms else {
            return false;
        };
        if turn_active {
            return false; // never suspend a live turn (the reader is producing output)
        }
        let mut slot = self.slot.lock().await;
        if slot.is_active() && now_ms - self.last_activity.load(Ordering::SeqCst) >= ttl {
            if let Slot::Active(handle) = std::mem::replace(&mut *slot, Slot::Dormant) {
                handle.reader.abort(); // release the io clone the reader holds
                // `handle.io` drops here → the ManagedProcess is reaped
                // (kill_on_drop). The backend swaps in fresh stdin on the next wake,
                // so the now-dangling old stdin is released when its process dies.
            }
            *self.current_abort.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return true;
        }
        false
    }

    /// UNCONDITIONAL force-terminate (the `UserCancelTimeout` force-kill path):
    /// tear the slot Active→Dormant regardless of `idle_ttl_ms`/`turn_active`
    /// (that gating is what `suspend_if_idle` is for — here the caller has
    /// already decided to kill). Ordering is load-bearing: abort the reader
    /// FIRST, THEN group-kill the process. With the reader aborted it can no
    /// longer observe the child's stdout EOF, so the kill does not surface a
    /// `SessionEvent::Detached` that the pump would mis-read as a crash. Takes
    /// the SAME `slot` lock as `ensure_awake`/`suspend_if_idle`, so it cannot
    /// race a concurrent wake (single-actor, no TOCTOU).
    pub async fn terminate(&self) {
        let mut slot = self.slot.lock().await;
        if let Slot::Active(handle) = std::mem::replace(&mut *slot, Slot::Dormant) {
            handle.reader.abort(); // stop the reader FIRST → no Detached emitted
            handle.io.terminate().await; // group-kill the CLI process tree (Layer A)
            // `handle.io` drops here → releases the slot's io clone.
        }
        *self.current_abort.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Sync teardown for the backend's `Drop`: abort the live reader (if any) so
    /// its `AgentIo` clone releases and the child is reaped. Does NOT touch the
    /// async `slot` (Drop cannot await); the mirrored `AbortHandle` is enough.
    pub fn abort_on_drop(&self) {
        if let Some(h) = self.current_abort.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
    }

    /// Test/diagnostic: is the process currently live?
    #[cfg(test)]
    pub async fn is_active(&self) -> bool {
        self.slot.lock().await.is_active()
    }

    /// Test-only: the live reader's abort handle (mirrors the slot's reader), so a
    /// backend test can assert drop/suspend actually aborted it.
    #[cfg(test)]
    pub fn current_abort_handle(&self) -> Option<AbortHandle> {
        self.current_abort.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Spawn the per-backend idle timer: every `check_interval_ms`, ask the
/// controller to suspend if idle AND no turn is in flight. Returns None when
/// `idle_ttl_ms` is None (the production default — no timer, no suspension). The
/// task holds a `Weak` so it exits once the backend (and its controller) drops;
/// the backend also aborts the returned handle in its `Drop` (belt-and-suspenders).
/// `now` is injected for determinism in tests; `turn_active` reports whether a
/// turn is currently in flight (the backend's live turn flag) so a streaming turn
/// is never suspended mid-flight.
pub fn spawn_idle_timer<N, T, S>(
    controller: &Arc<SuspendController>,
    check_interval_ms: u64,
    now: N,
    turn_active: T,
    on_suspend: S,
) -> Option<JoinHandle<()>>
where
    N: Fn() -> i64 + Send + 'static,
    T: Fn() -> bool + Send + 'static,
    // 009 R6: fired ONCE each time an idle-reap actually suspends. The backend
    // passes a closure that emits `SessionEvent::BackendSuspended` on its event_tx
    // so the orchestrator can clear the workflow_roster (cleanup path 3). Kept as a
    // callback so suspend.rs stays decoupled from SessionEvent (FSM-invisible).
    S: Fn() + Send + 'static,
{
    controller.idle_ttl_ms?;
    let weak = Arc::downgrade(controller);
    Some(tokio::spawn(async move {
        let interval = std::time::Duration::from_millis(check_interval_ms.max(1));
        loop {
            tokio::time::sleep(interval).await;
            let Some(ctrl) = weak.upgrade() else {
                break; // backend dropped → stop the timer
            };
            if ctrl.suspend_if_idle(now(), turn_active()).await {
                on_suspend();
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CountingTerminateIo, FakeAgentIo};
    use std::sync::atomic::AtomicUsize;

    /// Build a `ProcHandle` over a never-exiting fake process + a reader task that
    /// just parks (so abort is observable). Returns the handle + an abort handle to
    /// assert the reader's liveness.
    fn fake_handle() -> (ProcHandle, AbortHandle) {
        let io: Arc<dyn AgentIo> = Arc::from(Box::new(FakeAgentIo::never_exits(Vec::new())) as Box<dyn AgentIo>);
        let reader = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let abort = reader.abort_handle();
        (ProcHandle::new(reader, io), abort)
    }

    #[tokio::test]
    async fn off_by_default_never_suspends() {
        let (h, _a) = fake_handle();
        let ctrl = SuspendController::active(h, None, 0);
        assert!(ctrl.is_active().await);
        // Even long past any ttl, suspend is a no-op when idle_ttl is None.
        assert!(!ctrl.suspend_if_idle(1_000_000, false).await);
        assert!(
            ctrl.is_active().await,
            "None ttl → stays Active forever (production parity)"
        );
    }

    #[tokio::test]
    async fn suspends_after_ttl_then_wakes() {
        let (h, abort0) = fake_handle();
        let ctrl = SuspendController::active(h, Some(100), 0);

        // Not idle yet (50 < 100) → no suspend.
        assert!(!ctrl.suspend_if_idle(50, false).await);
        assert!(ctrl.is_active().await);

        // Idle past ttl → suspends + aborts the reader.
        assert!(ctrl.suspend_if_idle(150, false).await);
        assert!(!ctrl.is_active().await);
        for _ in 0..40 {
            if abort0.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            abort0.is_finished(),
            "suspend aborts the old reader (releases its io clone)"
        );

        // Next dispatch wakes via the backend's wake closure.
        let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = woke.clone();
        ctrl.ensure_awake(200, || async move {
            w.store(true, Ordering::SeqCst);
            let (h2, _) = fake_handle();
            Ok(h2)
        })
        .await
        .unwrap();
        assert!(woke.load(Ordering::SeqCst), "Dormant → wake closure ran");
        assert!(ctrl.is_active().await, "woke back to Active");
        assert_eq!(ctrl.last_activity(), 200);
    }

    #[tokio::test]
    async fn recent_dispatch_blocks_suspend_no_toctou() {
        // The single-actor invariant: a dispatch that just refreshed last_activity
        // (via ensure_awake on an already-Active slot) prevents an idle-close.
        let (h, _a) = fake_handle();
        let ctrl = SuspendController::active(h, Some(100), 0);
        // ensure_awake on an Active slot just refreshes last_activity (no wake).
        ctrl.ensure_awake(100, || async { panic!("must not wake an Active slot") })
            .await
            .unwrap();
        // last_activity=100; idle check at 150 sees only 50ms → must NOT close.
        assert!(!ctrl.suspend_if_idle(150, false).await);
        assert!(ctrl.is_active().await);
    }

    #[tokio::test]
    async fn turn_active_blocks_suspend_even_when_idle() {
        // The #1-critical guard: a long-running turn keeps last_activity pinned at
        // dispatch time (it only moves on Send), so without the turn_active gate the
        // idle timer would abort the reader mid-turn. With the gate, an idle-past-ttl
        // check that reports turn_active=true must NOT close the slot.
        let (h, _a) = fake_handle();
        let ctrl = SuspendController::active(h, Some(100), 0);
        // Way past the ttl, but a turn is in flight → must stay Active.
        assert!(!ctrl.suspend_if_idle(10_000, true).await, "never suspend a live turn");
        assert!(ctrl.is_active().await, "live turn kept the process resident");
        // Once the turn ends (turn_active=false) the same idle check suspends.
        assert!(ctrl.suspend_if_idle(10_000, false).await);
        assert!(!ctrl.is_active().await);
    }

    #[tokio::test]
    async fn wake_failure_leaves_dormant() {
        let (h, _a) = fake_handle();
        let ctrl = SuspendController::active(h, Some(100), 0);
        assert!(ctrl.suspend_if_idle(200, false).await);
        let r = ctrl
            .ensure_awake(300, || async { Err(BackendError::Transport("boom".into())) })
            .await;
        assert!(matches!(r, Err(BackendError::Transport(_))));
        assert!(
            !ctrl.is_active().await,
            "failed wake leaves the slot Dormant, not half-spawned"
        );
    }

    /// Build an Active controller over a terminate-counting io + a parked reader.
    /// Returns (controller, reader abort handle, terminate-call counter).
    fn counting_ctrl() -> (SuspendController, AbortHandle, Arc<AtomicUsize>) {
        let io = CountingTerminateIo::new();
        let counter = io.terminate_counter();
        let io: Arc<dyn AgentIo> = Arc::from(Box::new(io) as Box<dyn AgentIo>);
        let reader = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let abort = reader.abort_handle();
        let ctrl = SuspendController::active(ProcHandle::new(reader, io), None, 0);
        (ctrl, abort, counter)
    }

    /// T1 (spec §10.2): `terminate` aborts the live reader FIRST and group-kills
    /// the io EXACTLY ONCE, then leaves the slot Dormant with `current_abort`
    /// cleared — the unconditional Active→Dormant teardown for the force-kill path.
    #[tokio::test]
    async fn terminate_aborts_reader_and_group_kills_once() {
        let (ctrl, abort, counter) = counting_ctrl();
        assert!(ctrl.is_active().await);

        ctrl.terminate().await;

        for _ in 0..40 {
            if abort.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(abort.is_finished(), "terminate aborts the live reader");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "terminate group-kills the io exactly once"
        );
        assert!(!ctrl.is_active().await, "terminate leaves the slot Dormant");
        assert!(ctrl.current_abort_handle().is_none(), "terminate clears current_abort");
    }

    /// `terminate` on an already-Dormant slot is a no-op: no io.terminate, no panic.
    #[tokio::test]
    async fn terminate_on_dormant_is_noop() {
        let (ctrl, _abort, counter) = counting_ctrl();
        ctrl.terminate().await; // Active → Dormant, counts once
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        ctrl.terminate().await; // already Dormant → no-op
        assert_eq!(counter.load(Ordering::SeqCst), 1, "second terminate is a no-op");
    }

    #[tokio::test]
    async fn abort_on_drop_aborts_live_reader() {
        let (h, abort0) = fake_handle();
        let ctrl = SuspendController::active(h, None, 0);
        assert!(!abort0.is_finished());
        ctrl.abort_on_drop();
        for _ in 0..40 {
            if abort0.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(abort0.is_finished(), "abort_on_drop aborts the live reader");
    }

    #[tokio::test]
    async fn idle_timer_not_spawned_when_off() {
        let (h, _a) = fake_handle();
        let ctrl = Arc::new(SuspendController::active(h, None, 0));
        assert!(
            spawn_idle_timer(&ctrl, 10, || 0, || false, || {}).is_none(),
            "no timer when idle_ttl is None"
        );
    }

    #[tokio::test]
    async fn idle_timer_suspends_when_on() {
        let (h, abort0) = fake_handle();
        let ctrl = Arc::new(SuspendController::active(h, Some(20), 0));
        // A monotonically-advancing clock so the timer eventually crosses the ttl.
        let clock = Arc::new(AtomicI64::new(0));
        let c = clock.clone();
        // 009 R6: the on_suspend callback must fire exactly when an idle-reap happens.
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = fired.clone();
        let timer = spawn_idle_timer(
            &ctrl,
            5,
            move || c.fetch_add(50, Ordering::SeqCst) + 50,
            || false,
            move || f.store(true, Ordering::SeqCst),
        )
        .expect("timer spawned when ttl is Some");
        // Within ~1s the timer must observe idle and suspend.
        let mut suspended = false;
        for _ in 0..100 {
            if !ctrl.is_active().await {
                suspended = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        timer.abort();
        assert!(suspended, "the idle timer suspended the idle process");
        assert!(abort0.is_finished(), "timer-driven suspend aborted the reader");
        assert!(
            fired.load(Ordering::SeqCst),
            "009 R6: on_suspend fired on the idle-reap"
        );
    }
}
