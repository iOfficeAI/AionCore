//! Kill + identity-gated liveness probing (IC-1 core).
//!
//! The crate spawns each child as its own process-group leader, so teardown
//! can `kill(-pgid, SIGKILL)` the whole subtree. Before reaping a *persisted*
//! pgid (which may have been recycled by the OS onto an unrelated live
//! process), we re-read the live process start-time and require it to MATCH
//! what we recorded — otherwise we prune the registry entry and never kill.
//!
//! `pgid > 1` blast-floor everywhere (IC-6): never `kill(-1)` / `kill(-0)`.

use uuid::Uuid;

use crate::ProcessError;

/// Result of an identity-gated liveness probe (the pure decision input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Live, recorded epoch differs from current (a prior-run orphan), and the
    /// process start-time matches what we recorded — safe to reap.
    Match,
    /// A live process holds the pid, but its start-time differs from what we
    /// recorded → the PID was recycled onto an unrelated process. Prune only.
    RecycledPid,
    /// Live and start-time matches, but the recorded epoch == current epoch →
    /// this is one of THIS run's own processes, never a reap target.
    DiffEpoch,
    /// No process holds the pid (ESRCH). Prune only.
    Gone,
    /// Could not determine start-time on this platform (macOS/Windows fallback)
    /// or the recorded start-time was absent. Prune only — never kill on doubt.
    Unknown,
    /// `kill(pid, 0)` returned EPERM: some process holds the id but it is not
    /// provably ours. Prune only.
    EpermAlive,
}

/// Pure identity-gate decision. Reads NOTHING from the OS — the caller supplies
/// the observed start-time (from [`read_process_start_time`]). This is the
/// Tier-A-exhaustible core of IC-1.
///
/// Reap is permitted ONLY when the process is alive, its start-time matches the
/// recorded one, and the recorded epoch is from a *prior* run.
pub fn classify_liveness(
    recorded_start_ticks: Option<u64>,
    observed: ObservedLiveness,
    recorded_epoch: Uuid,
    current_epoch: Uuid,
) -> Liveness {
    match observed {
        ObservedLiveness::Gone => Liveness::Gone,
        ObservedLiveness::EpermAlive => Liveness::EpermAlive,
        ObservedLiveness::Alive { start_ticks } => {
            match (recorded_start_ticks, start_ticks) {
                // We could not record or cannot observe a start-time → never kill.
                (None, _) | (_, None) => Liveness::Unknown,
                // F57: `0` is a NON-identity sentinel. A real recorder never
                // emits 0 as a legitimate discriminator (Linux starttime jiffies
                // and the macOS (tvsec<<20)|tvusec packing are non-zero for any
                // post-boot process); a `Some(0)` therefore means corrupt /
                // zeroed data. Treating two zeros as a Match would AUTHORIZE A
                // KILL on garbage — the one value where the safe-failing argument
                // breaks. Refuse to gate on 0 → Unknown (prune-only, never kill).
                (Some(0), _) | (_, Some(0)) => Liveness::Unknown,
                (Some(rec), Some(obs)) if rec == obs => {
                    if recorded_epoch == current_epoch {
                        Liveness::DiffEpoch // our own current-run process
                    } else {
                        Liveness::Match // prior-run orphan, identity-confirmed
                    }
                }
                // Same pid, different start-time → recycled.
                (Some(_), Some(_)) => Liveness::RecycledPid,
            }
        }
    }
}

/// What an OS probe observed about a pid, fed into [`classify_liveness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedLiveness {
    /// The pid is live; `start_ticks` is its kernel start-time if obtainable.
    Alive { start_ticks: Option<u64> },
    /// No process holds the pid (ESRCH).
    Gone,
    /// `kill(pid, 0)` returned EPERM.
    EpermAlive,
}

/// Probe a pid's liveness + start-time via the OS (the impure shell around
/// [`classify_liveness`]).
pub fn probe(pid: u32) -> ObservedLiveness {
    #[cfg(unix)]
    {
        // Refuse to wrap: a pid that doesn't fit positive i32 would, as a
        // negative target, probe a GROUP instead of the process (F50). Treat as
        // not-observable → Gone (prune-only; never authorizes a kill).
        let Some(target) = pid_signal_target(pid) else {
            return ObservedLiveness::Gone;
        };
        let rc = unsafe { libc::kill(target, 0) };
        if rc == 0 {
            return ObservedLiveness::Alive {
                start_ticks: read_process_start_time(pid),
            };
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => ObservedLiveness::Gone,
            Some(libc::EPERM) => ObservedLiveness::EpermAlive,
            _ => ObservedLiveness::Gone,
        }
    }
    #[cfg(windows)]
    {
        windows_impl::probe(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        // No cheap probe on an unknown platform; Unknown-alive so the gate prunes.
        ObservedLiveness::Alive { start_ticks: None }
    }
}

/// Read a process's kernel start-time in platform clock ticks, if obtainable.
/// `None` on platforms where we have no cheap accessor (→ identity gate
/// downgrades to Unknown → prune-only).
pub fn read_process_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat field 22 (starttime), after the (comm) field which
        // may itself contain spaces/parens — split on the last ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = stat.rsplit_once(')')?.1;
        let starttime = after.split_whitespace().nth(19)?; // field 22 = index 19 after comm
        starttime.parse::<u64>().ok()
    }
    #[cfg(target_os = "macos")]
    {
        read_start_time_macos(pid)
    }
    #[cfg(target_os = "windows")]
    {
        windows_impl::read_start_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        // Unknown platform: no cheap accessor → None keeps the identity gate
        // conservative (prune, never kill on doubt).
        let _ = pid;
        None
    }
}

/// macOS kernel process start-time via `proc_pidinfo(PROC_PIDTBSDINFO)`, packed
/// as `(tvsec << 20) | (tvusec & 0xF_FFFF)` to keep sub-second resolution in a
/// single `u64` without touching the platform-agnostic pure core (design
/// Decision 1/5). `tvusec` < 1_000_000 < 2^20, so the low 20 bits hold it
/// losslessly; the comparison is equality-only so the exact packing is opaque
/// to `classify_liveness`. `None` on any failure → identity gate stays
/// conservative (prune, never kill).
#[cfg(target_os = "macos")]
fn read_start_time_macos(pid: u32) -> Option<u64> {
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`; we pass the
    // exact size of the zeroed struct and only read it back on the documented
    // success return (bytes-written == struct size).
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        // 0 / -1 / short read → could not obtain a trustworthy start-time.
        return None;
    }
    let secs = info.pbi_start_tvsec;
    let usecs = info.pbi_start_tvusec & 0xF_FFFF; // < 2^20, fits the low 20 bits
    Some((secs << 20) | usecs)
}

/// The reaper's OWN process group id, if obtainable. Used to refuse killing a
/// pgid we ourselves belong to (would self-SIGKILL). `None` on non-unix or if
/// the value would not fit a `u32` (defensive).
pub(crate) fn current_process_group() -> Option<u32> {
    #[cfg(unix)]
    {
        let pgrp = unsafe { libc::getpgrp() };
        u32::try_from(pgrp).ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The `libc::kill` target for SIGNALLING A SINGLE PID: the pid as a positive
/// `i32`. Returns `None` if the pid does not fit in the positive `i32` range
/// (≥ 2^31) — `pid as i32` would wrap NEGATIVE and accidentally signal a process
/// GROUP instead of the single process (F50). Pure; unit-tested.
#[cfg(unix)]
pub(crate) fn pid_signal_target(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|t| *t > 0)
}

/// The `libc::kill` target for SIGNALLING A WHOLE GROUP: the negative pgid.
/// Returns `None` if pgid ≤ 1 (IC-6 blast-floor) OR pgid ≥ 2^31 — in the latter
/// case `pgid as i32` wraps negative and `-(negative)` becomes POSITIVE, so the
/// group-kill would silently collapse into a single-PID kill and leak the
/// grandchild subtree (F50). Pure; unit-tested.
#[cfg(unix)]
pub(crate) fn group_kill_target(pgid: u32) -> Option<i32> {
    if pgid <= 1 {
        return None;
    }
    // Must fit positive i32 so that negation is a well-defined negative target.
    i32::try_from(pgid).ok().map(|p| -p)
}

/// Force-kill a process group (or the bare pid if no group), best-effort.
/// `ESRCH` (already gone) is treated as success. Honors the `pgid > 1`
/// blast-floor (IC-6) and refuses to wrap large ids (F50).
pub fn force_kill(pid: u32, process_group_id: Option<u32>) -> Result<(), ProcessError> {
    #[cfg(unix)]
    {
        if let Some(target) = process_group_id.and_then(group_kill_target) {
            return kill_target(target).or_else(|e| {
                // Group leader may already be gone; fall back to the bare pid.
                if e.already_gone {
                    match pid_signal_target(pid) {
                        Some(t) => kill_target(t).map_err(|e| e.into()),
                        None => Ok(()), // pid out of i32 range — nothing safe to do
                    }
                } else {
                    Err(e.into())
                }
            });
        }
        match pid_signal_target(pid) {
            Some(t) => kill_target(t).map_err(Into::into),
            None => Err(ProcessError::internal(format!("pid {pid} out of signalable i32 range"))),
        }
    }
    #[cfg(windows)]
    {
        // Windows COLD-REAP path (from a persisted registry pid, no live Job
        // handle): single-process TerminateProcess. The Job Object does NOT
        // persist across the owner's death, so a recycled-from-disk pid can only
        // be terminated as a single process — grandchildren are not reachable
        // here (I-9, documented). HOT kill (live ManagedProcess in hand) goes
        // through the Job in process.rs and DOES kill the whole subtree.
        let _ = process_group_id; // no pgid concept on Windows
        windows_impl::terminate_process(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, process_group_id);
        Err(ProcessError::internal("force_kill not supported on this platform"))
    }
}

#[cfg(unix)]
struct KillErr {
    already_gone: bool,
    msg: String,
}

#[cfg(unix)]
impl From<KillErr> for ProcessError {
    fn from(e: KillErr) -> Self {
        ProcessError::internal(e.msg)
    }
}

#[cfg(unix)]
fn kill_target(target: i32) -> Result<(), KillErr> {
    let rc = unsafe { libc::kill(target, libc::SIGKILL) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(()); // already gone == success
    }
    Err(KillErr {
        already_gone: false,
        msg: format!("SIGKILL to {target} failed: {err}"),
    })
}

/// Liveness of a process group by signal-0 to the negative pgid (Unix).
/// `true` if the group still has any member. Non-Unix / no pgid → false.
pub fn process_group_alive(process_group_id: Option<u32>) -> bool {
    #[cfg(unix)]
    {
        match process_group_id.and_then(group_kill_target) {
            Some(target) => {
                let rc = unsafe { libc::kill(target, 0) };
                if rc == 0 {
                    return true;
                }
                !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH))
            }
            None => false,
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no process-group concept (containment is a Job Object, and
        // `process_group_id` is always None here). Group-liveness is therefore
        // not meaningful; callers needing Windows liveness use `probe(pid)`
        // instead. Returns false (no group to be alive).
        let _ = process_group_id;
        false
    }
}

/// Windows Win32 FFI: liveness/identity probe + single-process termination
/// (cold-reap). Job Object subtree containment lives in aionui-runtime's
/// spawn.rs (it owns the Command). windows-sys raw FFI (Decision 2).
#[cfg(windows)]
mod windows_impl {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
        WaitForSingleObject,
    };

    use super::{ObservedLiveness, ProcessError};

    /// `SYNCHRONIZE` access right (0x0010_0000). Required for `WaitForSingleObject`
    /// on a process handle. windows-sys only re-exports the const under
    /// `Storage::FileSystem` (typed `FILE_ACCESS_RIGHTS`), so we use the literal —
    /// it is the standard-rights bit, identical across object types.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    /// Open a handle with the given access. `Ok(Some(h))` = opened (caller MUST
    /// CloseHandle it — it is OUR handle, NOT a tokio-owned Child handle).
    /// `Ok(None)` = the pid is genuinely gone (ERROR-not-access-denied).
    /// `Err(())` = access denied (a process holds the id but we may not query it).
    fn open(pid: u32, access: u32) -> Result<Option<*mut core::ffi::c_void>, ()> {
        // SAFETY: OpenProcess returns a handle or null on failure.
        let h = unsafe { OpenProcess(access, 0, pid) };
        if !h.is_null() {
            return Ok(Some(h));
        }
        // SAFETY: GetLastError reads the calling thread's last error.
        let err = unsafe { GetLastError() };
        if err == ERROR_ACCESS_DENIED { Err(()) } else { Ok(None) }
    }

    /// Liveness + identity (creation-time) probe.
    ///
    /// The handle MUST be opened with `SYNCHRONIZE` in addition to
    /// `PROCESS_QUERY_LIMITED_INFORMATION` — `WaitForSingleObject` requires
    /// SYNCHRONIZE, and without it the wait returns `WAIT_FAILED`, which would
    /// make a LIVE process look gone (the real-Windows bug this fixes: a fresh
    /// child probed as `Gone`).
    ///
    /// `OpenProcess` null + access-denied ⇒ `EpermAlive` (something holds the id,
    /// not provably ours). `OpenProcess` null + other ⇒ `Gone`.
    /// `WaitForSingleObject == WAIT_TIMEOUT` ⇒ still running (Alive);
    /// `WAIT_OBJECT_0` (signaled) ⇒ exited (Gone). Any other wait result
    /// (incl. WAIT_FAILED) is treated conservatively as Gone but only AFTER we
    /// confirmed the handle opened — a real failure here is logged by the caller.
    pub(super) fn probe(pid: u32) -> ObservedLiveness {
        let h = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE) {
            Ok(Some(h)) => h,
            Ok(None) => return ObservedLiveness::Gone,
            Err(()) => return ObservedLiveness::EpermAlive,
        };
        // SAFETY: h is a valid handle (opened with SYNCHRONIZE) we own; closed below.
        let wait = unsafe { WaitForSingleObject(h, 0) };
        let start = read_creation_token(h);
        // SAFETY: close the handle WE opened (not a tokio Child handle).
        unsafe { CloseHandle(h) };
        if wait == WAIT_TIMEOUT {
            // Still running. Avoids the GetExitCodeProcess STILL_ACTIVE(259)
            // ambiguity by using WaitForSingleObject as the liveness oracle.
            ObservedLiveness::Alive { start_ticks: start }
        } else if wait == WAIT_OBJECT_0 {
            ObservedLiveness::Gone // signaled = exited
        } else {
            // WAIT_FAILED or unexpected — conservative Gone (prune-only, never
            // authorizes a kill). With SYNCHRONIZE present this should not occur
            // for a live process.
            ObservedLiveness::Gone
        }
    }

    /// Creation-time identity token: combine the FILETIME's two u32 halves into
    /// a u64 (100ns ticks since 1601). Stable for the process's lifetime; a
    /// recycled PID gets a different creation time → the identity gate's
    /// `(pid, token)` pair defeats PID reuse. `None` on any failure.
    pub(super) fn read_start_time(pid: u32) -> Option<u64> {
        // GetProcessTimes needs only QUERY_LIMITED (no SYNCHRONIZE). open() returns
        // Ok(Some)=opened / Ok(None)=gone / Err=access-denied; only Some yields a token.
        let h = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            Ok(Some(h)) => h,
            Ok(None) | Err(()) => return None,
        };
        let token = read_creation_token(h);
        // SAFETY: close the handle we opened.
        unsafe { CloseHandle(h) };
        token
    }

    fn read_creation_token(h: *mut core::ffi::c_void) -> Option<u64> {
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut user = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // SAFETY: all four out-params are valid; h is a live handle.
        let ok = unsafe { GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }
        let token = ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
        // A zeroed creation time is not a trustworthy identity (F57: 0 is the
        // non-identity sentinel the pure gate refuses); treat as unobtainable.
        if token == 0 { None } else { Some(token) }
    }

    /// Cold-reap single-process termination (I-9) via `OpenProcess(PROCESS_TERMINATE)`
    /// then `TerminateProcess`. A pid that is already gone (OpenProcess fails) is
    /// success. Does NOT reach grandchildren — that needs the live Job handle
    /// (hot path), which a cold reap from disk does not have.
    pub(super) fn terminate_process(pid: u32) -> Result<(), ProcessError> {
        let h = match open(pid, PROCESS_TERMINATE) {
            Ok(Some(h)) => h,
            // already gone == success (like ESRCH); access-denied → nothing safe
            // to do, also treat as success (best-effort cold reap).
            Ok(None) | Err(()) => return Ok(()),
        };
        // SAFETY: h is a valid handle with PROCESS_TERMINATE; closed below.
        let ok = unsafe { TerminateProcess(h, 1) };
        let err = if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: close the handle we opened.
        unsafe { CloseHandle(h) };
        match err {
            None => Ok(()),
            Some(e) => Err(ProcessError::internal(format!("TerminateProcess({pid}) failed: {e}"))),
        }
    }
}
