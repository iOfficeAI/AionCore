//! Process-tree enumeration, for reaping descendants that left the group.
//!
//! A process-group SIGKILL only reaches members of that group. A tool
//! subprocess that called `setsid`/`setpgid` is in a different group and
//! survives — measured with agy 1.1.9, whose tool children each become their
//! own group leader:
//!
//! ```text
//! agy    pid=83547  pgid=83547
//! sleep  pid=83709  ppid=83547  pgid=83709   <- child of agy, own group
//! kill -9 -83547  =>  agy gone, sleep alive with PPID=1
//! ```
//!
//! The parent link is the one thing that still connects them, and it is lost
//! the moment the parent dies (the kernel reparents orphans to init). So the
//! set has to be captured BEFORE the kill, not looked up after it.

/// Never signal these, whatever a parent table claims.
///
/// 0 is "the caller's own group" to `kill(2)` and 1 is init; either would be a
/// catastrophic target if a malformed table ever named them.
const RESERVED_PIDS: [u32; 2] = [0, 1];

/// Bound on how deep the walk follows parent links.
///
/// The table is a snapshot of a live system, so it can be internally
/// inconsistent (a pid recycled between two reads can appear to be its own
/// ancestor). The visited-set already prevents a cycle from looping forever;
/// this is the second, blunter stop so a pathological table cannot make
/// teardown crawl.
const MAX_DEPTH: usize = 64;

/// Descendants of `root`, nearest-first, excluding `root` itself.
///
/// Pure over an already-captured `(pid, ppid)` table so the walk is testable
/// without spawning real process trees.
pub(crate) fn collect_descendants(root: u32, parents: &[(u32, u32)]) -> Vec<u32> {
    if RESERVED_PIDS.contains(&root) {
        return Vec::new();
    }
    let mut out: Vec<u32> = Vec::new();
    let mut frontier: Vec<u32> = vec![root];
    let mut seen: Vec<u32> = vec![root];

    for _ in 0..MAX_DEPTH {
        let mut next: Vec<u32> = Vec::new();
        for (pid, ppid) in parents {
            if !frontier.contains(ppid) || seen.contains(pid) || RESERVED_PIDS.contains(pid) {
                continue;
            }
            seen.push(*pid);
            next.push(*pid);
            out.push(*pid);
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

/// Live `(pid, ppid)` table, or empty when the platform gives us no cheap
/// accessor. Empty means "reap nothing extra" — never "nothing is running".
pub(crate) fn parent_table() -> Vec<(u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        linux_parent_table()
    }
    #[cfg(target_os = "macos")]
    {
        macos_parent_table()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn linux_parent_table() -> Vec<(u32, u32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Split on the LAST ')': the comm field is parenthesised and may itself
        // contain spaces and parens, so field counting only works after it.
        let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        // After comm: state, ppid, ... => ppid is the 2nd whitespace token.
        if let Some(ppid) = after.split_whitespace().nth(1).and_then(|v| v.parse::<u32>().ok()) {
            out.push((pid, ppid));
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn macos_parent_table() -> Vec<(u32, u32)> {
    // `<sys/proc_info.h>`: PROC_ALL_PIDS = 1. Not re-exported by the libc crate.
    const PROC_ALL_PIDS: u32 = 1;

    // SAFETY: called with a null buffer first, which is the documented way to
    // ask proc_listpids for the required byte count without writing anything.
    let needed = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let count = needed as usize / std::mem::size_of::<libc::c_int>();
    // Headroom: processes can appear between the sizing call and the read.
    let mut pids: Vec<libc::c_int> = vec![0; count + 32];
    let cap = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
    // SAFETY: the buffer is `cap` bytes and proc_listpids writes at most that
    // many; the returned byte count bounds what we then read back.
    let written = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), cap) };
    if written <= 0 {
        return Vec::new();
    }
    let n = (written as usize / std::mem::size_of::<libc::c_int>()).min(pids.len());

    let mut out = Vec::with_capacity(n);
    for &raw in &pids[..n] {
        let Ok(pid) = u32::try_from(raw) else { continue };
        if pid == 0 {
            continue; // proc_listpids pads the tail with zeroes
        }
        // SAFETY: proc_pidinfo writes at most `size` bytes into the zeroed
        // struct; only read back on the documented success return.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                raw,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        if got == size {
            out.push((pid, info.pbi_ppid));
        }
    }
    out
}

/// Descendants of `pid` paired with their start-times, captured for a later
/// sweep.
///
/// Must be called BEFORE the kill: the parent link is the only thing tying an
/// escaped child to this tree, and the kernel severs it by reparenting orphans
/// to init the instant their parent dies.
///
/// Not filtered to processes outside `group`: one still inside it is already
/// dead by sweep time, and `SIGKILL` to a gone pid is a no-op (`ESRCH` is
/// success). Filtering would need a second per-pid accessor to buy nothing.
pub(crate) fn escaped_descendants(pid: u32, _group: Option<u32>) -> Vec<(u32, Option<u64>)> {
    let table = parent_table();
    collect_descendants(pid, &table)
        .into_iter()
        .map(|p| (p, crate::read_process_start_time(p)))
        .collect()
}

/// `SIGKILL` snapshot members that are still the same process; returns how many
/// are STILL alive afterwards.
///
/// Identity is start-time equality. Between the snapshot and the sweep a pid
/// may have been recycled onto something unrelated, and killing that is far
/// worse than leaking — so anything not positively identified (no recorded
/// start-time, none observable, or a mismatch) is left alone, matching this
/// crate's standing "never kill on doubt" rule.
pub(crate) fn reap(snapshot: &[(u32, Option<u64>)]) -> usize {
    let own_pid = std::process::id();
    let mut still_alive = 0usize;
    for &(pid, recorded) in snapshot {
        if pid == own_pid {
            continue; // never signal the reaper itself
        }
        let observed = match crate::probe(pid) {
            crate::ObservedLiveness::Alive { start_ticks } => start_ticks,
            crate::ObservedLiveness::Gone => continue,
            crate::ObservedLiveness::EpermAlive => {
                still_alive += 1;
                continue;
            }
        };
        let same_process = match (recorded, observed) {
            // `0` is this crate's non-identity sentinel (see `classify_liveness`):
            // corrupt data, not a discriminator.
            (Some(0), _) | (_, Some(0)) | (None, _) | (_, None) => false,
            (Some(rec), Some(obs)) => rec == obs,
        };
        if !same_process {
            still_alive += 1;
            continue;
        }
        if crate::force_kill(pid, None).is_err() {
            still_alive += 1;
        }
    }
    still_alive
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_in_its_own_group_is_still_found_through_its_parent() {
        // The measured agy shape: the tool child is invisible to a group kill
        // but is still a child of the CLI while the CLI is alive.
        let table = [(83547, 45175), (83709, 83547)];
        assert_eq!(collect_descendants(83547, &table), vec![83709]);
    }

    #[test]
    fn grandchildren_are_reaped_too() {
        // An MCP server started by a tool the agent started.
        let table = [(200, 100), (300, 200), (400, 300)];
        assert_eq!(collect_descendants(100, &table), vec![200, 300, 400]);
    }

    #[test]
    fn unrelated_processes_are_never_touched() {
        let table = [(200, 100), (999, 1), (1000, 999)];
        assert_eq!(collect_descendants(100, &table), vec![200]);
    }

    #[test]
    fn the_root_never_reaps_itself() {
        // The caller kills the root through the group path; returning it here
        // would double-signal and, worse, invite callers to treat the list as
        // "everything to kill" including a pid they already handled.
        let table = [(100, 1), (200, 100)];
        let d = collect_descendants(100, &table);
        assert!(!d.contains(&100), "{d:?}");
    }

    #[test]
    fn init_and_pid_zero_are_never_returned() {
        // kill(0, …) signals OUR OWN group and pid 1 is init: a malformed table
        // naming either must not be able to turn teardown into a self-kill.
        let table = [(1, 0), (0, 0), (200, 100)];
        assert_eq!(collect_descendants(0, &table), Vec::<u32>::new());
        assert_eq!(collect_descendants(1, &table), Vec::<u32>::new());
        let d = collect_descendants(100, &table);
        assert!(!d.contains(&0) && !d.contains(&1), "{d:?}");
    }

    #[test]
    fn a_cyclic_table_terminates() {
        // Two pids claiming each other as parent — possible when a pid is
        // recycled between two reads of a live table.
        let table = [(200, 100), (100, 200)];
        let d = collect_descendants(100, &table);
        assert_eq!(d, vec![200], "{d:?}");
    }

    #[test]
    fn a_process_with_no_children_yields_nothing() {
        assert_eq!(collect_descendants(100, &[(200, 300)]), Vec::<u32>::new());
    }

    #[test]
    fn the_live_table_sees_this_test_process() {
        // Guards the platform readers: an accessor that silently returns an
        // empty table would make the whole reap a no-op and every test above
        // would still pass.
        let table = parent_table();
        if table.is_empty() {
            return; // platform without a cheap accessor; documented as no-op
        }
        let me = std::process::id();
        assert!(table.iter().any(|(pid, _)| *pid == me), "own pid missing from table");
    }
}
