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

use std::collections::{HashMap, HashSet};
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
    /// The set of repositories each project last resolved to, keyed by project
    /// id. A roots recompute diffs the fresh set against this to decide which
    /// repositories were added or removed — by `repo_id`, never by path, since a
    /// case-insensitive filesystem makes path comparison unsound.
    project_repos: RwLock<HashMap<String, HashSet<String>>>,
    /// Which connections have expressed interest in each project's repositories,
    /// by listing them. A project's `repositoriesChanged` frame fans out to these.
    /// Session-persistent: an entry is cleared only when the connection drops, not
    /// when it unsubscribes from a repository, so a client that navigates away and
    /// back keeps receiving changes without re-registering.
    project_interest: RwLock<HashMap<String, HashSet<String>>>,
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
                project_repos: RwLock::new(HashMap::new()),
                project_interest: RwLock::new(HashMap::new()),
            },
            dirty_rx,
        ))
    }

    /// Discover which of a set of roots are repositories.
    ///
    /// Roots that are not repositories are simply absent from the result — never
    /// represented as an empty repository. Registers each discovered repository in
    /// the runtime and provider, but does not touch any project's tracked set;
    /// that is the two project-aware callers' job.
    async fn discover_roots(&self, roots: &[ResolvedRoot]) -> Vec<ScmRepository> {
        let mut found = Vec::new();
        for root in roots {
            match self.provider.discover(root).await {
                // A root may surface many repositories now (one-level workspace
                // discovery); an attached root still yields at most one.
                Ok(repositories) => {
                    for repository in repositories {
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
                }
                Err(err) => {
                    // One unreadable root must not hide the project's other
                    // repositories.
                    tracing::warn!(pe_id = %root.pe_id, error = %err, "scm discover failed for root");
                }
            }
        }
        found
    }

    /// Discover a project's repositories and record the result as the project's
    /// baseline, so a later roots recompute can diff against it.
    pub(super) async fn discover(&self, project_id: &str, roots: &[ResolvedRoot]) -> Vec<ScmRepository> {
        let found = self.discover_roots(roots).await;
        let ids = found.iter().map(|r| r.repo_id.clone()).collect();
        self.project_repos.write().await.insert(project_id.to_owned(), ids);
        found
    }

    /// Recompute a project's repositories after its attached-folder set changed,
    /// returning what was added (full descriptors) and removed (ids) against the
    /// last known set.
    ///
    /// Removed repositories are released here (watch dropped, state and provider
    /// entry forgotten): nothing else would, and a repository no project can reach
    /// would otherwise keep its metadata watch armed for the process's lifetime.
    ///
    /// Order is load-bearing. Discovery runs *first* and re-registers every
    /// repository still present; only then is `removed` computed as "was in the
    /// old set, is not in the fresh one". A repository removed and re-added within
    /// one recompute is thus in the fresh set and never released — so a quick
    /// remove-then-re-add cannot tear down the watch the re-add just implied.
    pub(super) async fn recompute_project(
        &self,
        project_id: &str,
        roots: &[ResolvedRoot],
    ) -> (Vec<ScmRepository>, Vec<String>) {
        let now = self.discover_roots(roots).await;
        let now_ids: HashSet<String> = now.iter().map(|r| r.repo_id.clone()).collect();

        let previous = self
            .project_repos
            .read()
            .await
            .get(project_id)
            .cloned()
            .unwrap_or_default();
        let added: Vec<ScmRepository> = now.iter().filter(|r| !previous.contains(&r.repo_id)).cloned().collect();
        let removed: Vec<String> = previous.difference(&now_ids).cloned().collect();

        self.project_repos.write().await.insert(project_id.to_owned(), now_ids);
        for repo_id in &removed {
            self.release_repo(repo_id).await;
        }
        (added, removed)
    }

    /// Release a repository that has left every project: drop its watch, its cached
    /// state (and with it its subscriber list), and the provider's handle to it.
    async fn release_repo(&self, repo_id: &str) {
        // Unwatch first, so no further dirty signal can arrive for a repository we
        // are in the middle of forgetting.
        self.watcher.unwatch(repo_id);
        self.repos.write().await.remove(repo_id);
        self.provider.forget(repo_id);
    }

    /// Record that `session` is interested in a project's repositories, so it
    /// receives that project's `repositoriesChanged` frames until it disconnects.
    pub(super) async fn register_interest(&self, session: &str, project_id: &str) {
        self.project_interest
            .write()
            .await
            .entry(project_id.to_owned())
            .or_default()
            .insert(session.to_owned());
    }

    /// Connections that should receive a project's `repositoriesChanged` frame.
    pub(super) async fn project_subscribers_of(&self, project_id: &str) -> Vec<String> {
        self.project_interest
            .read()
            .await
            .get(project_id)
            .map(|sessions| sessions.iter().cloned().collect())
            .unwrap_or_default()
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

    /// Release everything a closed connection held: its repository subscriptions
    /// (and the watches they alone kept armed) and its project interest.
    ///
    /// Without the subscription cleanup a reconnect churn would leak one watch per
    /// dropped connection. Without the interest cleanup a dropped connection would
    /// linger in every project it listed — leaking unboundedly and, worse,
    /// receiving `repositoriesChanged` frames pushed to a session that is gone.
    /// This is the sole release point for `project_interest` (it is
    /// session-persistent, never dropped on repo unsubscribe).
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

        let mut interest = self.project_interest.write().await;
        for sessions in interest.values_mut() {
            sessions.remove(session);
        }
        interest.retain(|_, sessions| !sessions.is_empty());
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
