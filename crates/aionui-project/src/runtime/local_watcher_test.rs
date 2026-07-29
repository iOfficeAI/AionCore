use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::tempdir;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::canonical::{self, Canonical};
use crate::runtime::watcher::{RawEvent, Watcher};

use super::{LocalWatcher, WatchEntry, resolve_owner};

fn canon(path: &Path) -> Canonical {
    let uri = canonical::to_file_uri(path).expect("to_file_uri");
    canonical::canonicalize(&uri).expect("canonicalize")
}

/// Receive events until one is a `Changed` for `canonical` mentioning a path
/// whose file name is `child`, or the deadline elapses (returns false).
async fn saw_change(rx: &mut UnboundedReceiver<RawEvent>, canonical: &str, child: &str, within: Duration) -> bool {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Some(RawEvent::Changed { canonical: c, paths }) if c == canonical => {
                    if paths.iter().any(|p| p.ends_with(child)) {
                        return true;
                    }
                }
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test]
async fn watch_emits_changed_on_child_create() {
    let dir = tempdir().unwrap();
    let (watcher, mut rx) = LocalWatcher::new().unwrap();
    let c = canon(dir.path());

    watcher.watch(c.as_str()).unwrap();
    // Let the OS watch arm before mutating.
    tokio::time::sleep(Duration::from_millis(150)).await;
    std::fs::write(dir.path().join("new.ts"), b"x").unwrap();

    assert!(
        saw_change(&mut rx, c.as_str(), "new.ts", Duration::from_secs(5)).await,
        "expected a Changed event mentioning new.ts"
    );
}

// ── Event-path attribution: the realpath double-key fold (pure, all platforms)
//
// FSEvents delivers realpath'd paths (macOS `/var` → `/private/var`); `watch`
// registers both the lexical canonical and a second (realpath-folded) canonical
// so an event reported under either name attributes back to the one lexical
// identity the tree model keys on. This test drives the private `resolve_owner`
// with synthetic paths — no real symlink, no FSEvents — so it runs and asserts
// on every platform. Paths are built platform-absolute (Windows needs a drive
// letter; `Url::from_file_path` rejects non-absolute paths), and the base dirs
// are the source of truth for the expected keys, so the assertion is
// self-consistent regardless of per-platform case folding.

/// A pair of distinct absolute dirs (a "lexical" dir and a second "alias" dir)
/// that are valid on the host platform.
#[cfg(windows)]
fn alias_dirs() -> (PathBuf, PathBuf) {
    (PathBuf::from(r"C:\a"), PathBuf::from(r"C:\private\a"))
}
#[cfg(not(windows))]
fn alias_dirs() -> (PathBuf, PathBuf) {
    (PathBuf::from("/a"), PathBuf::from("/private/a"))
}

#[test]
fn resolve_owner_folds_realpath_alias_back_to_lexical() {
    let (lex_dir, alias_dir) = alias_dirs();
    let lexical = canon(&lex_dir);
    let alias = canon(&alias_dir);
    let entry = WatchEntry {
        lexical: lexical.as_str().to_owned(),
        notify_path: lex_dir.clone(),
    };
    let mut watched: HashMap<String, WatchEntry> = HashMap::new();
    watched.insert(lexical.as_str().to_owned(), entry.clone());
    watched.insert(alias.as_str().to_owned(), entry);

    let want = Some(lexical.as_str().to_owned());
    // Alias'd child path (as macOS FSEvents reports) attributes to the lexical id.
    assert_eq!(resolve_owner(&watched, &alias_dir.join("child.txt")), want);
    // Lexical child path (as Linux inotify reports) attributes to the same id.
    assert_eq!(resolve_owner(&watched, &lex_dir.join("child.txt")), want);

    // A path under no watched directory is unowned (platform-absolute).
    #[cfg(windows)]
    let unrelated = PathBuf::from(r"C:\other\x.txt");
    #[cfg(not(windows))]
    let unrelated = PathBuf::from("/other/x.txt");
    assert_eq!(resolve_owner(&watched, &unrelated), None);
}

#[test]
fn resolve_owner_case_folding_is_platform_relative() {
    // The canonical layer folds path case on macOS/Windows and preserves it on
    // Linux (`canonical::IGNORE_PATH_CASING`). resolve_owner inherits that, so a
    // differently-cased event path resolves on case-insensitive platforms and is
    // unowned on case-sensitive ones. Assert each platform's real behavior.
    let (lex_dir, _) = alias_dirs();
    let lexical = canon(&lex_dir);
    let entry = WatchEntry {
        lexical: lexical.as_str().to_owned(),
        notify_path: lex_dir.clone(),
    };
    let mut watched: HashMap<String, WatchEntry> = HashMap::new();
    watched.insert(lexical.as_str().to_owned(), entry);

    // Upper-cased sibling of the watched dir.
    let upper = PathBuf::from(lex_dir.to_string_lossy().to_uppercase());
    let child = upper.join("child.txt");

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    assert_eq!(
        resolve_owner(&watched, &child),
        Some(lexical.as_str().to_owned()),
        "case-insensitive platform folds the cased path back to the watched id"
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    assert_eq!(
        resolve_owner(&watched, &child),
        None,
        "case-sensitive platform treats the cased path as a different directory"
    );
}

#[tokio::test]
async fn unwatch_stops_events() {
    let dir = tempdir().unwrap();
    let (watcher, mut rx) = LocalWatcher::new().unwrap();
    let c = canon(dir.path());

    let handle = watcher.watch(c.as_str()).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    watcher.unwatch(&handle);
    // Drain anything already queued from arming.
    tokio::time::sleep(Duration::from_millis(150)).await;
    while rx.try_recv().is_ok() {}

    std::fs::write(dir.path().join("after.ts"), b"x").unwrap();
    assert!(
        !saw_change(&mut rx, c.as_str(), "after.ts", Duration::from_millis(800)).await,
        "expected no events after unwatch"
    );
}

#[tokio::test]
async fn two_watches_share_instance_and_unwatch_is_isolated() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let (watcher, mut rx) = LocalWatcher::new().unwrap();
    let ca = canon(dir_a.path());
    let cb = canon(dir_b.path());

    // One shared watcher instance backs both directories (macOS: one FSEvents
    // stream). Both must report changes independently.
    let ha = watcher.watch(ca.as_str()).unwrap();
    watcher.watch(cb.as_str()).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    std::fs::write(dir_a.path().join("x.ts"), b"x").unwrap();
    assert!(
        saw_change(&mut rx, ca.as_str(), "x.ts", Duration::from_secs(5)).await,
        "dir_a change observed on the shared watcher"
    );
    std::fs::write(dir_b.path().join("y.ts"), b"y").unwrap();
    assert!(
        saw_change(&mut rx, cb.as_str(), "y.ts", Duration::from_secs(5)).await,
        "dir_b change observed on the shared watcher"
    );

    // Unwatching one directory must not disturb the other.
    watcher.unwatch(&ha);
    tokio::time::sleep(Duration::from_millis(150)).await;
    while rx.try_recv().is_ok() {}

    std::fs::write(dir_b.path().join("more.ts"), b"m").unwrap();
    assert!(
        saw_change(&mut rx, cb.as_str(), "more.ts", Duration::from_secs(5)).await,
        "dir_b still live after unwatching dir_a"
    );
    std::fs::write(dir_a.path().join("again.ts"), b"a").unwrap();
    assert!(
        !saw_change(&mut rx, ca.as_str(), "again.ts", Duration::from_millis(800)).await,
        "dir_a silent after unwatch, not affected by dir_b's watch"
    );
}
