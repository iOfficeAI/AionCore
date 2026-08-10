//! Orchestration tests: identity assembly, sequence allocation, and the
//! subscription lifecycle that owns the watch.
//!
//! These use real temporary repositories (built with git2, so no `git` binary is
//! required and the suite runs the same on every platform).

use git2::Repository;
use tempfile::TempDir;

use super::*;

fn init_repo(dir: &std::path::Path) -> Repository {
    let repo = Repository::init(dir).expect("init repo");
    let mut cfg = repo.config().expect("config");
    cfg.set_str("user.name", "scm test").expect("set name");
    cfg.set_str("user.email", "scm@test.local").expect("set email");
    repo
}

fn write(dir: &std::path::Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write file");
}

fn commit_all(repo: &Repository, message: &str) {
    let mut index = repo.index().expect("index");
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add all");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = repo.signature().expect("signature");
    let parents = match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(parent) => vec![parent],
        None => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .expect("commit");
}

fn root_of(tmp: &TempDir, pe_id: &str) -> ResolvedRoot {
    ResolvedRoot {
        pe_id: pe_id.to_owned(),
        absolute_path: tmp.path().to_string_lossy().into_owned(),
        label: "fixture".to_owned(),
        pe_name: None,
        discover_children: false,
    }
}

/// A repository with one committed file and one uncommitted edit.
fn fixture(tmp: &TempDir) -> Repository {
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "base\n");
    commit_all(&repo, "base");
    write(tmp.path(), "a.txt", "edited\n");
    repo
}

#[tokio::test]
async fn discovery_lists_repositories_and_skips_plain_directories() {
    let repo_dir = TempDir::new().expect("tempdir");
    let _repo = fixture(&repo_dir);
    let plain_dir = TempDir::new().expect("tempdir");

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime
        .discover(
            "proj-test",
            &[root_of(&repo_dir, "pe-repo"), root_of(&plain_dir, "pe-plain")],
        )
        .await;

    assert_eq!(found.len(), 1, "only the repository is listed, got {found:?}");
    assert_eq!(found[0].root.pe_id, "pe-repo");
}

#[tokio::test]
async fn a_root_that_is_not_a_repository_does_not_hide_the_others() {
    // One unusable root must not cost a project its other repositories.
    let plain_dir = TempDir::new().expect("tempdir");
    let repo_dir = TempDir::new().expect("tempdir");
    let _repo = fixture(&repo_dir);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime
        .discover(
            "proj-test",
            &[root_of(&plain_dir, "pe-plain"), root_of(&repo_dir, "pe-repo")],
        )
        .await;

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].root.pe_id, "pe-repo");
}

#[tokio::test]
async fn the_orchestration_layer_assembles_pe_identity() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe-42")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };

    let status = runtime.refresh(&repo).await.expect("refresh ok");
    assert!(!status.resources.is_empty(), "the edit is reported");
    for resource in &status.resources {
        // The provider only knows repository-relative paths; identity is this
        // layer's to assemble from the resolved root.
        assert_eq!(resource.file.pe_id, "pe-42", "pe identity is filled in here");
        assert_eq!(
            resource.file.relative_path, resource.repo_relative_path,
            "and the path mirrors the repository-relative one"
        );
    }
}

#[tokio::test]
async fn sequence_numbers_increase_with_every_recompute() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };

    let first = runtime.refresh(&repo).await.expect("refresh ok").seq;
    let second = runtime.refresh(&repo).await.expect("refresh ok").seq;
    let third = runtime.refresh(&repo).await.expect("refresh ok").seq;

    assert!(first >= 1, "a published frame is never sequence zero, got {first}");
    assert!(
        second > first && third > second,
        "sequences must strictly increase: {first}, {second}, {third}"
    );
}

#[tokio::test]
async fn concurrent_refreshes_hand_out_distinct_ordered_sequences() {
    // Two refresh sources (an action finishing, a debounced signal) can race. If
    // the sequence were allocated outside the critical section they could collide
    // or invert, and a client would then drop a newer frame as "older".
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };
    let runtime = std::sync::Arc::new(runtime);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let runtime = std::sync::Arc::clone(&runtime);
        let repo = repo.clone();
        handles.push(tokio::spawn(async move { runtime.refresh(&repo).await.map(|s| s.seq) }));
    }

    let mut seqs = Vec::new();
    for handle in handles {
        seqs.push(handle.await.expect("task joins").expect("refresh ok"));
    }
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 8, "every recompute got its own sequence, got {seqs:?}");
    assert_eq!(seqs, (1..=8).collect::<Vec<u64>>(), "and they form one ordered run");
}

#[tokio::test]
async fn an_action_publishes_a_newer_frame_than_before_it() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };

    let before = runtime.refresh(&repo).await.expect("refresh ok").seq;
    let (_, status) = runtime
        .act(&repo, || async { Ok::<(), ScmError>(()) })
        .await
        .expect("action ok");
    let after = status.seq;
    assert!(after > before, "the post-action frame supersedes the earlier one");
}

#[tokio::test]
async fn a_failing_action_does_not_publish_a_frame() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };
    let before = runtime.refresh(&repo).await.expect("refresh ok").seq;

    let failed = runtime
        .act(&repo, || async {
            Err::<(), ScmError>(ScmError::OperationFailed {
                context: "test",
                message: "deliberate".to_owned(),
            })
        })
        .await;
    assert!(failed.is_err(), "the failure propagates");

    // A failed action must not consume a sequence or replace the frame, otherwise
    // clients would see a "new" status that reflects nothing.
    let after = runtime.refresh(&repo).await.expect("refresh ok").seq;
    assert_eq!(after, before + 1, "only the explicit recompute advanced it");
}

#[tokio::test]
async fn subscribing_returns_a_first_frame_and_records_the_subscriber() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };

    let status = runtime.subscribe("conn-1", &repo).await.expect("subscribe ok");
    assert!(status.seq >= 1, "the first frame carries a sequence");
    assert_eq!(runtime.subscribers_of(&repo.repo_id).await, vec!["conn-1".to_owned()]);
}

#[tokio::test]
async fn subscribing_twice_from_one_connection_counts_once() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };

    runtime.subscribe("conn-1", &repo).await.expect("subscribe ok");
    runtime.subscribe("conn-1", &repo).await.expect("re-subscribe ok");
    assert_eq!(
        runtime.subscribers_of(&repo.repo_id).await.len(),
        1,
        "a repeated subscribe must not double-count, or the watch would never release"
    );
}

#[tokio::test]
async fn the_watch_is_released_only_when_the_last_subscriber_leaves() {
    // Reference counted: several connections may observe one repository, and the
    // first one leaving must not blind the others.
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };

    runtime.subscribe("conn-1", &repo).await.expect("subscribe ok");
    runtime.subscribe("conn-2", &repo).await.expect("subscribe ok");
    assert!(runtime.is_watching(&repo.repo_id), "the watch is armed while observed");

    runtime.unsubscribe("conn-1", &repo).await;
    assert_eq!(
        runtime.subscribers_of(&repo.repo_id).await,
        vec!["conn-2".to_owned()],
        "the remaining connection keeps its subscription"
    );
    assert!(
        runtime.is_watching(&repo.repo_id),
        "and the watch stays armed — releasing it here would blind the other connection"
    );

    runtime.unsubscribe("conn-2", &repo).await;
    assert!(
        runtime.subscribers_of(&repo.repo_id).await.is_empty(),
        "now it is released"
    );
    assert!(
        !runtime.is_watching(&repo.repo_id),
        "and the watch goes with the last subscriber, so nothing leaks"
    );
}

#[tokio::test]
async fn a_closed_connection_releases_everything_it_held() {
    // Without this a reconnect churn leaks one armed watch per dropped connection:
    // nothing else ever tells the runtime that session is gone.
    let repo_a = TempDir::new().expect("tempdir");
    let _a = fixture(&repo_a);
    let repo_b = TempDir::new().expect("tempdir");
    let _b = fixture(&repo_b);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime
        .discover("proj-test", &[root_of(&repo_a, "pe-a"), root_of(&repo_b, "pe-b")])
        .await;
    assert_eq!(found.len(), 2);
    let first = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };
    let second = RepoRef {
        repo_id: found[1].repo_id.clone(),
    };

    runtime.subscribe("conn-1", &first).await.expect("subscribe ok");
    runtime.subscribe("conn-1", &second).await.expect("subscribe ok");
    runtime.subscribe("conn-2", &second).await.expect("subscribe ok");

    runtime.drop_session("conn-1").await;
    assert!(
        runtime.subscribers_of(&first.repo_id).await.is_empty(),
        "the repository only that connection watched is released"
    );
    assert!(
        !runtime.is_watching(&first.repo_id),
        "its watch is released too — otherwise a reconnect churn leaks one per connection"
    );
    assert!(
        runtime.is_watching(&second.repo_id),
        "the shared repository keeps its watch for the surviving connection"
    );
    assert_eq!(
        runtime.subscribers_of(&second.repo_id).await,
        vec!["conn-2".to_owned()],
        "the shared repository stays subscribed for the other connection"
    );
}

#[tokio::test]
async fn unsubscribing_an_unknown_repository_is_harmless() {
    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    // Arrives after a release, or for a repository that never existed — must not
    // panic, since a client can always send a stale id.
    runtime
        .unsubscribe(
            "conn-1",
            &RepoRef {
                repo_id: "scm:never".to_owned(),
            },
        )
        .await;
    runtime.drop_session("conn-1").await;
}

#[tokio::test]
async fn operating_on_an_unknown_repository_is_rejected() {
    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let unknown = RepoRef {
        repo_id: "scm:never-discovered".to_owned(),
    };

    assert!(matches!(
        runtime.refresh(&unknown).await,
        Err(ScmError::UnknownRepository { .. })
    ));
    assert!(matches!(
        runtime.subscribe("conn-1", &unknown).await,
        Err(ScmError::UnknownRepository { .. })
    ));
}

#[tokio::test]
async fn re_discovery_keeps_subscribers_and_sequence() {
    // Discovery runs again whenever a project lists its repositories; that must not
    // silently drop live subscriptions or rewind the sequence a client is tracking.
    let tmp = TempDir::new().expect("tempdir");
    let _repo = fixture(&tmp);

    let (runtime, _dirty) = ScmRuntime::new().expect("runtime builds");
    let found = runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;
    let repo = RepoRef {
        repo_id: found[0].repo_id.clone(),
    };
    let seq_before = runtime.subscribe("conn-1", &repo).await.expect("subscribe ok").seq;

    runtime.discover("proj-test", &[root_of(&tmp, "pe1")]).await;

    assert_eq!(
        runtime.subscribers_of(&repo.repo_id).await,
        vec!["conn-1".to_owned()],
        "the subscription survives re-discovery"
    );
    let seq_after = runtime.refresh(&repo).await.expect("refresh ok").seq;
    assert!(
        seq_after > seq_before,
        "and the sequence continues rather than resetting"
    );
}
