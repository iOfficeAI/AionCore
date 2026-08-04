use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tempfile::{TempDir, tempdir};

use crate::canonical::{self, Canonical};
use crate::runtime::error::FsError;
use crate::runtime::fs_runtime::{FsRuntimeRegistry, IFsRuntime, IoDispatch};
use crate::runtime::provider::{EntryFact, IFsProvider, Kind};
use crate::runtime::search::IFsSearchProvider;
use crate::runtime::watcher::{WatchHandle, Watcher};

use super::{Change, Hint, TreeModel, diff};

fn canon(path: &Path) -> Canonical {
    let uri = canonical::to_file_uri(path).expect("to_file_uri");
    canonical::canonicalize(&uri).expect("canonicalize")
}

/// A tree model over a real local runtime rooted at a fresh tempdir.
fn real_tree() -> (TreeModel, TempDir) {
    let (runtime, _rx) = crate::runtime::LocalFsRuntime::new().unwrap();
    let mut registry = FsRuntimeRegistry::new();
    registry.register("file", Arc::new(runtime));
    (TreeModel::new(registry), tempdir().unwrap())
}

fn names(entries: &[(String, EntryFact)]) -> Vec<&str> {
    entries.iter().map(|(n, _)| n.as_str()).collect()
}

#[tokio::test]
async fn mount_returns_baseline_snapshot() {
    let (mut tree, dir) = real_tree();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("README.md"), b"x").unwrap();

    let snap = tree.mount(canon(dir.path()).as_str()).await.unwrap();
    assert_eq!(names(&snap.entries), vec!["README.md", "src"]);
}

#[tokio::test]
async fn apply_all_detects_added_and_removed() {
    let (mut tree, dir) = real_tree();
    std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
    std::fs::write(dir.path().join("gone.txt"), b"x").unwrap();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    std::fs::write(dir.path().join("new.txt"), b"x").unwrap();
    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();

    let delta = tree.apply(c.as_str(), Hint::All).await.unwrap().expect("changes");
    let mut changes = delta.changes;
    changes.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        changes,
        vec![
            Change::Added {
                name: "new.txt".to_owned(),
                kind: Kind::File
            },
            Change::Removed {
                name: "gone.txt".to_owned()
            },
        ]
    );
}

#[tokio::test]
async fn apply_child_names_stats_only_named_children() {
    let (mut tree, dir) = real_tree();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    std::fs::write(dir.path().join("added.txt"), b"x").unwrap();
    // Also create an unrelated file, but do NOT name it in the hint → ignored.
    std::fs::write(dir.path().join("unhinted.txt"), b"x").unwrap();

    let delta = tree
        .apply(c.as_str(), Hint::ChildNames(vec!["added.txt".to_owned()]))
        .await
        .unwrap()
        .expect("changes");
    assert_eq!(
        delta.changes,
        vec![Change::Added {
            name: "added.txt".to_owned(),
            kind: Kind::File
        }]
    );
}

#[tokio::test]
async fn apply_child_names_detects_rewritten_file_over_real_fs() {
    // End-to-end over a real filesystem, which the synthetic `diff` tests cannot
    // cover: it is `fact_of` that must actually read the timestamp off the
    // metadata it already fetched. Were mtime left unpopulated, every test above
    // would still pass while nothing was ever detected in production.
    //
    // The new mtime is set explicitly rather than by writing twice: some
    // filesystems record whole seconds only, so back-to-back writes can leave the
    // timestamp untouched and the assertion would flake. `File::set_modified` is
    // stable and available on all supported targets.
    let (mut tree, dir) = real_tree();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, b"before").unwrap();
    let c = canon(dir.path());
    let baseline = tree.mount(c.as_str()).await.unwrap();
    let mtime_before = baseline.entries[0].1.mtime_ms.expect("real fs supplies an mtime");

    std::fs::write(&path, b"after").unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(bumped)
        .unwrap();

    let delta = tree
        .apply(c.as_str(), Hint::ChildNames(vec!["a.txt".to_owned()]))
        .await
        .unwrap()
        .expect("changes");
    assert_eq!(
        delta.changes,
        vec![Change::Modified {
            name: "a.txt".to_owned()
        }]
    );

    // The node's stored fact advanced, so re-applying with nothing further changed
    // is silent — a Modified that failed to update state would fire on every event.
    let after = tree.snapshot(c.as_str()).expect("mounted");
    assert_ne!(after.entries[0].1.mtime_ms.expect("mtime"), mtime_before);
    assert!(
        tree.apply(c.as_str(), Hint::ChildNames(vec!["a.txt".to_owned()]))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn apply_child_names_removes_deleted_subdir() {
    // The parent-reconcile half of the filetree-stale fix: once the watcher
    // attributes a subdir deletion to the PARENT (see local_watcher_test), the
    // parent applies `ChildNames([sub])` and must emit `Removed{sub}` so the
    // stale subdir node drops out of the listing.
    let (mut tree, dir) = real_tree();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    std::fs::remove_dir_all(dir.path().join("a")).unwrap();

    let delta = tree
        .apply(c.as_str(), Hint::ChildNames(vec!["a".to_owned()]))
        .await
        .unwrap()
        .expect("changes");
    assert_eq!(delta.changes, vec![Change::Removed { name: "a".to_owned() }]);
}

#[tokio::test]
async fn apply_child_names_renamed_subdir_surfaces_on_parent() {
    // Renaming an expanded subdir a→a2: the watcher attributes events on both
    // paths to the parent, which applies `ChildNames([a, a2])`. A real rename
    // preserves the inode, so the parent's diff coalesces removed+added into a
    // single `Renamed{a→a2}` (the frontend applies it in place) — the parent's
    // listing is reconciled either way, never left showing a stale `a`.
    let (mut tree, dir) = real_tree();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    std::fs::rename(dir.path().join("a"), dir.path().join("a2")).unwrap();

    let delta = tree
        .apply(c.as_str(), Hint::ChildNames(vec!["a".to_owned(), "a2".to_owned()]))
        .await
        .unwrap()
        .expect("changes");
    // Real-FS inode is preserved across rename → synthesized Renamed. On any
    // platform that reports inode 0 (e.g. Windows) this degrades to
    // Removed{a}+Added{a2}; both leave the parent listing correct, so assert the
    // stale `a` is gone rather than pinning one representation.
    let has_stale_a = delta.changes.iter().any(|ch| {
        matches!(ch, Change::Added { name, .. } if name == "a") || matches!(ch, Change::Renamed { to, .. } if to == "a")
    });
    let drops_a = delta.changes.iter().any(|ch| {
        matches!(ch, Change::Removed { name } if name == "a")
            || matches!(ch, Change::Renamed { from, .. } if from == "a")
    });
    assert!(drops_a, "parent must drop the old `a` entry, got {:?}", delta.changes);
    assert!(
        !has_stale_a,
        "parent must not keep a stale `a`, got {:?}",
        delta.changes
    );
}

#[tokio::test]
async fn apply_is_idempotent_when_nothing_changed() {
    let (mut tree, dir) = real_tree();
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    assert!(tree.apply(c.as_str(), Hint::All).await.unwrap().is_none());
    // Re-applying a stale hint for a child that did not change is also a no-op.
    assert!(
        tree.apply(c.as_str(), Hint::ChildNames(vec!["a.txt".to_owned()]))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn apply_synthesizes_rename_for_same_inode() {
    let (mut tree, dir) = real_tree();
    std::fs::write(dir.path().join("old.txt"), b"x").unwrap();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    std::fs::rename(dir.path().join("old.txt"), dir.path().join("new.txt")).unwrap();

    let delta = tree.apply(c.as_str(), Hint::All).await.unwrap().expect("changes");
    assert_eq!(
        delta.changes,
        vec![Change::Renamed {
            from: "old.txt".to_owned(),
            to: "new.txt".to_owned()
        }]
    );
}

#[tokio::test]
async fn apply_kind_change_is_remove_plus_add() {
    let (mut tree, dir) = real_tree();
    std::fs::write(dir.path().join("x"), b"data").unwrap();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();

    std::fs::remove_file(dir.path().join("x")).unwrap();
    std::fs::create_dir(dir.path().join("x")).unwrap();

    let delta = tree.apply(c.as_str(), Hint::All).await.unwrap().expect("changes");
    let mut changes = delta.changes;
    changes.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        changes,
        vec![
            Change::Added {
                name: "x".to_owned(),
                kind: Kind::Dir
            },
            Change::Removed { name: "x".to_owned() },
        ]
    );
}

// ── Pure `diff` reconciliation: rename synthesis vs inode=0 degradation ────
//
// `diff` is the private reconciliation core. The rename-synthesis guard keys on
// a stable non-zero inode; when the provider cannot supply one (`inode == 0` —
// the entire Windows rename path, `local_provider::inode_of` on `not(unix)`),
// synthesis must degrade to removed+added. This branch is distinct from the
// same-name kind-change branch (covered by `apply_kind_change_is_remove_plus_add`)
// and requires its own coverage.

/// Baseline mtime for facts whose timestamp is irrelevant to the assertion.
/// Shared by both sides of a diff so it never accidentally reads as modified.
const MTIME: i64 = 1_700_000_000_000;

fn file_fact(inode: u64) -> EntryFact {
    file_fact_at(inode, Some(MTIME))
}

/// `file_fact` with an explicit mtime — `None` models a provider/filesystem that
/// cannot supply one.
fn file_fact_at(inode: u64, mtime_ms: Option<i64>) -> EntryFact {
    EntryFact {
        kind: Kind::File,
        inode,
        symlink_target: None,
        mtime_ms,
    }
}

fn dir_fact(inode: u64) -> EntryFact {
    dir_fact_at(inode, Some(MTIME))
}

fn dir_fact_at(inode: u64, mtime_ms: Option<i64>) -> EntryFact {
    EntryFact {
        kind: Kind::Dir,
        inode,
        symlink_target: None,
        mtime_ms,
    }
}

fn symlink_fact_at(inode: u64, mtime_ms: Option<i64>) -> EntryFact {
    EntryFact {
        kind: Kind::Symlink,
        inode,
        symlink_target: Some("target".to_owned()),
        mtime_ms,
    }
}

#[test]
fn diff_same_inode_kind_change_is_remove_add_not_rename() {
    // Same name "x", same inode, but File→Dir. Reproduces the Linux inode-reuse
    // case (freed file inode reassigned to the new dir) deterministically, with
    // no dependency on real-FS inode behavior. A rename preserves kind, so a
    // kind change must be Removed + Added even when the inode collides — never a
    // (nonsensical) self-rename `Renamed { from: "x", to: "x" }`.
    let old = BTreeMap::from([("x".to_owned(), file_fact(7))]);
    let fresh = BTreeMap::from([("x".to_owned(), dir_fact(7))]);

    let mut changes = diff(&old, &fresh);
    changes.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        changes,
        vec![
            Change::Added {
                name: "x".to_owned(),
                kind: Kind::Dir
            },
            Change::Removed { name: "x".to_owned() },
        ]
    );
}

#[test]
fn diff_inode_zero_rename_degrades_to_remove_add() {
    // Same content moved a→b, but the provider reports inode 0 (unknown) for
    // both — as on Windows. Without a stable inode there is nothing to match,
    // so this must be Removed{a} + Added{b}, NOT a synthesized Renamed.
    let old = BTreeMap::from([("a".to_owned(), file_fact(0))]);
    let fresh = BTreeMap::from([("b".to_owned(), file_fact(0))]);

    let mut changes = diff(&old, &fresh);
    changes.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        changes,
        vec![
            Change::Added {
                name: "b".to_owned(),
                kind: Kind::File
            },
            Change::Removed { name: "a".to_owned() },
        ]
    );
}

#[test]
fn diff_same_nonzero_inode_synthesizes_rename() {
    // Contrast case: identical non-zero inode on both sides → the removed+added
    // pair is coalesced into one Renamed. Locks in "only a real inode synthesizes".
    let old = BTreeMap::from([("a".to_owned(), file_fact(42))]);
    let fresh = BTreeMap::from([("b".to_owned(), file_fact(42))]);

    assert_eq!(
        diff(&old, &fresh),
        vec![Change::Renamed {
            from: "a".to_owned(),
            to: "b".to_owned()
        }]
    );
}

// ── Pure `diff` reconciliation: content-modify detection ───────────────────
//
// A surviving file whose mtime moved is `Change::Modified`. The rules under test
// are all one-directional on purpose: this signal only lights up a refresh
// affordance, so failing to report is tolerable while reporting spuriously is
// not. Each test below pins one way that asymmetry must hold.

#[test]
fn diff_file_mtime_change_is_modified() {
    // Same name, same kind, same inode — only the timestamp moved. The listing is
    // unchanged, so nothing else may be emitted alongside it.
    let old = BTreeMap::from([("a.txt".to_owned(), file_fact_at(7, Some(MTIME)))]);
    let fresh = BTreeMap::from([("a.txt".to_owned(), file_fact_at(7, Some(MTIME + 1)))]);

    assert_eq!(
        diff(&old, &fresh),
        vec![Change::Modified {
            name: "a.txt".to_owned()
        }]
    );
}

#[test]
fn diff_identical_mtime_is_no_change() {
    // The idempotence guard: re-reconciling an untouched level must stay silent,
    // otherwise every apply would emit a delta for every entry.
    let old = BTreeMap::from([("a.txt".to_owned(), file_fact(7))]);
    let fresh = BTreeMap::from([("a.txt".to_owned(), file_fact(7))]);

    assert_eq!(diff(&old, &fresh), vec![]);
}

#[test]
fn diff_unknown_mtime_on_either_side_is_never_modified() {
    // `None` means "cannot tell", never "changed". Both directions matter: the
    // old→new direction is the first reconcile after a provider starts supplying
    // timestamps, and new→old is a provider that stopped. Neither may report the
    // whole level as modified.
    let known = file_fact_at(7, Some(MTIME));
    let unknown = file_fact_at(7, None);

    for (old_fact, fresh_fact) in [
        (unknown.clone(), known.clone()),
        (known.clone(), unknown.clone()),
        (unknown.clone(), unknown.clone()),
    ] {
        let old = BTreeMap::from([("a.txt".to_owned(), old_fact)]);
        let fresh = BTreeMap::from([("a.txt".to_owned(), fresh_fact)]);
        assert_eq!(diff(&old, &fresh), vec![], "unknown mtime must not report modified");
    }
}

#[test]
fn diff_dir_and_symlink_mtime_change_is_not_modified() {
    // Only files carry a content-modify signal. A directory's mtime also moves
    // when a child is created or deleted — that is already expressed by the
    // child's own Added/Removed change, so reporting the parent too would be
    // duplicate noise. A symlink's mtime describes the link itself, not the
    // content a subscriber would re-read.
    let old = BTreeMap::from([
        ("sub".to_owned(), dir_fact_at(7, Some(MTIME))),
        ("link".to_owned(), symlink_fact_at(8, Some(MTIME))),
    ]);
    let fresh = BTreeMap::from([
        ("sub".to_owned(), dir_fact_at(7, Some(MTIME + 1))),
        ("link".to_owned(), symlink_fact_at(8, Some(MTIME + 1))),
    ]);

    assert_eq!(diff(&old, &fresh), vec![]);
}

#[test]
fn diff_kind_change_with_moved_mtime_is_remove_add_not_modified() {
    // A file replaced by a same-named directory changes kind *and* mtime. Kind
    // change wins: Removed + Added. Emitting Modified as well would tell a
    // subscriber to re-read a path that is no longer a file.
    let old = BTreeMap::from([("x".to_owned(), file_fact_at(7, Some(MTIME)))]);
    let fresh = BTreeMap::from([("x".to_owned(), dir_fact_at(9, Some(MTIME + 1)))]);

    let mut changes = diff(&old, &fresh);
    changes.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        changes,
        vec![
            Change::Added {
                name: "x".to_owned(),
                kind: Kind::Dir
            },
            Change::Removed { name: "x".to_owned() },
        ]
    );
}

#[test]
fn diff_rename_with_moved_mtime_stays_a_single_rename() {
    // A rename does not survive under its old name, so it is not a Modified
    // candidate — even when the move also bumped the timestamp. Guards against
    // a future refactor matching Modified by inode rather than by surviving name.
    let old = BTreeMap::from([("a".to_owned(), file_fact_at(42, Some(MTIME)))]);
    let fresh = BTreeMap::from([("b".to_owned(), file_fact_at(42, Some(MTIME + 1)))]);

    assert_eq!(
        diff(&old, &fresh),
        vec![Change::Renamed {
            from: "a".to_owned(),
            to: "b".to_owned()
        }]
    );
}

#[test]
fn diff_reports_modified_alongside_other_changes() {
    // A debounce window batches unrelated changes to one level: one file rewritten,
    // one created, one deleted. All three must surface in the same batch — a
    // Modified must not mask or be masked by its neighbours.
    let old = BTreeMap::from([
        ("kept.txt".to_owned(), file_fact_at(1, Some(MTIME))),
        ("gone.txt".to_owned(), file_fact_at(2, Some(MTIME))),
    ]);
    let fresh = BTreeMap::from([
        ("kept.txt".to_owned(), file_fact_at(1, Some(MTIME + 1))),
        ("new.txt".to_owned(), file_fact_at(3, Some(MTIME))),
    ]);

    let mut changes = diff(&old, &fresh);
    changes.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        changes,
        vec![
            Change::Added {
                name: "new.txt".to_owned(),
                kind: Kind::File
            },
            Change::Modified {
                name: "kept.txt".to_owned()
            },
            Change::Removed {
                name: "gone.txt".to_owned()
            },
        ]
    );
}

#[tokio::test]
async fn unmount_removes_node_and_snapshot_is_none() {
    let (mut tree, dir) = real_tree();
    let c = canon(dir.path());
    tree.mount(c.as_str()).await.unwrap();
    assert!(tree.snapshot(c.as_str()).is_some());

    tree.unmount(c.as_str());
    assert!(tree.snapshot(c.as_str()).is_none());
}

#[tokio::test]
async fn apply_on_unmounted_canonical_is_none() {
    let (mut tree, dir) = real_tree();
    // Never mounted → in-flight-after-unmount guard returns None, not an error.
    let c = canon(dir.path());
    assert!(tree.apply(c.as_str(), Hint::All).await.unwrap().is_none());
}

// ── TOCTOU ordering: a fake runtime that logs call order ──────────────────

#[derive(Clone, Default)]
struct CallLog(Arc<Mutex<Vec<&'static str>>>);
impl CallLog {
    fn push(&self, s: &'static str) {
        self.0.lock().unwrap().push(s);
    }
    fn calls(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().clone()
    }
}

struct FakeProvider {
    log: CallLog,
}
#[async_trait]
impl IFsProvider for FakeProvider {
    fn scheme(&self) -> &str {
        "file"
    }
    async fn read_dir(&self, _uri: &str) -> Result<Vec<(String, EntryFact)>, FsError> {
        self.log.push("read_dir");
        Ok(vec![])
    }
    async fn stat(&self, _uri: &str) -> Result<Option<EntryFact>, FsError> {
        Ok(None)
    }
    async fn read(&self, _uri: &str) -> Result<Vec<u8>, FsError> {
        Ok(vec![])
    }
    async fn write(&self, _uri: &str, _data: &[u8]) -> Result<(), FsError> {
        Ok(())
    }
    async fn create_file(&self, _uri: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn mkdir(&self, _uri: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn remove(&self, _uri: &str, _recursive: bool) -> Result<(), FsError> {
        Ok(())
    }
    async fn rename(&self, _from: &str, _to: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn copy(&self, _from: &str, _to: &str, _recursive: bool) -> Result<(), FsError> {
        Ok(())
    }
}

struct FakeWatcher {
    log: CallLog,
}
impl Watcher for FakeWatcher {
    fn watch(&self, canonical: &str) -> Result<WatchHandle, FsError> {
        self.log.push("watch");
        Ok(WatchHandle {
            canonical: canonical.to_owned(),
        })
    }
    fn unwatch(&self, _handle: &WatchHandle) {
        self.log.push("unwatch");
    }
}

struct FakeRuntime {
    provider: FakeProvider,
    watcher: FakeWatcher,
}
impl IFsRuntime for FakeRuntime {
    fn provider(&self) -> &dyn IFsProvider {
        &self.provider
    }
    fn watcher(&self) -> &dyn Watcher {
        &self.watcher
    }
    fn io_dispatch(&self) -> IoDispatch {
        IoDispatch::Inline
    }
    fn search_provider(&self) -> Option<Arc<dyn IFsSearchProvider>> {
        // The tree model never uses search; this fake need not provide it.
        None
    }
}

#[tokio::test]
async fn mount_arms_watch_before_reading_baseline() {
    let log = CallLog::default();
    let runtime = FakeRuntime {
        provider: FakeProvider { log: log.clone() },
        watcher: FakeWatcher { log: log.clone() },
    };
    let mut registry = FsRuntimeRegistry::new();
    registry.register("file", Arc::new(runtime));
    let mut tree = TreeModel::new(registry);

    tree.mount("file:///tmp/x").await.unwrap();

    // Watch armed strictly before the baseline read — no TOCTOU gap.
    assert_eq!(log.calls(), vec!["watch", "read_dir"]);
}
