use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::ProjectService;
use crate::canonical::to_file_uri;
use crate::monitor::{FsInbound, FsMonitorActor, FsWirePush};
use crate::runtime::{EntryFact, Kind, RawEvent, ShardOutput, Snapshot, Subscriber};

// ── recording push port ────────────────────────────────────────────────────

/// An [`FsWirePush`] that records every `(session, frame)` for assertions.
#[derive(Clone, Default)]
struct RecordingPush {
    sent: Arc<Mutex<Vec<(String, Value)>>>,
}

impl FsWirePush for RecordingPush {
    fn push(&self, session: &str, frame: Value) {
        self.sent.lock().unwrap().push((session.to_owned(), frame));
    }
}

impl RecordingPush {
    fn frames(&self) -> Vec<(String, Value)> {
        self.sent.lock().unwrap().clone()
    }
    /// Last frame delivered to `session`.
    fn last_for(&self, session: &str) -> Option<Value> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(s, _)| s == session)
            .map(|(_, f)| f.clone())
    }
}

// ── harness ─────────────────────────────────────────────────────────────────

/// Build an actor over a real in-memory-DB `ProjectService` + a fresh tempdir
/// registered as a standard project. Returns the pe_id of that workspace root.
async fn setup() -> (
    FsMonitorActor,
    UnboundedReceiver<RawEvent>,
    RecordingPush,
    String,
    TempDir,
    Database,
) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));

    let dir = tempfile::tempdir().unwrap();
    let created = service
        .create_standard("system_default_user", to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    let pe_id = created.project_explorer.pe_id;

    let push = RecordingPush::default();
    let (actor, raw_rx) = FsMonitorActor::new(service, Arc::new(push.clone()), 4096).unwrap();
    (actor, raw_rx, push, pe_id, dir, db)
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn dir_ref(pe_id: &str, rel: &str) -> Value {
    json!({"pe_id":pe_id,"relative_path":rel})
}

/// The folded canonical the actor keys a directory on (matches what Subscribe
/// derives from `resolve_reference` → `canonicalize`). Lets a test inject a
/// synthetic watcher event for a mounted node.
fn canon(path: &std::path::Path) -> String {
    let uri = to_file_uri(path).unwrap();
    crate::canonical::canonicalize(&uri).unwrap().as_str().to_owned()
}

// ══ dispatch-level tests (deterministic, no timers, cross-platform) ══════════

#[tokio::test]
async fn initialize_negotiates_version() {
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(0, "initialize", json!({"protocol_version": 1})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["id"], 0);
    assert_eq!(reply["result"]["protocol_version"], 1);
}

#[tokio::test]
async fn initialize_rejects_unsupported_version() {
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(0, "initialize", json!({"protocol_version": 0})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32010);
    assert_eq!(reply["error"]["message"], "protocol_version_unsupported");
}

#[tokio::test]
async fn subscribe_root_returns_baseline_snapshot() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("README.md"), b"x").unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(1, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["id"], 1);
    let snaps = reply["result"]["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["target"], dir_ref(&pe, ""));
    let names: Vec<&str> = snaps[0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"src"));
    assert!(names.contains(&"README.md"));
    // canonical / absolute path must never leak.
    assert!(
        reply.to_string().find("file://").is_none(),
        "no canonical uri on the wire: {reply}"
    );
}

#[tokio::test]
async fn subscribe_multiple_targets_returns_snapshot_per_target() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("main.ts"), b"x").unwrap();

    // Array subscribe (root + a child dir) → one snapshot per target, in order.
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                1,
                "fs/subscribe",
                json!({"targets":[dir_ref(&pe, ""), dir_ref(&pe, "src")]}),
            ),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    let snaps = reply["result"]["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0]["target"], dir_ref(&pe, ""));
    assert_eq!(snaps[1]["target"], dir_ref(&pe, "src"));
    let src_names: Vec<&str> = snaps[1]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(src_names, vec!["main.ts"]);
}

#[tokio::test]
async fn subscribe_unknown_pe_is_out_of_scope() {
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(2, "fs/subscribe", json!({"targets":[dir_ref("pe-nope", "")]})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32000);
    assert_eq!(reply["error"]["message"], "out_of_scope");
    assert_eq!(reply["error"]["data"]["pe_id"], "pe-nope");
}

#[tokio::test]
async fn subscribe_parent_escape_is_invalid_relative_path() {
    let (mut actor, _rx, push, pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(3, "fs/subscribe", json!({"targets":[dir_ref(&pe, "../escape")]})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32004);
    assert_eq!(reply["error"]["message"], "invalid_relative_path");
}

#[tokio::test]
async fn read_existing_file_returns_utf8() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(4, "fs/read", json!({"file":dir_ref(&pe, "a.txt")})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["result"]["content"], "hello");
    assert_eq!(reply["result"]["encoding"], "utf-8");
}

#[tokio::test]
async fn read_missing_file_is_resource_not_found() {
    let (mut actor, _rx, push, pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(5, "fs/read", json!({"file":dir_ref(&pe, "missing.txt")})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32002);
    assert_eq!(reply["error"]["message"], "resource_not_found");
    assert_eq!(reply["error"]["data"]["relative_path"], "missing.txt");
}

#[tokio::test]
async fn read_non_utf8_falls_back_to_base64() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("bin"), [0xff, 0xfe, 0x00]).unwrap();
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(6, "fs/read", json!({"file":dir_ref(&pe, "bin")})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["result"]["encoding"], "base64");
    assert!(!reply["result"]["content"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let (mut actor, _rx, push, pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                7,
                "fs/write",
                json!({"file":dir_ref(&pe, "new.txt"),"content":"written"}),
            ),
        )
        .await;
    assert!(push.last_for("1").unwrap()["result"].is_object());

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(8, "fs/read", json!({"file":dir_ref(&pe, "new.txt")})),
        )
        .await;
    assert_eq!(push.last_for("1").unwrap()["result"]["content"], "written");
}

#[tokio::test]
async fn write_base64_decodes_to_bytes() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    // base64("hi") = "aGk="
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                9,
                "fs/write",
                json!({"file":dir_ref(&pe, "b.bin"),"content":"aGk=","encoding":"base64"}),
            ),
        )
        .await;
    assert!(push.last_for("1").unwrap()["result"].is_object());
    assert_eq!(std::fs::read(dir.path().join("b.bin")).unwrap(), b"hi");
}

#[tokio::test]
async fn mkdir_then_remove_roundtrip() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(10, "fs/mkdir", json!({"dir":dir_ref(&pe, "sub")})),
        )
        .await;
    assert!(dir.path().join("sub").is_dir());

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(11, "fs/remove", json!({"target":dir_ref(&pe, "sub")})),
        )
        .await;
    assert!(push.last_for("1").unwrap()["result"].is_object());
    assert!(!dir.path().join("sub").exists());
}

#[tokio::test]
async fn rename_moves_entry() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("old.txt"), b"x").unwrap();
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                12,
                "fs/rename",
                json!({"from":dir_ref(&pe, "old.txt"),"to":dir_ref(&pe, "renamed.txt")}),
            ),
        )
        .await;
    assert!(push.last_for("1").unwrap()["result"].is_object());
    assert!(!dir.path().join("old.txt").exists());
    assert!(dir.path().join("renamed.txt").exists());
}

#[tokio::test]
async fn mkdir_existing_dir_is_provider_unavailable() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    // mkdir over an existing dir → AlreadyExists → provider_unavailable (-32006).
    // Platform-independent trigger of the command→FsError→code wiring.
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(30, "fs/mkdir", json!({"dir":dir_ref(&pe, "sub")})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32006);
    assert_eq!(reply["error"]["message"], "provider_unavailable");
    assert_eq!(reply["error"]["data"]["relative_path"], "sub");
}

#[tokio::test]
async fn initialize_bad_params_is_invalid_params() {
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(31, "initialize", json!({"wrong": "shape"})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32602);
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame("1", "system_default_user", request(13, "fs/teleport", json!({})))
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32601);
}

#[tokio::test]
async fn malformed_frame_is_invalid_request() {
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    // No `method` field → not a valid JSON-RPC request.
    actor
        .dispatch_frame("1", "system_default_user", json!({"jsonrpc":"2.0","id":1}))
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32600);
}

#[tokio::test]
async fn unsubscribe_is_notification_no_reply() {
    let (mut actor, _rx, push, pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(0, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
        )
        .await;
    let before = push.frames().len();
    // notification (the id is ignored by unsubscribe; it emits no response)
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            json!({"jsonrpc":"2.0","method":"fs/unsubscribe","params":{"targets":[dir_ref(&pe, "")]}}),
        )
        .await;
    assert_eq!(push.frames().len(), before, "unsubscribe must not reply");
}

/// realpath containment: a symlink escaping the folder root is rejected before
/// IO. Unix-only — creating a symlink on Windows needs elevated privilege; the
/// `realpath_within` logic itself is platform-agnostic (walks the deepest
/// existing ancestor), exercised on unix here and noted in the test report.
#[cfg(unix)]
#[tokio::test]
async fn command_symlink_escape_is_resource_outside_folder() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"top secret").unwrap();
    // A symlink inside the root pointing at the outside dir.
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(20, "fs/read", json!({"file":dir_ref(&pe, "link/secret.txt")})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32003);
    assert_eq!(reply["error"]["message"], "resource_outside_folder");
}

/// The overflow rescan path emits `ShardOutput::Snapshot` fanned out (not placed
/// in a reply, unlike subscribe). Drive `fan_out` directly with a synthetic
/// snapshot to two subscribers on different sessions and assert each receives an
/// `fs/snapshot` keyed to *its own* pe-relative target (scoped translation).
#[tokio::test]
async fn fan_out_snapshot_is_scoped_and_pe_keyed_per_subscriber() {
    let (actor, _rx, push, _pe, _dir, _db) = setup().await;
    let snapshot = Snapshot {
        canonical: "file:///backend/only".to_owned(),
        entries: vec![(
            "a.txt".to_owned(),
            EntryFact {
                kind: Kind::File,
                inode: 1,
                symlink_target: None,
            },
        )],
    };
    let outputs = vec![ShardOutput::Snapshot {
        subscribers: vec![
            Subscriber {
                session: "1".to_owned(),
                pe_id: "pe1".to_owned(),
                rel: "src".to_owned(),
            },
            Subscriber {
                session: "2".to_owned(),
                pe_id: "pe9".to_owned(),
                rel: String::new(),
            },
        ],
        snapshot,
    }];

    actor.fan_out(outputs);

    let f1 = push.last_for("1").unwrap();
    assert_eq!(f1["method"], "fs/snapshot");
    assert_eq!(f1["params"]["target"], json!({"pe_id":"pe1","relative_path":"src"}));
    assert_eq!(f1["params"]["entries"][0]["name"], "a.txt");
    // Same canonical fact, but session 2 sees its own pe-relative identity.
    let f2 = push.last_for("2").unwrap();
    assert_eq!(f2["params"]["target"], json!({"pe_id":"pe9","relative_path":""}));
    // Backend canonical never crosses the wire.
    assert!(!f1.to_string().contains("backend/only"));
}

// ══ event-loop tests (timed, real watcher) ═══════════════════════════════════

/// Poll `pred` against recorded frames until it holds or the deadline elapses.
async fn wait_until(push: &RecordingPush, within: Duration, pred: impl Fn(&[(String, Value)]) -> bool) -> bool {
    tokio::time::timeout(within, async {
        loop {
            if pred(&push.frames()) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false)
}

fn has_delta_adding(frames: &[(String, Value)], session: &str, name: &str) -> bool {
    frames.iter().any(|(s, f)| {
        s == session
            && f["method"] == "fs/delta"
            && f["params"]["changes"]
                .as_array()
                .map(|cs| cs.iter().any(|c| c["op"] == "added" && c["name"] == name))
                .unwrap_or(false)
    })
}

#[tokio::test]
async fn live_change_fans_delta_to_subscriber_only() {
    let (actor, raw_rx, push, pe, dir, _db) = setup().await;
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    // Session 1 subscribes the root; session 2 stays silent (scoped-push check).
    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(1, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
    })
    .unwrap();
    // Let subscribe mount + arm the watch.
    tokio::time::sleep(Duration::from_millis(250)).await;

    std::fs::write(dir.path().join("live.ts"), b"x").unwrap();

    assert!(
        wait_until(&push, Duration::from_secs(5), |f| has_delta_adding(f, "1", "live.ts")).await,
        "subscriber 1 must receive an fs/delta adding live.ts"
    );
    // Scoped push: session 2 (never subscribed) must have received nothing.
    assert!(
        !push.frames().iter().any(|(s, _)| s == "2"),
        "non-subscriber must receive no push"
    );

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn overflow_fans_full_snapshot_through_event_loop() {
    // Drive a real event loop, but feed the raw-event channel ourselves (ignore
    // the watcher's) so we can inject a synthetic kernel overflow deterministically.
    let (actor, _watcher_rx, push, pe, dir, _db) = setup().await;
    let (tx, rx) = unbounded_channel();
    let (raw_tx, raw_rx) = unbounded_channel::<RawEvent>();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(1, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Files a rescan (apply All) will pick up.
    std::fs::write(dir.path().join("x.ts"), b"x").unwrap();
    std::fs::write(dir.path().join("y.ts"), b"y").unwrap();

    // Inject a kernel overflow for the subscribed root → rescan → full snapshot.
    raw_tx
        .send(RawEvent::Overflow {
            canonical: canon(dir.path()),
        })
        .unwrap();

    let got_snapshot = wait_until(&push, Duration::from_secs(5), |frames| {
        frames.iter().any(|(s, m)| {
            s == "1"
                && m["method"] == "fs/snapshot"
                && m["params"]["entries"]
                    .as_array()
                    .map(|es| es.iter().any(|e| e["name"] == "x.ts"))
                    .unwrap_or(false)
        })
    })
    .await;
    assert!(got_snapshot, "overflow must push a full fs/snapshot through the loop");

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn disconnect_drops_session_subscriptions() {
    let (actor, raw_rx, push, pe, dir, _db) = setup().await;
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(1, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Disconnect drops all of session 1's subscriptions (node enters grace).
    tx.send(FsInbound::Disconnect {
        session: "1".to_owned(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let count_before = push.frames().len();
    // A change now must not fan out to the disconnected session.
    std::fs::write(dir.path().join("after.ts"), b"x").unwrap();
    let no_new_delta = !wait_until(&push, Duration::from_millis(800), |f| {
        has_delta_adding(f, "1", "after.ts")
    })
    .await;
    assert!(no_new_delta, "no delta after disconnect");
    assert_eq!(push.frames().len(), count_before, "no push to a dropped session");

    drop(tx);
    let _ = handle.await;
}
