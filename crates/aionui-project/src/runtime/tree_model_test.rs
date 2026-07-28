use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tempfile::{TempDir, tempdir};

use crate::canonical::{self, Canonical};
use crate::runtime::error::FsError;
use crate::runtime::fs_runtime::{FsRuntimeRegistry, IFsRuntime, IoDispatch};
use crate::runtime::provider::{EntryFact, IFsProvider, Kind};
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

fn file_fact(inode: u64) -> EntryFact {
    EntryFact {
        kind: Kind::File,
        inode,
        symlink_target: None,
    }
}

fn dir_fact(inode: u64) -> EntryFact {
    EntryFact {
        kind: Kind::Dir,
        inode,
        symlink_target: None,
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
