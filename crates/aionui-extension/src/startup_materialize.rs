//! Startup-time materialization of the embedded builtin skills corpus to
//! `{data_dir}/builtin-skills/`. Gated on a `.version` file so repeat
//! starts with the same binary skip the rewrite.
//!
//! Algorithm:
//!   staging = data_dir/.builtin-skills.tmp (fresh each call)
//!   write all BUILTIN_SKILLS entries into staging
//!   write staging/.version ← binary version
//!   atomic rename(target → .builtin-skills.old, staging → target)
//!   best-effort remove .builtin-skills.old
//!
//! The atomic rename guarantees that concurrent backend processes, or a
//! crash mid-write, never observe a half-populated target — the old tree
//! stays in place until staging is fully ready.

use std::fs::OpenOptions;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use include_dir::Dir;
use tracing::{debug, error, info, warn};

use crate::error::ExtensionError;

const VERSION_FILE: &str = ".version";
const LOCK_FILE_NAME: &str = ".builtin-skills.lock";
const STAGING_DIR_NAME: &str = ".builtin-skills.tmp";
const OLD_DIR_NAME: &str = ".builtin-skills.old";

/// Total budget for acquiring the builtin-skills materialization lock.
///
/// Why 15s: this lock is taken *after* the HTTP listener is bound and the
/// `AIONCORE_LISTENING <port>` line has already been printed (aionui-app
/// `async_main` binds the listener, then calls `init_data_layer`). So the
/// parent process is no longer in its port-report window (60s) — it is in its
/// `/health` polling window, which AionUi caps at 30s
/// (`waitForHealth(port, timeoutMs = 30_000)` in
/// `packages/web-host/src/backend-launcher.ts`) before it SIGKILLs the
/// backend. A 15s budget sits at half of the parent's patience: long enough to
/// ride out a peer that is legitimately mid-materialization, and short enough
/// that a timeout still leaves ~15s for the error to surface on stderr as a
/// parseable `BOOTSTRAP_DATA_INIT_FAILED stage=data.builtin_skills` boundary
/// line, instead of the user getting an unexplained SIGKILL (AIONUI-168).
const MATERIALIZE_LOCK_BUDGET: Duration = Duration::from_secs(15);

/// Poll interval while a peer holds the materialize lock. 150ms makes the
/// hand-off latency negligible against the budget while keeping the retry
/// count around 100 for a full 15s wait.
const MATERIALIZE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(150);

const STARTUP_FILE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// Decide whether to materialize based on the `.version` file, then do it.
/// Returns `true` if a write happened, `false` if the gate said "skip".
///
/// When `BUILTIN_SKILLS_ENV_VAR` is set and non-empty, the caller has
/// already routed `builtin_skills_dir` at the env-var path — this
/// function still runs but the gate will see whatever version the dev
/// tree has on disk (or missing, and materialize into that dev path,
/// which is wrong). Callers MUST check the env var before calling.
pub async fn materialize_if_needed(
    data_dir: &Path,
    corpus: &Dir<'static>,
    binary_version: &str,
) -> Result<bool, ExtensionError> {
    let target = data_dir.join(crate::constants::BUILTIN_SKILLS_DIR_NAME);

    if version_file_matches(&target, binary_version).await {
        info!(
            target = %target.display(),
            version = binary_version,
            "builtin skills up to date; skipping materialize"
        );
        return Ok(false);
    }

    info!(
        target = %target.display(),
        version = binary_version,
        "materializing embedded builtin skills"
    );
    let _guard = MaterializeLockGuard::acquire(data_dir).await?;
    if version_file_matches(&target, binary_version).await {
        info!(
            target = %target.display(),
            version = binary_version,
            "builtin skills up to date after materialize lock; skipping rewrite"
        );
        return Ok(false);
    }

    match materialize_embedded_builtin_skills_unlocked(data_dir, corpus, binary_version).await {
        Ok(()) => {}
        Err(e) if existing_builtin_skills_looks_usable(&target).await => {
            warn!(
                target = %target.display(),
                version = binary_version,
                error = %e,
                "failed to refresh builtin skills; continuing with existing tree"
            );
            return Ok(false);
        }
        Err(e) => return Err(e),
    }
    Ok(true)
}

/// Read `.version` and compare against the provided `binary_version`.
/// Returns `true` only on exact match. Missing file / IO error /
/// mismatch all return `false`.
async fn version_file_matches(target: &Path, binary_version: &str) -> bool {
    let version_path = target.join(VERSION_FILE);
    match tokio::fs::read_to_string(&version_path).await {
        Ok(s) => s == binary_version,
        Err(_) => false,
    }
}

/// Unconditional materialize: stage, write each file, atomic rename.
/// Exposed separately for tests that want to bypass the gate.
pub async fn materialize_embedded_builtin_skills(
    data_dir: &Path,
    corpus: &Dir<'static>,
    binary_version: &str,
) -> Result<(), ExtensionError> {
    let _guard = MaterializeLockGuard::acquire(data_dir).await?;
    materialize_embedded_builtin_skills_unlocked(data_dir, corpus, binary_version).await
}

async fn materialize_embedded_builtin_skills_unlocked(
    data_dir: &Path,
    corpus: &Dir<'static>,
    binary_version: &str,
) -> Result<(), ExtensionError> {
    let target = data_dir.join(crate::constants::BUILTIN_SKILLS_DIR_NAME);
    let staging = data_dir.join(STAGING_DIR_NAME);
    let old = data_dir.join(OLD_DIR_NAME);

    // Ensure data_dir itself exists before we try to write into it.
    tokio::fs::create_dir_all(data_dir).await?;

    // Clean any leftover staging from a previous crashed run.
    if staging.exists() {
        retry_startup_file_op("remove builtin skills staging dir", &staging, || {
            tokio::fs::remove_dir_all(&staging)
        })
        .await
        .map_err(|e| {
            ExtensionError::Io(std::io::Error::new(
                e.kind(),
                format!("failed to clean staging dir {}: {e}", staging.display()),
            ))
        })?;
    }
    tokio::fs::create_dir_all(&staging).await?;

    write_dir_recursive(corpus, &staging).await?;

    let version_path = staging.join(VERSION_FILE);
    tokio::fs::write(&version_path, binary_version).await?;

    // Move existing target out of the way, then move staging in.
    if target.exists() {
        if old.exists() {
            // Tolerate leftover .old from a crashed rename sequence.
            if let Err(e) = retry_startup_file_op("remove old builtin skills dir", &old, || {
                tokio::fs::remove_dir_all(&old)
            })
            .await
            {
                warn!(
                    old = %old.display(),
                    error = %e,
                    "failed to remove stale old builtin skills tree before refresh"
                );
            }
        }
        retry_startup_file_op("rename builtin skills target to old", &target, || {
            tokio::fs::rename(&target, &old)
        })
        .await?;
    }

    if let Err(e) = retry_startup_file_op("rename builtin skills staging to target", &staging, || {
        tokio::fs::rename(&staging, &target)
    })
    .await
    {
        // Try to restore the original target so we don't leave the user
        // with no builtin skills.
        if old.exists()
            && let Err(restore_error) = retry_startup_file_op("restore old builtin skills target", &old, || {
                tokio::fs::rename(&old, &target)
            })
            .await
        {
            warn!(
                old = %old.display(),
                target = %target.display(),
                error = %restore_error,
                "failed to restore old builtin skills tree after refresh failure"
            );
        }
        return Err(ExtensionError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "atomic rename staging→target failed ({} → {}): {e}",
                staging.display(),
                target.display()
            ),
        )));
    }

    // Best-effort cleanup of the superseded tree.
    if old.exists()
        && let Err(e) = retry_startup_file_op("remove superseded builtin skills dir", &old, || {
            tokio::fs::remove_dir_all(&old)
        })
        .await
    {
        warn!(
            old = %old.display(),
            error = %e,
            "failed to remove superseded builtin skills tree (leaving behind)"
        );
    }

    Ok(())
}

async fn existing_builtin_skills_looks_usable(target: &Path) -> bool {
    if !target.is_dir() {
        return false;
    }
    tokio::fs::metadata(target.join(VERSION_FILE))
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

async fn retry_startup_file_op<T, F, Fut>(operation: &str, path: &Path, mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    for (attempt, delay) in STARTUP_FILE_RETRY_DELAYS.iter().enumerate() {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable_startup_file_error(&e) => {
                warn!(
                    operation,
                    path = %path.display(),
                    attempt = attempt + 1,
                    retry_after_ms = delay.as_millis(),
                    raw_os_error = ?e.raw_os_error(),
                    error = %e,
                    "Startup file operation failed; retrying"
                );
                tokio::time::sleep(*delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    op().await
}

fn is_retryable_startup_file_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock => true,
        _ => matches!(error.raw_os_error(), Some(5 | 32 | 33)),
    }
}

struct MaterializeLockGuard {
    file: std::fs::File,
}

impl MaterializeLockGuard {
    async fn acquire(data_dir: &Path) -> Result<Self, ExtensionError> {
        Self::acquire_within(data_dir, MATERIALIZE_LOCK_BUDGET).await
    }

    /// Bounded acquisition: poll `try_lock_exclusive` until `budget` elapses,
    /// then fail with a classifiable timeout instead of blocking forever.
    ///
    /// A blocking `lock_exclusive` here made a contended startup look like a
    /// hang: the process printed one line and went silent until the parent
    /// killed it, leaving nothing to diagnose (AIONUI-168).
    ///
    /// `budget` is a parameter so tests can drive the timeout path in
    /// milliseconds rather than waiting out the production budget.
    async fn acquire_within(data_dir: &Path, budget: Duration) -> Result<Self, ExtensionError> {
        let data_dir = data_dir.to_path_buf();
        let lock_path = data_dir.join(LOCK_FILE_NAME);
        let open_path = lock_path.clone();
        // Directory creation and open() are blocking syscalls; keep them off
        // the reactor. The lock polling below is non-blocking by construction.
        let file = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir)?;
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&open_path)
        })
        .await
        .map_err(|e| std::io::Error::other(format!("builtin skills lock task failed: {e}")))??;

        let started = Instant::now();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    if attempts > 1 {
                        info!(
                            lock_path = %lock_path.display(),
                            waited_ms = started.elapsed().as_millis() as u64,
                            attempts,
                            "acquired builtin skills materialize lock after contention"
                        );
                    }
                    return Ok(Self { file });
                }
                // Held by a peer — keep polling until the budget runs out.
                Err(e) if is_lock_contended(&e) => {}
                // Anything else (EPERM, ENOLCK, unsupported filesystem, …) is
                // not going to resolve by waiting.
                Err(e) => return Err(ExtensionError::Io(e)),
            }

            let waited = started.elapsed();
            if waited >= budget {
                error!(
                    lock_path = %lock_path.display(),
                    waited_ms = waited.as_millis() as u64,
                    budget_ms = budget.as_millis() as u64,
                    attempts,
                    "builtin skills materialize lock is still held by another process; giving up"
                );
                return Err(ExtensionError::BuiltinSkillsLockTimeout {
                    lock_path: lock_path.display().to_string(),
                    waited_ms: waited.as_millis() as u64,
                });
            }

            debug!(
                lock_path = %lock_path.display(),
                waited_ms = waited.as_millis() as u64,
                budget_ms = budget.as_millis() as u64,
                attempts,
                "builtin skills materialize lock is busy; retrying"
            );
            tokio::time::sleep(MATERIALIZE_LOCK_POLL_INTERVAL.min(budget - waited)).await;
        }
    }
}

/// True when `error` is the platform's "already locked by someone else"
/// signal as produced by `fs2::try_lock_exclusive` (`EWOULDBLOCK` on Unix,
/// `ERROR_LOCK_VIOLATION` on Windows).
fn is_lock_contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

impl Drop for MaterializeLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Recursively copy every file in an `include_dir::Dir` tree into `dest`.
/// Directories are created as needed. Files overwrite silently.
async fn write_dir_recursive(dir: &Dir<'static>, dest: &Path) -> Result<(), ExtensionError> {
    // The include_dir API is synchronous; we flatten into a Vec then
    // feed the writes through tokio::fs to stay off the reactor's thread
    // for big IO bursts.
    let mut stack: Vec<(&Dir<'static>, PathBuf)> = vec![(dir, dest.to_path_buf())];
    while let Some((d, prefix)) = stack.pop() {
        for file in d.files() {
            let rel = file.path();
            let out_path = prefix.join(rel.strip_prefix(d.path()).unwrap_or(rel));
            if let Some(parent) = out_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&out_path, file.contents()).await?;
        }
        for sub in d.dirs() {
            let sub_rel = sub.path();
            let sub_dest = prefix.join(sub_rel.strip_prefix(d.path()).unwrap_or(sub_rel));
            tokio::fs::create_dir_all(&sub_dest).await?;
            stack.push((sub, sub_dest));
        }
    }
    Ok(())
}

#[cfg(test)]
mod materialize_lock_tests {
    use super::*;

    /// Uncontended acquisition must not pay any of the retry budget.
    #[tokio::test]
    async fn acquire_within_succeeds_immediately_when_uncontended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = Instant::now();

        let guard = MaterializeLockGuard::acquire_within(dir.path(), MATERIALIZE_LOCK_BUDGET)
            .await
            .expect("uncontended acquire must succeed");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "uncontended acquire took {:?}",
            started.elapsed()
        );
        assert!(dir.path().join(LOCK_FILE_NAME).exists(), "lock file must be created");
        drop(guard);
    }

    /// Regression for AIONUI-168: with the lock held by a peer, acquisition
    /// must give up inside its budget with a classifiable timeout instead of
    /// blocking until the parent process kills us.
    ///
    /// Unix-only: this relies on `flock` locks being owned by the open file
    /// description, so two `open()`s in the same process genuinely contend
    /// (verified by fs2's own tests, fs2-0.4.3/src/unix.rs `lock_replace`).
    /// The equivalent Windows guarantee is not verified here, so the test is
    /// gated rather than left to pass vacuously.
    #[cfg(unix)]
    #[tokio::test]
    async fn acquire_within_times_out_while_a_peer_holds_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let budget = Duration::from_millis(300);

        // Peer: a second open file description holding the exclusive flock.
        let peer = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.path().join(LOCK_FILE_NAME))
            .expect("open peer lock file");
        FileExt::lock_exclusive(&peer).expect("peer must take the lock");

        let started = Instant::now();
        let result = MaterializeLockGuard::acquire_within(dir.path(), budget).await;
        let elapsed = started.elapsed();

        match result {
            Err(ExtensionError::BuiltinSkillsLockTimeout { lock_path, waited_ms }) => {
                assert_eq!(lock_path, dir.path().join(LOCK_FILE_NAME).display().to_string());
                assert!(
                    waited_ms >= budget.as_millis() as u64,
                    "reported wait {waited_ms}ms is shorter than the {budget:?} budget"
                );
            }
            Err(other) => panic!("expected BuiltinSkillsLockTimeout, got {other:?}"),
            Ok(_) => panic!("acquire must not succeed while a peer holds the lock"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "acquire blocked for {elapsed:?} instead of honouring its {budget:?} budget"
        );

        FileExt::unlock(&peer).expect("release peer lock");
    }

    /// The budget is a ceiling, not a fixed wait: once the peer releases, the
    /// poll loop must pick the lock up rather than run out the clock.
    #[cfg(unix)]
    #[tokio::test]
    async fn acquire_within_succeeds_once_the_peer_releases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peer = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.path().join(LOCK_FILE_NAME))
            .expect("open peer lock file");
        FileExt::lock_exclusive(&peer).expect("peer must take the lock");

        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            FileExt::unlock(&peer).expect("release peer lock");
        });

        let started = Instant::now();
        let guard = MaterializeLockGuard::acquire_within(dir.path(), Duration::from_secs(5))
            .await
            .expect("acquire must succeed after the peer releases");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(200),
            "acquire returned in {elapsed:?}, before the peer could have released"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "acquire took {elapsed:?}, i.e. it ran out the whole budget"
        );
        drop(guard);
        releaser.await.expect("releaser task");
    }
}
