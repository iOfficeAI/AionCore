//! `GitScmProvider` — the git implementation of [`IScmProvider`], in-process
//! via git2 (no external binary, so headless and cross-platform).
//!
//! Written against the git2 public API only; it shares no code with the
//! checkpoint snapshot service in `aionui-file`, which models a single
//! workspace and a temp-repo mode neither of which fits source control.
//!
//! git2 handles are synchronous and not `Send`, so every repository access runs
//! inside `spawn_blocking` and the handle never crosses an await point (the same
//! shape the filename-search walk uses).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use git2::{Delta, DiffOptions, ErrorClass, ErrorCode, Repository, Status, StatusOptions, StatusShow};

use super::error::ScmError;
use super::provider::{IScmProvider, IScmStaging};
use super::trash_sink::{PlatformTrash, TrashSink};
use super::types::{
    ContentRef, DiffContent, FileRef, RepoRef, ResolvedRoot, ScmActionFailure, ScmActionOutcome, ScmCapabilities,
    ScmHead, ScmRepository, ScmRepositoryState, ScmResource, ScmResourceState, ScmStatus,
};

/// Cap on resources in one status result.
///
/// Applies to untracked entries too, deliberately: scan cost tracks the number
/// of non-ignored files (tracked ∪ untracked), and an agent creating files in
/// bulk goes down exactly that path, so counting only tracked changes would
/// leave the real growth uncapped.
pub(super) const STATUS_RESOURCE_LIMIT: usize = 10_000;

/// Largest blob inlined into a diff before it is reported as truncated.
const DIFF_CONTENT_LIMIT: usize = 1 << 20;

/// Largest number of add/delete candidates for which rename detection is run.
///
/// Rename detection compares content similarity across every delete × add pair,
/// so its cost grows super-linearly in the number of candidates: measured on
/// this workspace at ~12ms for 100 renames, 109ms for 500, 366ms for 1000 and
/// ~1.0s for 3000, against ~12ms with detection off. Bulk moves are exactly the
/// case that would stall the panel, so past this many candidates the pass is
/// skipped and those files are reported as a delete plus a create — accurate,
/// just less informative than a rename.
const RENAME_DETECTION_CANDIDATE_LIMIT: usize = 400;

/// Registry entry: what the provider remembers per discovered repository.
///
/// Only the on-disk locations. Repository handles are reopened per operation
/// rather than cached, so nothing non-`Send` is held across awaits and a repo
/// replaced on disk cannot leave a stale handle behind.
#[derive(Debug, Clone)]
struct RepoEntry {
    /// Work-tree root (the pe root itself: one repo per pe at most).
    workdir: PathBuf,
    /// The **real** git directory. For a linked worktree or a submodule the
    /// `.git` entry beside the work tree is a file pointing elsewhere, so this is
    /// what a metadata watch must be armed on.
    git_dir: PathBuf,
}

/// git source-control provider.
pub struct GitScmProvider {
    /// Discovered repositories by opaque `repo_id`.
    repos: std::sync::RwLock<HashMap<String, RepoEntry>>,
    /// Where discarded untracked files go. Injectable so the "never delete
    /// outright" guarantee is testable; production always uses the platform trash.
    trash: Arc<dyn TrashSink>,
}

impl GitScmProvider {
    pub fn new() -> Self {
        Self::with_trash(Arc::new(PlatformTrash))
    }

    /// Build with a specific trash sink. Crate-internal: the outward contract
    /// takes no sink argument, so this exists for tests (and for a future
    /// platform override) without widening the public API.
    pub(super) fn with_trash(trash: Arc<dyn TrashSink>) -> Self {
        Self {
            repos: std::sync::RwLock::new(HashMap::new()),
            trash,
        }
    }

    /// Opaque `repo_id` for a pe root. Generation rule only — consumers must
    /// not parse it back into a `pe_id`.
    fn repo_id_for(pe_id: &str) -> String {
        format!("scm:{pe_id}")
    }

    /// The real git directory of a discovered repository, for arming a metadata
    /// watch. `None` when the repository is not known.
    pub(super) fn git_dir_of(&self, repo: &RepoRef) -> Option<PathBuf> {
        self.repos
            .read()
            .expect("scm repo registry poisoned")
            .get(&repo.repo_id)
            .map(|entry| entry.git_dir.clone())
    }

    /// Drop a repository's cached workdir / git-dir handle. Called when the runtime
    /// releases a repository that has left every project, so a stale entry cannot
    /// outlive the repository it describes.
    pub(super) fn forget(&self, repo_id: &str) {
        self.repos.write().expect("scm repo registry poisoned").remove(repo_id);
    }

    fn entry(&self, repo: &RepoRef) -> Result<RepoEntry, ScmError> {
        self.repos
            .read()
            .expect("scm repo registry poisoned")
            .get(&repo.repo_id)
            .cloned()
            .ok_or_else(|| ScmError::UnknownRepository {
                repo_id: repo.repo_id.clone(),
            })
    }
}

impl Default for GitScmProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a git2 failure into the neutral taxonomy, tagged with the operation.
fn engine_error(context: &'static str, err: &git2::Error) -> ScmError {
    ScmError::OperationFailed {
        context,
        message: format!("{} (class={:?}, code={:?})", err.message(), err.class(), err.code()),
    }
}

/// Whether a failed `statuses()` call is the known stale-index writeback
/// failure, i.e. worth retrying without index writeback.
///
/// Two real causes: a read-only `.git` whose index is stale, and an
/// `index.lock` held by a concurrent git command. Both surface as a failure of
/// the whole call rather than a silent downgrade, so they must be caught
/// explicitly instead of being reported to the user as "source control broke".
fn is_index_writeback_failure(err: &git2::Error) -> bool {
    matches!(err.code(), ErrorCode::Locked)
        || matches!(err.class(), ErrorClass::Index | ErrorClass::Os | ErrorClass::Filesystem)
}

/// Classify one git status flag set into the neutral state plus its staged side.
///
/// Order matters and encodes the data-safety rule: conflicted and renamed are
/// recognized before the regular create/modify/delete folding, because folding
/// them into `modified` invites a user to stage or discard a conflict
/// resolution, or hides that a path changed.
fn classify(status: Status) -> Vec<(ScmResourceState, Option<bool>)> {
    if status.contains(Status::CONFLICTED) {
        return vec![(ScmResourceState::Conflicted, None)];
    }

    let mut out = Vec::new();

    // Index side (staged).
    if status.contains(Status::INDEX_RENAMED) {
        out.push((ScmResourceState::Renamed, Some(true)));
    } else if status.contains(Status::INDEX_NEW) {
        out.push((ScmResourceState::Created, Some(true)));
    } else if status.contains(Status::INDEX_DELETED) {
        out.push((ScmResourceState::Deleted, Some(true)));
    } else if status.intersects(Status::INDEX_MODIFIED | Status::INDEX_TYPECHANGE) {
        out.push((ScmResourceState::Modified, Some(true)));
    }

    // Work-tree side (unstaged). A file can appear on both sides at once — it
    // then yields two resources, which is how "staged and further edited" is
    // represented without a nested shape.
    if status.contains(Status::WT_RENAMED) {
        out.push((ScmResourceState::Renamed, Some(false)));
    } else if status.contains(Status::WT_NEW) {
        out.push((ScmResourceState::Created, Some(false)));
    } else if status.contains(Status::WT_DELETED) {
        out.push((ScmResourceState::Deleted, Some(false)));
    } else if status.intersects(Status::WT_MODIFIED | Status::WT_TYPECHANGE) {
        out.push((ScmResourceState::Modified, Some(false)));
    }

    out
}

/// Read head as the neutral model sees it: branch name, or detached.
fn read_head(repo: &Repository) -> Option<ScmHead> {
    match repo.head() {
        Ok(head) => {
            let detached = repo.head_detached().unwrap_or(false);
            Some(ScmHead {
                name: head.shorthand().map(str::to_owned),
                detached: detached.then_some(true),
            })
        }
        // Unborn head (fresh repo, no commit yet) is a normal state, not an
        // error: the repository exists and has a change list.
        Err(_) => Some(ScmHead::default()),
    }
}

/// Run one `statuses()` pass, with the stale-index fallback.
///
/// Index writeback is on by default because without it a single external
/// mtime-churning pass (touch, build output, unpack, rsync) makes every later
/// recompute read all file contents, and that does not self-heal. When
/// writeback itself is impossible, the call is retried read-only: the resource
/// list is identical, only the timing degrades, and the degradation is logged
/// and reported rather than hidden.
fn collect_status(repo: &Repository) -> Result<(Vec<ScmResource>, bool, bool, Option<ScmHead>), ScmError> {
    // Read head in the same blocking pass that computes the change list: a
    // checkout moves head, and the status frame is the only frame the refresh
    // path emits, so head must ride along with it or a branch switch never
    // reaches the client (see `ScmStatus::head`).
    let head = read_head(repo);
    // First pass without rename detection: it is the cheap one, and its
    // add/delete count is exactly the input size rename detection would work on.
    let (probe, degraded) = match run_statuses(repo, true, false) {
        Ok(s) => (s, false),
        Err(err) if is_index_writeback_failure(&err) => {
            tracing::warn!(
                error = %err.message(),
                class = ?err.class(),
                code = ?err.code(),
                "scm status: index writeback failed, retrying without it (degraded: recompute stays slow \
                 until the index can be refreshed — read-only .git or a concurrent git holding index.lock)"
            );
            (
                run_statuses(repo, false, false).map_err(|e| engine_error("status", &e))?,
                true,
            )
        }
        Err(err) => return Err(engine_error("status", &err)),
    };

    // Rename detection only has work to do when something was added and
    // something removed; running it on a bulk move is what costs seconds.
    let candidates = probe
        .iter()
        .filter(|e| {
            e.status()
                .intersects(Status::INDEX_NEW | Status::INDEX_DELETED | Status::WT_NEW | Status::WT_DELETED)
        })
        .count();
    let detect_renames = candidates > 0 && candidates <= RENAME_DETECTION_CANDIDATE_LIMIT;
    if candidates > RENAME_DETECTION_CANDIDATE_LIMIT {
        tracing::info!(
            candidates,
            limit = RENAME_DETECTION_CANDIDATE_LIMIT,
            "scm status: skipping rename detection, too many add/delete candidates \
             (renames surface as delete + create)"
        );
    }

    let statuses_owned = if detect_renames {
        // Writeback already happened (or was proven impossible) in the probe, so
        // this pass never needs it again.
        run_statuses(repo, false, true).map_err(|e| engine_error("status", &e))?
    } else {
        probe
    };

    let mut resources = Vec::new();
    let mut truncated = false;

    for entry in statuses_owned.iter() {
        let Some(path) = entry.path() else {
            // Non-UTF-8 path: skipped rather than lossily rendered, since a
            // mangled path would not round-trip back to a real file.
            tracing::warn!("scm status: skipping entry with non-UTF-8 path");
            continue;
        };
        // The wire contract says `relative_path` is `/`-separated, and libgit2
        // stores repo paths that way on every platform (it converts `\` to `/` on
        // input, see git2's `path_to_repo_path`). So this is reported, not
        // rewritten: a blind `\` → `/` substitution would corrupt the identity of
        // files whose *name* legitimately contains a backslash, which is valid on
        // Linux and macOS. If this ever fires, the fix belongs at whichever layer
        // actually produced the separator — not silently here.
        if path.contains('\\') {
            tracing::warn!(
                repo_relative_path = %path,
                "scm status: path contains a backslash where the contract expects `/`-separated \
                 segments; reporting as-is (see cross-platform checklist)"
            );
        }
        // For a rename, `entry.path()` is the OLD path; the current path lives in
        // the delta's new_file. Reporting the old path as the resource identity
        // would point the UI at a file that no longer exists, so the two are
        // taken from the delta explicitly.
        //
        // Each side is filtered **before** choosing between them: a file can carry a
        // non-rename change on the index side and a rename on the work-tree side
        // (stage an edit, then move the file). Selecting first and filtering after
        // would take the index delta merely because it exists, then discard it for
        // not being a rename — losing the rename sitting on the other side, and with
        // it the file's real current path.
        let rename_of = |delta: git2::DiffDelta<'_>| {
            let from = delta.old_file().path()?.to_string_lossy().into_owned();
            let to = delta.new_file().path()?.to_string_lossy().into_owned();
            Some((from, to))
        };
        let rename_pair = entry
            .head_to_index()
            .filter(|d| d.status() == Delta::Renamed)
            .and_then(rename_of)
            .or_else(|| {
                entry
                    .index_to_workdir()
                    .filter(|d| d.status() == Delta::Renamed)
                    .and_then(rename_of)
            });
        let current_path = rename_pair.as_ref().map_or(path, |(_, to)| to.as_str());
        let rename_from = rename_pair.as_ref().map(|(from, _)| from.clone());

        for (state, staged) in classify(entry.status()) {
            if resources.len() >= STATUS_RESOURCE_LIMIT {
                truncated = true;
                break;
            }
            resources.push(ScmResource {
                file: FileRef {
                    pe_id: String::new(), // filled by the caller, which owns identity
                    relative_path: current_path.to_owned(),
                },
                repo_relative_path: current_path.to_owned(),
                state,
                staged,
                rename_from: matches!(state, ScmResourceState::Renamed)
                    .then(|| rename_from.clone())
                    .flatten(),
            });
        }
        if truncated {
            break;
        }
    }

    Ok((resources, truncated, degraded, head))
}

fn run_statuses(
    repo: &Repository,
    update_index: bool,
    detect_renames: bool,
) -> Result<git2::Statuses<'_>, git2::Error> {
    let mut opts = StatusOptions::new();
    opts.show(StatusShow::IndexAndWorkdir)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .include_unmodified(false)
        .renames_head_to_index(detect_renames)
        .renames_index_to_workdir(detect_renames)
        .update_index(update_index);
    repo.statuses(Some(&mut opts))
}

/// Read a blob at one anchor. `None` = the file has no version there.
fn read_at(repo: &Repository, rel: &str, at: ContentRef) -> Result<Option<Vec<u8>>, ScmError> {
    let path = Path::new(rel);
    match at {
        ContentRef::Working => {
            let Some(workdir) = repo.workdir() else {
                return Err(ScmError::OperationFailed {
                    context: "original",
                    message: "bare repository has no work tree".to_owned(),
                });
            };
            match std::fs::read(workdir.join(path)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(ScmError::Io {
                    path: rel.to_owned(),
                    message: err.to_string(),
                }),
            }
        }
        ContentRef::Staged => {
            let index = repo.index().map_err(|e| engine_error("original", &e))?;
            if let Some(entry) = index.get_path(path, 0) {
                let blob = repo.find_blob(entry.id).map_err(|e| engine_error("original", &e))?;
                return Ok(Some(blob.content().to_vec()));
            }
            // No resolved entry. Two very different situations share that shape, and
            // conflating them produces wrong output rather than an error:
            //
            //  * the file simply is not staged — "no content here" is the truthful
            //    answer, and callers render it as an addition;
            //  * the file is **conflicted**, so the index holds the three sides of
            //    the conflict instead of one resolved version. There is no "staged
            //    version" to return, and answering "no content" would let a diff
            //    against it read as *the whole file was deleted* while the file sits
            //    intact on disk.
            //
            // The second case is therefore refused, with the same meaning the
            // actions use for conflicted resources: not now, resolve the conflict
            // first.
            let conflicted = [1, 2, 3].iter().any(|stage| index.get_path(path, *stage).is_some());
            if conflicted {
                return Err(ScmError::OpaqueResource { path: rel.to_owned() });
            }
            Ok(None)
        }
        ContentRef::Committed => {
            let Ok(head) = repo.head() else {
                return Ok(None); // unborn head: nothing committed yet
            };
            let tree = head.peel_to_tree().map_err(|e| engine_error("original", &e))?;
            match tree.get_path(path) {
                Ok(entry) => {
                    let blob = repo.find_blob(entry.id()).map_err(|e| engine_error("original", &e))?;
                    Ok(Some(blob.content().to_vec()))
                }
                Err(err) if err.code() == ErrorCode::NotFound => Ok(None),
                Err(err) => Err(engine_error("original", &err)),
            }
        }
    }
}

/// Build a unified patch between two anchors for one file.
fn diff_between(repo: &Repository, rel: &str, from: ContentRef, to: ContentRef) -> Result<DiffContent, ScmError> {
    let old = read_at(repo, rel, from)?;
    let new = read_at(repo, rel, to)?;

    let is_binary = |b: &Vec<u8>| b.contains(&0);
    if old.as_ref().is_some_and(is_binary) || new.as_ref().is_some_and(is_binary) {
        return Ok(DiffContent {
            binary: true,
            ..DiffContent::default()
        });
    }
    if old.as_ref().is_some_and(|b| b.len() > DIFF_CONTENT_LIMIT)
        || new.as_ref().is_some_and(|b| b.len() > DIFF_CONTENT_LIMIT)
    {
        return Ok(DiffContent {
            truncated: true,
            ..DiffContent::default()
        });
    }

    let mut patch = String::new();
    let mut opts = DiffOptions::new();
    git2::Patch::from_buffers(
        old.as_deref().unwrap_or_default(),
        Some(Path::new(rel)),
        new.as_deref().unwrap_or_default(),
        Some(Path::new(rel)),
        Some(&mut opts),
    )
    .and_then(|mut p| {
        p.print(&mut |_, _, line| {
            match line.origin() {
                '+' | '-' | ' ' => patch.push(line.origin()),
                _ => {}
            }
            patch.push_str(&String::from_utf8_lossy(line.content()));
            true
        })
    })
    .map_err(|e| engine_error("diff", &e))?;

    Ok(DiffContent {
        patch: Some(patch),
        ..DiffContent::default()
    })
}

/// Restore tracked files to their committed content and send untracked ones to
/// the system trash.
///
/// Untracked files are never unlinked: a discard that permanently destroys a
/// file the version-control system has no copy of is unrecoverable, so the
/// platform trash is the floor.
fn revert_paths(repo: &Repository, rels: &[String], trash: &dyn TrashSink) -> Result<Vec<(usize, String)>, ScmError> {
    let Some(workdir) = repo.workdir().map(Path::to_path_buf) else {
        return Err(ScmError::OperationFailed {
            context: "revert",
            message: "bare repository has no work tree".to_owned(),
        });
    };

    // One scan for the whole request; consulted per file below.
    let rename_sources = rename_sources(repo);

    // Indices into `rels`, so a failure can be reported against the exact request
    // item it came from.
    let mut tracked: Vec<(usize, &String)> = Vec::new();
    let mut to_trash: Vec<(usize, PathBuf)> = Vec::new();
    // Renames: (request index, path it came from, absolute path it moved to).
    let mut renamed: Vec<(usize, String, PathBuf)> = Vec::new();

    for (index, rel) in rels.iter().enumerate() {
        let status = repo
            .status_file(Path::new(rel.as_str()))
            .map_err(|e| engine_error("revert", &e))?;

        // Same opaque rule the status model states: a conflicted resource offers
        // no action, because acting on it can destroy a half-finished resolution.
        if classify(status).first().is_some_and(|(state, _)| state.is_opaque()) {
            return Err(ScmError::OpaqueResource { path: rel.clone() });
        }

        // A rename is reported to clients as **one** resource, so discarding it has
        // to undo the whole move: bring back the path the file came from, and send
        // the path it moved to through the trash. Treating the new path as a plain
        // untracked creation — which is what its raw flags look like — would trash
        // it and leave the old path missing, i.e. lose the file from both places
        // while reporting success.
        if let Some(from) = rename_sources.get(rel.as_str()) {
            renamed.push((index, from.clone(), workdir.join(rel)));
            continue;
        }

        if status.contains(Status::WT_NEW) && !status.intersects(Status::INDEX_NEW) {
            to_trash.push((index, workdir.join(rel)));
        } else {
            tracked.push((index, rel));
        }
    }

    // Failures are collected, not returned early: the files in one request are
    // independent, and stopping midway would leave earlier ones already changed
    // while telling the caller only "failed".
    let mut failed: Vec<(usize, String)> = Vec::new();

    if !tracked.is_empty() {
        // Restored from the **index**, not from the last commit.
        //
        // This is what discarding a working-tree change means, and it matches the
        // version-control system's own behaviour: restoring from the index drops
        // the unstaged edit and keeps whatever was staged, whereas restoring from
        // the last commit would additionally throw away the staged version — and
        // doing that *without* also updating the index produces a state the engine
        // itself never creates: work tree at the commit, index still holding the
        // staged version, so the file shows changes on **both** sides and the
        // change count does not go down even though the user asked to discard.
        //
        // For a file whose staged version equals the committed one — the common
        // case — the two sources are identical, so this needs no special casing.
        //
        // Checked out one at a time so one unreadable path does not cost the other
        // files their restore.
        for (index, rel) in &tracked {
            let mut builder = git2::build::CheckoutBuilder::new();
            builder.force().remove_untracked(false).update_index(false);
            builder.path(rel.as_str());
            // `None` = the repository's own index.
            if let Err(err) = repo.checkout_index(None, Some(&mut builder)) {
                failed.push((*index, err.message().to_owned()));
            }
        }
    }

    // Undo each rename as the pair it is: restore the source, then remove the
    // destination. Source first, so a failure to restore leaves the file present at
    // its new path rather than at neither.
    for (index, from, to) in renamed {
        let mut builder = git2::build::CheckoutBuilder::new();
        builder.force().remove_untracked(false).update_index(false);
        builder.path(from.as_str());
        // From HEAD, not the index: a staged rename has removed the source from the
        // index, so the index has nothing to restore it from.
        let restored = match repo.head().and_then(|h| h.peel(git2::ObjectType::Commit)) {
            Ok(obj) => repo
                .checkout_tree(&obj, Some(&mut builder))
                .map_err(|err| err.message().to_owned()),
            Err(err) => Err(err.message().to_owned()),
        };
        if let Err(message) = restored {
            failed.push((index, format!("restoring {from} failed: {message}")));
            continue;
        }
        if let Err(message) = trash.trash(&to) {
            failed.push((index, format!("move to trash failed: {message}")));
        }
    }

    for (index, path) in to_trash {
        // Never swallowed and never followed by a delete: a failed trash move must
        // leave the file in place.
        if let Err(message) = trash.trash(&path) {
            failed.push((index, format!("move to trash failed: {message}")));
        }
    }

    Ok(failed)
}

/// Every rename in the repository, as `destination -> source`.
///
/// Built in **one** pass and consulted from memory afterwards. Two reasons, and
/// both matter:
///
///  * Correctness. Scanning per file invites the mistake of aborting the scan on
///    the first entry that is not a rename — `?` inside such a loop returns from
///    the whole function, not just that iteration, so a single unrelated modified
///    file would hide every rename behind it. Here non-renames are simply skipped.
///  * Cost. A repository-wide status is not cheap, and doing one per requested
///    file turns a multi-file discard into as many full scans as there are files.
fn rename_sources(repo: &Repository) -> HashMap<String, String> {
    let mut opts = StatusOptions::new();
    opts.show(StatusShow::IndexAndWorkdir)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return HashMap::new();
    };

    let mut sources = HashMap::new();
    for entry in statuses.iter() {
        // Each side is checked independently, and anything that is not a rename is
        // skipped rather than ending the scan.
        let pair = entry
            .head_to_index()
            .filter(|d| d.status() == Delta::Renamed)
            .or_else(|| entry.index_to_workdir().filter(|d| d.status() == Delta::Renamed));
        let Some(pair) = pair else { continue };
        let (Some(from), Some(to)) = (pair.old_file().path(), pair.new_file().path()) else {
            continue;
        };
        sources.insert(to.to_string_lossy().into_owned(), from.to_string_lossy().into_owned());
    }
    sources
}

/// Pair per-file failures back with the identities the caller passed in.
///
/// The engine works in repository-relative paths; the outward result must speak
/// the same `{ pe_id, relative_path }` identity the caller used, so failures are
/// carried back by **position** in the request rather than re-derived or matched
/// by path.
///
/// Position, not path, because the identity must be exactly the one the caller
/// sent: a path-based lookup can miss (duplicate entries, any normalisation the
/// engine applies) and would then have to invent something, and an invented
/// identity — a blank `pe_id`, say — matches nothing on the client and silently
/// turns "this file failed" into "something failed, unclear what". Every engine
/// site derives its index from the same request slice, so an out-of-range index
/// means an internal invariant broke; that is reported as a whole-request failure
/// instead of being papered over.
fn outcome_of(files: &[FileRef], failed: Vec<(usize, String)>) -> Result<ScmActionOutcome, ScmError> {
    let mut failures = Vec::with_capacity(failed.len());
    for (index, reason) in failed {
        let Some(file) = files.get(index) else {
            debug_assert!(false, "failure index {index} out of range for {} files", files.len());
            return Err(ScmError::OperationFailed {
                context: "action_result",
                message: format!(
                    "internal: failure index {index} out of range for {} requested files",
                    files.len()
                ),
            });
        };
        failures.push(ScmActionFailure {
            file: file.clone(),
            reason,
        });
    }
    Ok(ScmActionOutcome { failed: failures })
}

/// Open the repository for one operation, on a blocking thread.
///
/// Every git2 access funnels through here so no `Repository` (not `Send`) is
/// ever held across an await.
async fn with_repo<T, F>(workdir: PathBuf, context: &'static str, f: F) -> Result<T, ScmError>
where
    T: Send + 'static,
    F: FnOnce(&Repository) -> Result<T, ScmError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let repo = Repository::open(&workdir).map_err(|e| engine_error(context, &e))?;
        f(&repo)
    })
    .await
    .map_err(|err| ScmError::OperationFailed {
        context,
        message: format!("blocking task failed: {err}"),
    })?
}

/// Validate the requested anchors against this provider's capabilities.
///
/// git has a staging area, so `Staged` is always allowed here; the check exists
/// so the rule lives with the contract rather than only in a provider that
/// happens to support it.
fn check_anchor(caps: ScmCapabilities, anchor: ContentRef) -> Result<(), ScmError> {
    if matches!(anchor, ContentRef::Staged) && !caps.staging {
        return Err(ScmError::unsupported_staged_anchor());
    }
    Ok(())
}

#[async_trait]
impl IScmProvider for GitScmProvider {
    fn provider_id(&self) -> &str {
        "git"
    }

    fn capabilities(&self) -> ScmCapabilities {
        ScmCapabilities {
            staging: true,
            local_branches: true,
            // Stage 2: the engine could serve these (revwalk / network), but the
            // contract must not advertise what the protocol does not expose yet.
            history_graph: false,
            remote_ops: false,
        }
    }

    async fn discover(&self, root: &ResolvedRoot) -> Result<Option<ScmRepository>, ScmError> {
        let path = PathBuf::from(&root.absolute_path);
        let caps = self.capabilities();

        // `open` (not `discover`): one pe root is at most one repo — never walk
        // up to a parent repository, never enumerate nested ones.
        let opened = tokio::task::spawn_blocking(move || match Repository::open(&path) {
            Ok(repo) => {
                let workdir = repo.workdir().map(Path::to_path_buf);
                // `path()` is the real git dir, which is what a watch must be
                // armed on: for a linked worktree or a submodule the `.git`
                // entry beside the work tree is a file pointing elsewhere.
                let git_dir = repo.path().to_path_buf();
                Ok(Some((workdir, git_dir, read_head(&repo))))
            }
            Err(err) if matches!(err.code(), ErrorCode::NotFound) => Ok(None),
            Err(err) => Err(err),
        })
        .await
        .map_err(|err| ScmError::OperationFailed {
            context: "discover",
            message: format!("blocking task failed: {err}"),
        })?
        .map_err(|e| engine_error("discover", &e))?;

        let Some((workdir, git_dir, head)) = opened else {
            return Ok(None);
        };
        // A bare repository has no work tree, so it has no change list to show;
        // treating it as "not a repository" here beats surfacing a repo whose
        // every operation would fail.
        let Some(workdir) = workdir else {
            tracing::debug!(pe_id = %root.pe_id, "scm discover: bare repository has no work tree, not surfaced");
            return Ok(None);
        };
        let repo_id = Self::repo_id_for(&root.pe_id);
        self.repos
            .write()
            .expect("scm repo registry poisoned")
            .insert(repo_id.clone(), RepoEntry { workdir, git_dir });

        Ok(Some(ScmRepository {
            repo_id,
            provider_id: self.provider_id().to_owned(),
            root: FileRef {
                pe_id: root.pe_id.clone(),
                relative_path: String::new(),
            },
            label: root.label.clone(),
            // Passed through untouched: identity resolution already decided
            // whether the entry has a name of its own, and the provider must not
            // second-guess it.
            pe_name: root.pe_name.clone(),
            head,
            capabilities: caps,
            state: ScmRepositoryState::Idle,
        }))
    }

    async fn status(&self, repo: &RepoRef) -> Result<ScmStatus, ScmError> {
        let entry = self.entry(repo)?;
        // Identity is deliberately left unassembled: a provider knows only
        // repository-relative paths, and pe identity comes from the resolved root,
        // so the orchestration layer owns it (`formal/runtime/source-control.md`).
        // Resources therefore carry an empty `pe_id` until that layer fills it.
        let (resources, truncated, degraded, head) = with_repo(entry.workdir, "status", collect_status).await?;

        if truncated {
            tracing::info!(
                repo_id = %repo.repo_id,
                limit = STATUS_RESOURCE_LIMIT,
                "scm status: resource list truncated at limit"
            );
        }

        Ok(ScmStatus {
            repository: repo.clone(),
            resources,
            head,
            // Left at zero: allocating the sequence is the orchestration layer's
            // job, inside the critical section that serializes recomputes.
            seq: 0,
            truncated,
            degraded,
        })
    }

    async fn diff(
        &self,
        repo: &RepoRef,
        file: &FileRef,
        from: ContentRef,
        to: ContentRef,
    ) -> Result<DiffContent, ScmError> {
        check_anchor(self.capabilities(), from)?;
        check_anchor(self.capabilities(), to)?;
        let entry = self.entry(repo)?;
        let rel = file.relative_path.clone();
        with_repo(entry.workdir, "diff", move |r| diff_between(r, &rel, from, to)).await
    }

    async fn original(&self, repo: &RepoRef, file: &FileRef, at: ContentRef) -> Result<Option<Vec<u8>>, ScmError> {
        check_anchor(self.capabilities(), at)?;
        let entry = self.entry(repo)?;
        let rel = file.relative_path.clone();
        with_repo(entry.workdir, "original", move |r| read_at(r, &rel, at)).await
    }

    async fn revert(&self, repo: &RepoRef, files: &[FileRef]) -> Result<ScmActionOutcome, ScmError> {
        let entry = self.entry(repo)?;
        let rels: Vec<String> = files.iter().map(|f| f.relative_path.clone()).collect();
        let trash = Arc::clone(&self.trash);
        let failed = with_repo(entry.workdir, "revert", move |r| revert_paths(r, &rels, trash.as_ref())).await?;
        outcome_of(files, failed)
    }

    fn staging(&self) -> Option<&dyn IScmStaging> {
        Some(self)
    }
}

#[async_trait]
impl IScmStaging for GitScmProvider {
    async fn stage(&self, repo: &RepoRef, files: &[FileRef]) -> Result<ScmActionOutcome, ScmError> {
        let entry = self.entry(repo)?;
        let rels: Vec<String> = files.iter().map(|f| f.relative_path.clone()).collect();

        let failed = with_repo(entry.workdir, "stage", move |r| {
            let mut index = r.index().map_err(|e| engine_error("stage", &e))?;
            // Pre-check every file before touching the index, so a conflicted
            // selection is refused without half-staging the rest.
            for rel in &rels {
                let status = r
                    .status_file(Path::new(rel.as_str()))
                    .map_err(|e| engine_error("stage", &e))?;
                if classify(status).first().is_some_and(|(state, _)| state.is_opaque()) {
                    return Err(ScmError::OpaqueResource { path: rel.clone() });
                }
            }

            let mut failed: Vec<(usize, String)> = Vec::new();
            for (slot, rel) in rels.iter().enumerate() {
                let path = Path::new(rel.as_str());
                // A deletion must be recorded as a removal: `add_path` on a
                // missing file fails, which would make staging a delete error.
                let exists = r.workdir().is_some_and(|w| w.join(path).exists());
                let res = if exists {
                    index.add_path(path)
                } else {
                    index.remove_path(path)
                };
                if let Err(err) = res {
                    failed.push((slot, err.message().to_owned()));
                }
            }
            // Written once: the files that were added stay staged even though a
            // sibling failed, which is what best effort means here.
            index.write().map_err(|e| engine_error("stage", &e))?;
            Ok(failed)
        })
        .await?;
        outcome_of(files, failed)
    }

    async fn unstage(&self, repo: &RepoRef, files: &[FileRef]) -> Result<ScmActionOutcome, ScmError> {
        let entry = self.entry(repo)?;
        let rels: Vec<String> = files.iter().map(|f| f.relative_path.clone()).collect();

        let failed = with_repo(entry.workdir, "unstage", move |r| {
            // Same pre-check as stage and discard: a conflicted selection is
            // refused before the index is touched, so all three actions offer the
            // identical all-or-nothing guarantee for blocked resources.
            for rel in &rels {
                let status = r
                    .status_file(Path::new(rel.as_str()))
                    .map_err(|e| engine_error("unstage", &e))?;
                if classify(status).first().is_some_and(|(state, _)| state.is_opaque()) {
                    return Err(ScmError::OpaqueResource { path: rel.clone() });
                }
            }

            match r.head().and_then(|h| h.peel(git2::ObjectType::Commit)) {
                Ok(obj) => {
                    // Fast path first: resetting the whole selection in one call is
                    // dramatically cheaper than one call per file — measured at
                    // roughly 15× for a hundred files and 35× for a few thousand,
                    // which is the difference between instant and a visible stall.
                    // Only when it fails is the batch retried file by file, to find
                    // out *which* entries are at fault; correctness of the per-file
                    // report is preserved without paying for it on every request.
                    let mut failed: Vec<(usize, String)> = Vec::new();
                    if r.reset_default(Some(&obj), rels.iter().map(String::as_str)).is_err() {
                        for (index, rel) in rels.iter().enumerate() {
                            if let Err(err) = r.reset_default(Some(&obj), [rel.as_str()].iter()) {
                                failed.push((index, err.message().to_owned()));
                            }
                        }
                    }
                    Ok(failed)
                }
                // Unborn head: there is no committed version to reset to, so
                // unstaging means dropping the entry from the index entirely.
                Err(err) if err.code() == ErrorCode::UnbornBranch => {
                    let mut index = r.index().map_err(|e| engine_error("unstage", &e))?;
                    let mut failed: Vec<(usize, String)> = Vec::new();
                    for (index_of, rel) in rels.iter().enumerate() {
                        if let Err(err) = index.remove_path(Path::new(rel.as_str())) {
                            failed.push((index_of, err.message().to_owned()));
                        }
                    }
                    index.write().map_err(|e| engine_error("unstage", &e))?;
                    Ok(failed)
                }
                Err(err) => Err(engine_error("unstage", &err)),
            }
        })
        .await?;
        outcome_of(files, failed)
    }
}

#[cfg(test)]
#[path = "git_provider_test.rs"]
mod git_provider_test;
