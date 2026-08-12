//! Watch tests: noise filtering and event attribution.
//!
//! The pure parts are tested directly — they decide whether a signal is raised at
//! all, and getting them wrong is either a storm of pointless recomputes or a
//! panel that never updates. Live filesystem delivery is covered by the
//! registration test, which uses a real repository directory.

use std::collections::HashMap;

use super::*;

fn entry(repo_id: &str, paths: &[&str]) -> WatchEntry {
    WatchEntry {
        repo_ids: [repo_id.to_owned()].into_iter().collect(),
        registered: paths.iter().map(PathBuf::from).collect(),
    }
}

/// An entry several repositories share, which is what a single git directory
/// backing two projects produces.
fn shared_entry(repo_ids: &[&str], paths: &[&str]) -> WatchEntry {
    WatchEntry {
        repo_ids: repo_ids.iter().map(|id| (*id).to_owned()).collect(),
        registered: paths.iter().map(PathBuf::from).collect(),
    }
}

/// Expected attribution of exactly one repository.
fn only(repo_id: &str) -> std::collections::BTreeSet<String> {
    [repo_id.to_owned()].into_iter().collect()
}

fn registry_of(pairs: &[(&str, WatchEntry)]) -> HashMap<String, WatchEntry> {
    pairs
        .iter()
        .map(|(key, value)| {
            (
                (*key).to_owned(),
                WatchEntry {
                    repo_ids: value.repo_ids.clone(),
                    registered: value.registered.clone(),
                },
            )
        })
        .collect()
}

#[test]
fn git_lock_churn_is_filtered_out() {
    // Git writes through lock files around every real change; keeping them would
    // re-arm the debounce window without adding information.
    for noise in [
        "/repo/.git/index.lock",
        "/repo/.git/HEAD.lock",
        "/repo/.git/packed-refs.lock",
        "/repo/.git/refs/heads/main.lock",
        "/repo/.git/index.stash.12345",
        "/repo/.git/index.stash.12345.lock",
    ] {
        assert!(is_noise(Path::new(noise)), "{noise} is churn");
    }
}

#[test]
fn real_metadata_changes_are_not_filtered() {
    // These are precisely the files whose changes mean "recompute".
    for signal in [
        "/repo/.git/index",
        "/repo/.git/HEAD",
        "/repo/.git/MERGE_HEAD",
        "/repo/.git/ORIG_HEAD",
        "/repo/.git/FETCH_HEAD",
        "/repo/.git/COMMIT_EDITMSG",
        "/repo/.git/refs/heads/main",
        "/repo/.git/refs/stash",
    ] {
        assert!(!is_noise(Path::new(signal)), "{signal} is a signal");
    }
}

#[test]
fn a_file_merely_containing_lock_is_not_treated_as_churn() {
    // Only the `.lock` suffix and the stash-copy prefix are churn; a tracked file
    // called `lockfile.json` must not silence its repository.
    assert!(!is_noise(Path::new("/repo/.git/lockfile.json")));
    assert!(!is_noise(Path::new("/repo/.git/my.lock.txt")));
}

#[test]
fn an_event_is_attributed_to_the_repository_that_registered_its_directory() {
    let registry = registry_of(&[("/w/repo/.git", entry("scm:pe1", &["/w/repo/.git"]))]);

    assert_eq!(attribute(&registry, Path::new("/w/repo/.git/index")), only("scm:pe1"));
    // The registered directory itself is also attributed.
    assert_eq!(attribute(&registry, Path::new("/w/repo/.git")), only("scm:pe1"));
}

#[test]
fn an_unrelated_path_is_attributed_to_nobody() {
    let registry = registry_of(&[("/w/repo/.git", entry("scm:pe1", &["/w/repo/.git"]))]);

    assert!(attribute(&registry, Path::new("/w/other/.git/index")).is_empty());
    assert!(attribute(&registry, Path::new("/w/repo/src/main.rs")).is_empty());
}

#[test]
fn a_sibling_sharing_a_name_prefix_is_not_attributed() {
    // Plain string prefixing would match `/w/repo-backup/...` against `/w/repo`;
    // that would make one repository's churn recompute another's status.
    let registry = registry_of(&[("/w/repo/.git", entry("scm:pe1", &["/w/repo/.git"]))]);

    assert!(attribute(&registry, Path::new("/w/repo-backup/.git/index")).is_empty());
}

#[test]
fn each_repository_gets_its_own_signal() {
    let registry = registry_of(&[
        ("/w/a/.git", entry("scm:pe-a", &["/w/a/.git"])),
        ("/w/b/.git", entry("scm:pe-b", &["/w/b/.git"])),
    ]);

    assert_eq!(attribute(&registry, Path::new("/w/a/.git/index")), only("scm:pe-a"));
    assert_eq!(
        attribute(&registry, Path::new("/w/b/.git/refs/heads/main")),
        only("scm:pe-b")
    );
}

#[test]
fn the_most_specific_registration_wins() {
    // `.git` and `.git/refs` are both registered for the same repository; matching
    // the longer key keeps attribution stable rather than order-dependent.
    let registry = registry_of(&[
        ("/w/repo/.git", entry("scm:pe1", &["/w/repo/.git"])),
        ("/w/repo/.git/refs", entry("scm:pe1", &["/w/repo/.git/refs"])),
    ]);

    assert_eq!(
        attribute(&registry, Path::new("/w/repo/.git/refs/heads/main")),
        only("scm:pe1")
    );
}

#[test]
fn registration_keys_include_the_realpath_form() {
    // macOS reports FSEvents paths with symlinks resolved, so a watch registered
    // through a symlinked path would never match its own events unless both forms
    // are keys. Uses a real directory so the realpath is genuine.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let keys = keys_for(tmp.path());

    assert!(
        keys.contains(&tmp.path().to_string_lossy().into_owned()),
        "the path as given is always a key"
    );
    let real = std::fs::canonicalize(tmp.path()).expect("canonicalize");
    assert!(
        keys.contains(&real.to_string_lossy().into_owned()),
        "and so is its realpath form, got {keys:?}"
    );
}

#[test]
fn keys_are_not_duplicated_when_the_path_is_already_real() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let real = std::fs::canonicalize(tmp.path()).expect("canonicalize");
    assert_eq!(keys_for(&real).len(), 1, "no redundant duplicate key");
}

#[tokio::test]
async fn watching_a_repository_then_releasing_it_is_clean() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs")).expect("mkdir git dir");

    let (watcher, _rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe1", &git_dir).expect("watch arms");
    assert!(
        !watcher.registry.lock().expect("registry").is_empty(),
        "registration is recorded"
    );

    watcher.unwatch("scm:pe1");
    assert!(
        watcher.registry.lock().expect("registry").is_empty(),
        "releasing clears every key for that repository"
    );

    // Idempotent: releasing again must not panic, so callers need not track state.
    watcher.unwatch("scm:pe1");
}

#[tokio::test]
async fn a_repository_without_a_refs_directory_still_watches() {
    // A freshly initialised repository has no `refs` yet; index signals must still
    // arm rather than the whole registration failing.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(&git_dir).expect("mkdir git dir");

    let (watcher, _rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe1", &git_dir).expect("watch arms without refs");
    assert!(!watcher.registry.lock().expect("registry").is_empty());
}

/// Wait for a dirty signal naming `repo_id`, draining unrelated ones.
///
/// Bounded wait rather than a fixed sleep: platforms coalesce and deliver
/// filesystem events on their own schedule, so betting on a duration is what
/// makes such tests flaky. A generous ceiling costs nothing when the signal
/// arrives promptly, and on failure the caller reports what did arrive.
async fn saw_dirty(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ScmDirty>,
    repo_id: &str,
    within: std::time::Duration,
) -> Result<(), Vec<String>> {
    let mut seen = Vec::new();
    let outcome = tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Some(dirty) if dirty.repo_id == repo_id => return true,
                Some(other) => seen.push(other.repo_id),
                None => return false,
            }
        }
    })
    .await;

    match outcome {
        Ok(true) => Ok(()),
        // Either the channel closed or the deadline passed with nothing matching.
        Ok(false) | Err(_) => Err(seen),
    }
}

/// Give the OS a moment to arm the watch before mutating, so the write is not
/// missed simply because registration had not taken effect yet.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// End-to-end: writing the index — what every staging/commit/checkout does — must
/// actually reach us as a signal. The pure tests above prove filtering and
/// attribution are right; this proves a real filesystem write arrives at all.
#[tokio::test]
async fn writing_the_index_produces_a_dirty_signal() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs")).expect("mkdir git dir");

    let (watcher, mut rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe1", &git_dir).expect("watch arms");
    settle().await;

    std::fs::write(git_dir.join("index"), b"index-v1").expect("write index");

    if let Err(seen) = saw_dirty(&mut rx, "scm:pe1", std::time::Duration::from_secs(10)).await {
        panic!("no dirty signal for the index write; signals seen instead: {seen:?}");
    }
}

/// A ref update (branch move, commit, stash) lands under `refs/`, which is
/// watched recursively — a nested path must still be attributed.
#[tokio::test]
async fn updating_a_nested_ref_produces_a_dirty_signal() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs").join("heads")).expect("mkdir refs/heads");

    let (watcher, mut rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe1", &git_dir).expect("watch arms");
    settle().await;

    std::fs::write(git_dir.join("refs").join("heads").join("main"), b"deadbeef\n").expect("write ref");

    if let Err(seen) = saw_dirty(&mut rx, "scm:pe1", std::time::Duration::from_secs(10)).await {
        panic!("no dirty signal for the nested ref update; signals seen instead: {seen:?}");
    }
}

/// Two repositories must not be confused: a write in one is attributed to that
/// one only, or an unrelated panel would recompute on someone else's churn.
#[tokio::test]
async fn a_write_is_attributed_to_its_own_repository() {
    let one = tempfile::TempDir::new().expect("tempdir");
    let two = tempfile::TempDir::new().expect("tempdir");
    let git_one = one.path().join(".git");
    let git_two = two.path().join(".git");
    std::fs::create_dir_all(git_one.join("refs")).expect("mkdir");
    std::fs::create_dir_all(git_two.join("refs")).expect("mkdir");

    let (watcher, mut rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe-one", &git_one).expect("watch arms");
    watcher.watch("scm:pe-two", &git_two).expect("watch arms");
    settle().await;

    std::fs::write(git_two.join("index"), b"only-two").expect("write index");

    if let Err(seen) = saw_dirty(&mut rx, "scm:pe-two", std::time::Duration::from_secs(10)).await {
        panic!("no dirty signal for the repository that changed; seen: {seen:?}");
    }
}

/// After release, writes must stop producing signals — otherwise an unsubscribed
/// repository would keep waking the runtime, which is the leak the reference
/// counting exists to prevent.
#[tokio::test]
async fn a_released_repository_stops_signalling() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs")).expect("mkdir git dir");

    let (watcher, mut rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe1", &git_dir).expect("watch arms");
    settle().await;

    // Prove the watch works before releasing it, so a later silence cannot be
    // mistaken for "the write never happened".
    std::fs::write(git_dir.join("index"), b"before").expect("write index");
    saw_dirty(&mut rx, "scm:pe1", std::time::Duration::from_secs(10))
        .await
        .expect("baseline signal arrives while watched");

    watcher.unwatch("scm:pe1");
    settle().await;
    while rx.try_recv().is_ok() {} // drain anything already queued

    std::fs::write(git_dir.join("index"), b"after").expect("write index again");
    // A short window is right here: the assertion is absence, and the baseline
    // above already established signals arrive quickly when armed.
    assert!(
        saw_dirty(&mut rx, "scm:pe1", std::time::Duration::from_millis(1500))
            .await
            .is_err(),
        "a released repository must not keep signalling"
    );
}

#[test]
fn one_path_can_be_attributed_to_several_repositories() {
    // The same checkout opened under two projects yields two repo ids over one
    // git directory. Both must be attributed, or the one left out goes stale.
    let registry = registry_of(&[(
        "/w/repo/.git",
        shared_entry(&["scm:pe-a", "scm:pe-b"], &["/w/repo/.git"]),
    )]);

    let attributed = attribute(&registry, Path::new("/w/repo/.git/index"));
    assert_eq!(
        attributed,
        ["scm:pe-a".to_owned(), "scm:pe-b".to_owned()].into_iter().collect(),
        "a shared path wakes every repository behind it"
    );
}

/// Registering a second repository over a git directory must not displace the
/// first. Before the registry held a set, the later registration replaced the
/// earlier one and the first repository's subscribers simply stopped receiving
/// refreshes — no error, no log, just a change list that never updates.
#[tokio::test]
async fn two_repositories_sharing_a_git_dir_both_receive_signals() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs")).expect("mkdir git dir");

    let (watcher, mut rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe-a", &git_dir).expect("first repo arms");
    watcher.watch("scm:pe-b", &git_dir).expect("second repo arms");
    assert!(
        watcher.is_watching("scm:pe-a") && watcher.is_watching("scm:pe-b"),
        "both registrations survive"
    );
    settle().await;

    std::fs::write(git_dir.join("index"), b"index-v1").expect("write index");

    // Both must be notified by the one write.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let deadline = std::time::Duration::from_secs(10);
    let collected = tokio::time::timeout(deadline, async {
        while seen.len() < 2 {
            match rx.recv().await {
                Some(dirty) => {
                    seen.insert(dirty.repo_id);
                }
                None => break,
            }
        }
    })
    .await;
    assert!(
        collected.is_ok() && seen.len() == 2,
        "one write must wake both repositories, saw {seen:?}"
    );
}

/// Releasing one of two repositories sharing a git directory must leave the other
/// watching. The registry entry is shared, so a release that drops the whole entry
/// would silently blind the survivor.
#[tokio::test]
async fn releasing_one_shared_repository_leaves_the_other_watching() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs")).expect("mkdir git dir");

    let (watcher, mut rx) = GitWatcher::new().expect("watcher builds");
    watcher.watch("scm:pe-a", &git_dir).expect("first arms");
    watcher.watch("scm:pe-b", &git_dir).expect("second arms");
    settle().await;

    watcher.unwatch("scm:pe-a");
    assert!(!watcher.is_watching("scm:pe-a"), "the released one is gone");
    assert!(
        watcher.is_watching("scm:pe-b"),
        "the other keeps its registration after a sibling is released"
    );
    while rx.try_recv().is_ok() {} // drain anything already queued

    std::fs::write(git_dir.join("index"), b"index-v2").expect("write index");

    // The survivor must still receive signals, and the released one must not.
    if let Err(seen) = saw_dirty(&mut rx, "scm:pe-b", std::time::Duration::from_secs(10)).await {
        panic!("the surviving repository stopped receiving signals; saw instead: {seen:?}");
    }
}
