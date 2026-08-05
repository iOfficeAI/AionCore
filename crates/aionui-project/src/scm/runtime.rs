//! `ScmRuntime` — orchestration between the provider, the watch, and the wire.
//!
//! Holds the little state source control needs and nothing more: per repository
//! the last computed status, the monotonic sequence number, the set of
//! subscribed connections, and the watch registration. There is no incremental
//! tree and no reconciliation machinery — status is a derived quantity that is
//! cheap to recompute and has no stable per-item identity, so the model is
//! "something is dirty → recompute in full → replace the frame"
//! (`formal/runtime/source-control.md`).
//!
//! Two responsibilities are load-bearing and easy to get wrong:
//!
//! **Sequence allocation.** Two refresh sources run concurrently — an action
//! finishing, and a debounced watch signal — so their results can arrive out of
//! order. Every recompute therefore happens inside one per-repository critical
//! section that also allocates the sequence number, which is what makes the
//! numbers monotonic and lets a client drop a frame that is older than what it
//! already applied. Allocating outside that section would hand out numbers whose
//! order does not match the order the statuses were computed in.
//!
//! **Identity.** The provider deals in repository-relative paths; the pe identity
//! belongs to the resolved root. Assembling `{ pe_id, relative_path }` is this
//! layer's job, so a provider can never invent identity.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use super::error::ScmError;
use super::git_provider::GitScmProvider;
use super::provider::IScmProvider;
use super::types::{FileRef, RepoRef, ResolvedRoot, ScmRepository, ScmStatus};
use super::watch::GitWatcher;

/// Per-repository state. Thin by design (see module docs).
struct RepoState {
    /// Descriptor handed to clients.
    repository: ScmRepository,
    /// Real git directory, for arming and releasing the watch.
    git_dir: PathBuf,
    /// Last computed status, if it has been computed at least once.
    last: Option<ScmStatus>,
    /// Highest sequence number handed out for this repository.
    seq: u64,
    /// Connections currently subscribed. The watch lives exactly as long as this
    /// is non-empty, so a repository nobody is looking at costs nothing.
    subscribers: Vec<String>,
}

/// Guards one repository's recompute-and-publish critical section.
///
/// A separate lock per repository: work on unrelated repositories proceeds
/// concurrently, while everything touching one repository — actions and refreshes
/// alike — is serialized so sequence numbers and statuses stay in agreement.
type RepoLock = Arc<Mutex<()>>;

/// Orchestration layer for source control.
pub struct ScmRuntime {
    provider: Arc<GitScmProvider>,
    watcher: Arc<GitWatcher>,
    repos: RwLock<HashMap<String, RepoState>>,
    locks: RwLock<HashMap<String, RepoLock>>,
}

impl ScmRuntime {
    /// Build the runtime, returning it together with the dirty-signal receiver
    /// the caller must drive (debounce then [`ScmRuntime::refresh`]).
    pub(super) fn new() -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<super::watch::ScmDirty>), ScmError> {
        let (watcher, dirty_rx) = GitWatcher::new()?;
        Ok((
            Self {
                provider: Arc::new(GitScmProvider::new()),
                watcher: Arc::new(watcher),
                repos: RwLock::new(HashMap::new()),
                locks: RwLock::new(HashMap::new()),
            },
            dirty_rx,
        ))
    }

    /// Discover which of a project's roots are repositories.
    ///
    /// Roots that are not repositories are simply absent from the result — never
    /// represented as an empty repository.
    pub(super) async fn discover(&self, roots: &[ResolvedRoot]) -> Vec<ScmRepository> {
        let mut found = Vec::new();
        for root in roots {
            match self.provider.discover(root).await {
                Ok(Some(repository)) => {
                    let git_dir = self.provider.git_dir_of(&RepoRef {
                        repo_id: repository.repo_id.clone(),
                    });
                    let mut repos = self.repos.write().await;
                    let entry = repos.entry(repository.repo_id.clone());
                    match entry {
                        std::collections::hash_map::Entry::Occupied(mut slot) => {
                            // Re-discovery of a known repository refreshes its
                            // descriptor (head may have moved) but must not drop
                            // subscribers or the sequence it has already handed out.
                            slot.get_mut().repository = repository.clone();
                        }
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert(RepoState {
                                repository: repository.clone(),
                                git_dir: git_dir.unwrap_or_default(),
                                last: None,
                                seq: 0,
                                subscribers: Vec::new(),
                            });
                        }
                    }
                    found.push(repository);
                }
                Ok(None) => {}
                Err(err) => {
                    // One unreadable root must not hide the project's other
                    // repositories.
                    tracing::warn!(pe_id = %root.pe_id, error = %err, "scm discover failed for root");
                }
            }
        }
        found
    }

    /// Subscribe `session` to a repository and return its current status.
    ///
    /// The watch is armed **before** the first status is computed, so a change
    /// landing during that computation still produces a signal instead of being
    /// lost in the gap.
    pub(super) async fn subscribe(&self, session: &str, repo: &RepoRef) -> Result<ScmStatus, ScmError> {
        let git_dir = {
            let mut repos = self.repos.write().await;
            let state = repos
                .get_mut(&repo.repo_id)
                .ok_or_else(|| ScmError::UnknownRepository {
                    repo_id: repo.repo_id.clone(),
                })?;
            let first = state.subscribers.is_empty();
            if !state.subscribers.iter().any(|s| s == session) {
                state.subscribers.push(session.to_owned());
            }
            first.then(|| state.git_dir.clone())
        };

        if let Some(git_dir) = git_dir
            && let Err(err) = self.watcher.watch(&repo.repo_id, &git_dir)
        {
            // Losing live refresh degrades to manual refresh; it must not fail
            // the subscription, which still returns a correct first frame.
            tracing::warn!(repo_id = %repo.repo_id, error = %err, "scm watch arm failed; live refresh unavailable");
        }

        self.refresh(repo).await
    }

    /// Unsubscribe `session`. The watch is released once nobody is subscribed —
    /// reference counted, since several connections may observe one repository.
    pub(super) async fn unsubscribe(&self, session: &str, repo: &RepoRef) {
        let release = {
            let mut repos = self.repos.write().await;
            match repos.get_mut(&repo.repo_id) {
                Some(state) => {
                    state.subscribers.retain(|s| s != session);
                    let empty = state.subscribers.is_empty();
                    if empty {
                        // Drop the cached frame with the watch: without a watch it
                        // would go stale unnoticed, and recomputing is cheap.
                        state.last = None;
                    }
                    empty
                }
                None => false,
            }
        };
        if release {
            self.watcher.unwatch(&repo.repo_id);
        }
    }

    /// Release everything a closed connection held.
    ///
    /// Without this a reconnect churn would leak one watch per dropped
    /// connection, since nothing else ever tells us that session is gone.
    pub(super) async fn drop_session(&self, session: &str) {
        let orphaned: Vec<String> = {
            let mut repos = self.repos.write().await;
            let mut orphaned = Vec::new();
            for (repo_id, state) in repos.iter_mut() {
                let before = state.subscribers.len();
                state.subscribers.retain(|s| s != session);
                if before != state.subscribers.len() && state.subscribers.is_empty() {
                    state.last = None;
                    orphaned.push(repo_id.clone());
                }
            }
            orphaned
        };
        for repo_id in orphaned {
            self.watcher.unwatch(&repo_id);
        }
    }

    /// Recompute a repository's status and publish it as the current frame.
    ///
    /// The recompute and the sequence allocation share one critical section (see
    /// module docs): that is what keeps sequence order equal to computation
    /// order when an action-triggered refresh races a watch-triggered one.
    pub(super) async fn refresh(&self, repo: &RepoRef) -> Result<ScmStatus, ScmError> {
        let lock = self.lock_for(&repo.repo_id).await;
        let _guard = lock.lock().await;

        let pe_id = self.pe_id_of(repo).await?;
        let mut status = self.provider.status(repo).await?;

        // Identity is assembled here, not in the provider: the provider knows
        // repository-relative paths, the pe identity comes from the resolved root.
        for resource in &mut status.resources {
            resource.file = FileRef {
                pe_id: pe_id.clone(),
                relative_path: resource.repo_relative_path.clone(),
            };
        }

        // Monotonicity rests on **two** guards, and both are deliberate:
        //   1. the per-repository critical section entered above, which serializes
        //      whole recomputes (and actions) against each other, and
        //   2. this single write guard, which makes read-increment-store atomic.
        // Removing either one alone still looks correct and keeps the tests green,
        // because the other masks it — but removing both lets concurrent refreshes
        // hand out duplicate sequences, and a client then discards a newer frame as
        // "older". Do not "simplify" one away.
        let mut repos = self.repos.write().await;
        let state = repos
            .get_mut(&repo.repo_id)
            .ok_or_else(|| ScmError::UnknownRepository {
                repo_id: repo.repo_id.clone(),
            })?;
        state.seq += 1;
        status.seq = state.seq;
        state.last = Some(status.clone());
        Ok(status)
    }

    /// Connections that should receive a repository's frame.
    pub(super) async fn subscribers_of(&self, repo_id: &str) -> Vec<String> {
        self.repos
            .read()
            .await
            .get(repo_id)
            .map(|state| state.subscribers.clone())
            .unwrap_or_default()
    }

    /// The provider, for read-only calls that need no orchestration (diff,
    /// original) and for actions, which the caller wraps in [`ScmRuntime::act`].
    pub(super) fn provider(&self) -> &GitScmProvider {
        &self.provider
    }

    /// Run a mutating action inside the repository's critical section, then
    /// recompute so the published frame reflects it.
    ///
    /// Actions and refreshes share the lock deliberately: a status computed
    /// halfway through a staging operation would describe a state that never
    /// existed.
    pub(super) async fn act<T, F, Fut>(&self, repo: &RepoRef, action: F) -> Result<(T, ScmStatus), ScmError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, ScmError>>,
    {
        let lock = self.lock_for(&repo.repo_id).await;
        let guard = lock.lock().await;
        let produced = action().await?;
        drop(guard);
        let status = self.refresh(repo).await?;
        Ok((produced, status))
    }

    /// Whether a repository's metadata watch is armed. For tests that must verify
    /// release, not merely that the subscriber list emptied.
    #[cfg(test)]
    pub(super) fn is_watching(&self, repo_id: &str) -> bool {
        self.watcher.is_watching(repo_id)
    }

    async fn lock_for(&self, repo_id: &str) -> RepoLock {
        if let Some(lock) = self.locks.read().await.get(repo_id) {
            return Arc::clone(lock);
        }
        let mut locks = self.locks.write().await;
        Arc::clone(locks.entry(repo_id.to_owned()).or_default())
    }

    /// pe identity of a repository's root, for the authorization guard.
    pub(super) async fn pe_id_of_public(&self, repo: &RepoRef) -> Result<String, ScmError> {
        self.pe_id_of(repo).await
    }

    async fn pe_id_of(&self, repo: &RepoRef) -> Result<String, ScmError> {
        self.repos
            .read()
            .await
            .get(&repo.repo_id)
            .map(|state| state.repository.root.pe_id.clone())
            .ok_or_else(|| ScmError::UnknownRepository {
                repo_id: repo.repo_id.clone(),
            })
    }
}

#[cfg(test)]
#[path = "runtime_test.rs"]
mod runtime_test;
