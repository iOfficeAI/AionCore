use std::path::Path;
use std::sync::Arc;

use tempfile::{TempDir, tempdir};

use crate::canonical::{self, Canonical};
use crate::runtime::LocalFsRuntime;
use crate::runtime::fs_runtime::FsRuntimeRegistry;
use crate::runtime::subscription::{GRACE_TTL_MS, Subscriber};
use crate::runtime::tree_model::{Change, Hint, TreeModel};
use crate::runtime::watcher::RawEvent;

use super::{Command, Shard, ShardOutput, raw_to_command};

fn canon(path: &Path) -> Canonical {
    let uri = canonical::to_file_uri(path).expect("to_file_uri");
    canonical::canonicalize(&uri).expect("canonicalize")
}

fn real_shard(budget: usize) -> (Shard, TempDir) {
    let (runtime, _rx) = LocalFsRuntime::new().unwrap();
    let mut registry = FsRuntimeRegistry::new();
    registry.register("file", Arc::new(runtime));
    (Shard::new(TreeModel::new(registry), budget), tempdir().unwrap())
}

fn sub(session: &str) -> Subscriber {
    Subscriber {
        session: session.to_owned(),
        pe_id: "pe1".to_owned(),
        rel: String::new(),
    }
}

#[tokio::test]
async fn subscribe_mounts_and_returns_snapshot_to_subscriber() {
    let (mut shard, dir) = real_shard(100);
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
    let c = canon(dir.path()).as_str().to_owned();

    let out = shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();

    match out.as_slice() {
        [ShardOutput::Snapshot { subscribers, snapshot }] => {
            assert_eq!(subscribers, &vec![sub("s1")]);
            assert_eq!(
                snapshot.entries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
                vec!["a.txt"]
            );
        }
        other => panic!("expected one Snapshot, got {other:?}"),
    }
    assert!(shard.is_watched(&c));
}

#[tokio::test]
async fn already_live_subscriber_gets_snapshot_to_self_only() {
    let (mut shard, dir) = real_shard(100);
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
    let c = canon(dir.path()).as_str().to_owned();

    // First subscriber mounts.
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();

    // A second, later subscriber on the same live canonical must immediately get
    // the current listing — delivered only to itself, never re-sent to s1.
    let out = shard
        .handle(Command::Subscribe {
            sub: sub("s2"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();

    match out.as_slice() {
        [ShardOutput::Snapshot { subscribers, snapshot }] => {
            assert_eq!(subscribers, &vec![sub("s2")], "snapshot goes to s2 alone");
            assert_eq!(
                snapshot.entries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
                vec!["a.txt"]
            );
        }
        other => panic!("expected one Snapshot to s2, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_fans_delta_to_current_subscribers() {
    let (mut shard, dir) = real_shard(100);
    let c = canon(dir.path()).as_str().to_owned();
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();

    std::fs::write(dir.path().join("new.txt"), b"x").unwrap();
    let out = shard
        .handle(Command::Apply {
            canonical: c.clone(),
            hint: Hint::ChildNames(vec!["new.txt".to_owned()]),
        })
        .await
        .unwrap();

    match out.as_slice() {
        [ShardOutput::Delta { subscribers, delta }] => {
            assert_eq!(subscribers, &vec![sub("s1")]);
            assert_eq!(
                delta.changes,
                vec![Change::Added {
                    name: "new.txt".to_owned(),
                    kind: crate::runtime::provider::Kind::File
                }]
            );
        }
        other => panic!("expected one Delta, got {other:?}"),
    }
}

#[tokio::test]
async fn overflow_rescans_and_pushes_full_snapshot() {
    let (mut shard, dir) = real_shard(100);
    let c = canon(dir.path()).as_str().to_owned();
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();

    // Bulk change while "kernel dropped events" → overflow forces a full rescan.
    std::fs::write(dir.path().join("x.txt"), b"x").unwrap();
    std::fs::write(dir.path().join("y.txt"), b"y").unwrap();

    let out = shard.handle(Command::Overflow { canonical: c.clone() }).await.unwrap();
    match out.as_slice() {
        [ShardOutput::Snapshot { subscribers, snapshot }] => {
            assert_eq!(subscribers, &vec![sub("s1")]);
            let mut names: Vec<&str> = snapshot.entries.iter().map(|(n, _)| n.as_str()).collect();
            names.sort();
            assert_eq!(names, vec!["x.txt", "y.txt"]);
        }
        other => panic!("expected one Snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn guard_apply_on_unmounted_is_silent() {
    let (mut shard, dir) = real_shard(100);
    let c = canon(dir.path()).as_str().to_owned();
    // Never subscribed → never mounted. An in-flight event must produce nothing.
    let out = shard
        .handle(Command::Apply {
            canonical: c,
            hint: Hint::All,
        })
        .await
        .unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn guard_overflow_on_unmounted_is_silent() {
    let (mut shard, dir) = real_shard(100);
    let c = canon(dir.path()).as_str().to_owned();
    // Never subscribed → never mounted. An in-flight overflow (kernel drop after
    // unwatch) must be dropped silently — symmetric to the Apply guard.
    let out = shard.handle(Command::Overflow { canonical: c }).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn guard_mount_then_disconnect_graces_then_reap_unmounts() {
    let (mut shard, dir) = real_shard(100);
    let c = canon(dir.path()).as_str().to_owned();
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();
    assert!(shard.is_watched(&c));

    // Disconnect before anyone else subscribed → node kept warm (grace).
    shard
        .handle(Command::DropSession {
            session: "s1".to_owned(),
            now: 0,
        })
        .await
        .unwrap();
    assert!(shard.is_watched(&c), "still warm during grace");

    // TTL elapses → reap unmounts + unwatches.
    shard.handle(Command::ReapTick { now: GRACE_TTL_MS }).await.unwrap();
    assert!(!shard.is_watched(&c), "unmounted after grace TTL");

    // Post-unmount in-flight event is dropped (no panic, no output).
    let out = shard
        .handle(Command::Apply {
            canonical: c,
            hint: Hint::All,
        })
        .await
        .unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn grace_rescue_keeps_node_and_cancels_reap() {
    let (mut shard, dir) = real_shard(100);
    let c = canon(dir.path()).as_str().to_owned();
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();
    shard
        .handle(Command::Unsubscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();
    // Rescue within TTL: the rescued subscriber must also immediately receive the
    // current listing (RescuedFromGrace serves the kept-warm node's snapshot).
    let out = shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 100,
        })
        .await
        .unwrap();
    match out.as_slice() {
        [ShardOutput::Snapshot { subscribers, .. }] => {
            assert_eq!(subscribers, &vec![sub("s1")], "rescued subscriber gets a snapshot");
        }
        other => panic!("expected one Snapshot on rescue, got {other:?}"),
    }

    // Well past original TTL: still watched (reap finds nothing to evict).
    shard
        .handle(Command::ReapTick { now: GRACE_TTL_MS * 10 })
        .await
        .unwrap();
    assert!(shard.is_watched(&c));
}

#[tokio::test]
async fn warm_lru_evicts_oldest_over_budget_on_reap() {
    let (mut shard, dir_a) = real_shard(1);
    let dir_b = tempdir().unwrap();
    let ca = canon(dir_a.path()).as_str().to_owned();
    let cb = canon(dir_b.path()).as_str().to_owned();

    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: ca.clone(),
            now: 0,
        })
        .await
        .unwrap();
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: cb.clone(),
            now: 0,
        })
        .await
        .unwrap();
    shard
        .handle(Command::Unsubscribe {
            sub: sub("s1"),
            canonical: ca.clone(),
            now: 10,
        })
        .await
        .unwrap();
    shard
        .handle(Command::Unsubscribe {
            sub: sub("s1"),
            canonical: cb.clone(),
            now: 20,
        })
        .await
        .unwrap();

    // Budget 1, both warm → reap enforces budget, evicts oldest (ca).
    shard.handle(Command::ReapTick { now: 0 }).await.unwrap();
    assert!(!shard.is_watched(&ca), "oldest warm node evicted");
    assert!(shard.is_watched(&cb), "newest warm node kept");
}

#[test]
fn raw_changed_maps_to_apply_child_names() {
    let event = RawEvent::Changed {
        canonical: "file:///work/app".to_owned(),
        paths: vec!["/work/app/a.txt".to_owned(), "/work/app/b.txt".to_owned()],
    };
    assert_eq!(
        raw_to_command(event),
        Command::Apply {
            canonical: "file:///work/app".to_owned(),
            hint: Hint::ChildNames(vec!["a.txt".to_owned(), "b.txt".to_owned()]),
        }
    );
}

#[test]
fn raw_overflow_maps_to_overflow_command() {
    let event = RawEvent::Overflow {
        canonical: "file:///work/app".to_owned(),
    };
    assert_eq!(
        raw_to_command(event),
        Command::Overflow {
            canonical: "file:///work/app".to_owned()
        }
    );
}

// ── Debounce coalescing (pure, clock-free) ────────────────────────────────

#[test]
fn debouncer_coalesces_changed_by_canonical() {
    let mut d = super::Debouncer::new();
    d.push(RawEvent::Changed {
        canonical: "file:///c".to_owned(),
        paths: vec!["/c/a.txt".to_owned()],
    });
    d.push(RawEvent::Changed {
        canonical: "file:///c".to_owned(),
        paths: vec!["/c/b.txt".to_owned(), "/c/a.txt".to_owned()],
    });

    assert_eq!(
        d.drain(),
        vec![Command::Apply {
            canonical: "file:///c".to_owned(),
            hint: Hint::ChildNames(vec!["a.txt".to_owned(), "b.txt".to_owned()]),
        }]
    );
    assert!(d.is_empty());
}

#[test]
fn debouncer_overflow_supersedes_changed() {
    let mut d = super::Debouncer::new();
    d.push(RawEvent::Changed {
        canonical: "file:///c".to_owned(),
        paths: vec!["/c/a.txt".to_owned()],
    });
    d.push(RawEvent::Overflow {
        canonical: "file:///c".to_owned(),
    });
    d.push(RawEvent::Changed {
        canonical: "file:///c".to_owned(),
        paths: vec!["/c/b.txt".to_owned()],
    });

    assert_eq!(
        d.drain(),
        vec![Command::Overflow {
            canonical: "file:///c".to_owned()
        }]
    );
}

// ── Real end-to-end wiring: watcher → map → shard → output ─────────────────

#[tokio::test]
async fn end_to_end_watch_change_produces_delta() {
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedReceiver;

    // Build a shard sharing the SAME runtime whose watcher feeds `rx`.
    let (runtime, mut rx): (_, UnboundedReceiver<RawEvent>) = LocalFsRuntime::new().unwrap();
    let mut registry = FsRuntimeRegistry::new();
    registry.register("file", Arc::new(runtime));
    let mut shard = Shard::new(TreeModel::new(registry), 100);

    let dir = tempdir().unwrap();
    let c = canon(dir.path()).as_str().to_owned();

    // Subscribe arms the watch (via mount) and returns the baseline.
    shard
        .handle(Command::Subscribe {
            sub: sub("s1"),
            canonical: c.clone(),
            now: 0,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Change the directory; drive one raw event through the real pipeline.
    std::fs::write(dir.path().join("live.ts"), b"x").unwrap();

    let delta = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = rx.recv().await.expect("watcher event");
            let cmd = raw_to_command(ev);
            let out = shard.handle(cmd).await.unwrap();
            if let Some(ShardOutput::Delta { delta, .. }) = out.into_iter().next()
                && delta
                    .changes
                    .iter()
                    .any(|ch| matches!(ch, Change::Added { name, .. } if name == "live.ts"))
            {
                return delta;
            }
        }
    })
    .await
    .expect("a delta adding live.ts");

    assert_eq!(delta.canonical, c);
}
