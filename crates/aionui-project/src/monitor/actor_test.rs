use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::ProjectService;
use crate::canonical::to_file_uri;
use crate::monitor::{FsInbound, FsMonitorActor, FsWirePush};
use crate::runtime::{
    Budget, CancellationToken, EntryFact, FsError, IFsRuntime, IFsSearchProvider, Kind, LocalFsRuntime, MatchMode,
    NameMatcher, RawEvent, SearchSink, ShardOutput, Snapshot, Subscriber,
};

use super::super::search::{ActiveSearch, SearchDone, SearchJob, SearchRoot, run_search};

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
async fn create_file_makes_empty_file() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(10, "fs/createFile", json!({"file":dir_ref(&pe, "new.ts")})),
        )
        .await;
    assert!(push.last_for("1").unwrap()["result"].is_object());
    let path = dir.path().join("new.ts");
    assert!(path.is_file());
    // create_new opens without truncating existing content, but a fresh file is empty.
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
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

// ══ fs/copy · fs/move (drag-transfer) ════════════════════════════════════════

#[tokio::test]
async fn copy_file_into_subdir_keeps_source() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                40,
                "fs/copy",
                json!({"from":dir_ref(&pe, "a.txt"),"to_dir":dir_ref(&pe, "sub")}),
            ),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["result"]["name"], "a.txt");
    assert_eq!(reply["result"]["to"]["relative_path"], "sub/a.txt");
    assert!(dir.path().join("sub").join("a.txt").is_file());
    // Copy keeps the source.
    assert!(dir.path().join("a.txt").is_file());
}

#[tokio::test]
async fn copy_into_same_dir_auto_renames() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();

    // Copy a.txt into the dir it already lives in → deliberate duplicate,
    // auto-renamed with the extension preserved.
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                41,
                "fs/copy",
                json!({"from":dir_ref(&pe, "a.txt"),"to_dir":dir_ref(&pe, "")}),
            ),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["result"]["name"], "a copy.txt");
    assert_eq!(reply["result"]["to"]["relative_path"], "a copy.txt");
    assert!(dir.path().join("a.txt").is_file());
    assert!(dir.path().join("a copy.txt").is_file());
}

#[tokio::test]
async fn copy_directory_is_recursive() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("child.txt"), b"x").unwrap();
    std::fs::create_dir(dir.path().join("dest")).unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                42,
                "fs/copy",
                json!({"from":dir_ref(&pe, "src"),"to_dir":dir_ref(&pe, "dest")}),
            ),
        )
        .await;

    assert!(push.last_for("1").unwrap()["result"].is_object());
    assert!(dir.path().join("dest").join("src").join("child.txt").is_file());
    // Source tree untouched.
    assert!(dir.path().join("src").join("child.txt").is_file());
}

#[tokio::test]
async fn move_file_into_subdir_removes_source() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                43,
                "fs/move",
                json!({"from":dir_ref(&pe, "a.txt"),"to_dir":dir_ref(&pe, "sub")}),
            ),
        )
        .await;

    assert!(push.last_for("1").unwrap()["result"].is_object());
    assert!(dir.path().join("sub").join("a.txt").is_file());
    // Move removes the source.
    assert!(!dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn move_into_own_parent_is_noop() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();

    // Moving into the directory it already sits in must not manufacture a copy.
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                44,
                "fs/move",
                json!({"from":dir_ref(&pe, "a.txt"),"to_dir":dir_ref(&pe, "")}),
            ),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["result"]["name"], "a.txt");
    assert!(dir.path().join("a.txt").is_file());
    assert!(!dir.path().join("a copy.txt").exists());
}

#[tokio::test]
async fn copy_directory_into_own_descendant_is_rejected() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::create_dir(dir.path().join("a").join("b")).unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                45,
                "fs/copy",
                json!({"from":dir_ref(&pe, "a"),"to_dir":dir_ref(&pe, "a/b")}),
            ),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32602);
    assert_eq!(reply["error"]["message"], "invalid_params");
}

#[tokio::test]
async fn transfer_missing_source_is_resource_not_found() {
    let (mut actor, _rx, push, pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                46,
                "fs/copy",
                json!({"from":dir_ref(&pe, "ghost.txt"),"to_dir":dir_ref(&pe, "")}),
            ),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32002);
    assert_eq!(reply["error"]["message"], "resource_not_found");
}

#[tokio::test]
async fn transfer_root_source_is_invalid_params() {
    let (mut actor, _rx, push, pe, _dir, _db) = setup().await;
    // The workspace root cannot itself be a transfer source.
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                47,
                "fs/copy",
                json!({"from":dir_ref(&pe, ""),"to_dir":dir_ref(&pe, "")}),
            ),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32602);
}

#[tokio::test]
async fn copy_across_project_explorers() {
    // Two independently-bound roots on one service → cross-pe copy resolves each
    // reference against its own root.
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let pe_a = service
        .create_standard("system_default_user", to_file_uri(dir_a.path()).unwrap())
        .await
        .unwrap()
        .project_explorer
        .pe_id;
    let pe_b = service
        .create_standard("system_default_user", to_file_uri(dir_b.path()).unwrap())
        .await
        .unwrap()
        .project_explorer
        .pe_id;
    std::fs::write(dir_a.path().join("x.txt"), b"cross").unwrap();

    let push = RecordingPush::default();
    let (mut actor, _rx) = FsMonitorActor::new(service, Arc::new(push.clone()), 4096).unwrap();

    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(
                48,
                "fs/copy",
                json!({"from":dir_ref(&pe_a, "x.txt"),"to_dir":dir_ref(&pe_b, "")}),
            ),
        )
        .await;

    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["result"]["name"], "x.txt");
    assert_eq!(reply["result"]["to"]["pe_id"], pe_b);
    assert!(dir_b.path().join("x.txt").is_file());
    // Source root untouched.
    assert!(dir_a.path().join("x.txt").is_file());
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
async fn create_file_existing_is_provider_unavailable() {
    let (mut actor, _rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("keep.txt"), b"original").unwrap();

    // createFile over an existing file → AlreadyExists → provider_unavailable
    // (-32006), and it must NOT truncate the existing content.
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(30, "fs/createFile", json!({"file":dir_ref(&pe, "keep.txt")})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["error"]["code"], -32006);
    assert_eq!(reply["error"]["message"], "provider_unavailable");
    assert_eq!(reply["error"]["data"]["relative_path"], "keep.txt");
    assert_eq!(std::fs::read(dir.path().join("keep.txt")).unwrap(), b"original");
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
/// Driven through `fs/mkdir` — any resolve-guarded command shares the guard.
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
            request(20, "fs/mkdir", json!({"dir":dir_ref(&pe, "link/secret.txt")})),
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
                mtime_ms: Some(1_700_000_000_000),
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

fn has_delta_modifying(frames: &[(String, Value)], session: &str, name: &str) -> bool {
    frames.iter().any(|(s, f)| {
        s == session
            && f["method"] == "fs/delta"
            && f["params"]["changes"]
                .as_array()
                .map(|cs| cs.iter().any(|c| c["op"] == "modified" && c["name"] == name))
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

/// An edit to a file's *contents* must reach the subscriber as `op: "modified"`.
///
/// The two ends of this were already covered and the middle was not: `TreeModel`
/// tests prove the diff produces a `Modified` change, and the test above proves a
/// delta reaches a subscriber — but nothing showed a `modified` change surviving
/// the trip. Since the whole point of that change is to let preview light a refresh
/// affordance, "generated correctly" and "delivered" are separate claims, and only
/// the first had evidence.
///
/// The timestamp is advanced explicitly rather than by writing twice: some
/// filesystems record whole seconds, so back-to-back writes can leave mtime
/// untouched, and the detection deliberately under-reports when it cannot see a
/// change. A test that relied on write timing would pass or fail by the clock —
/// and an intermittently-red test gets triaged as a flake and muted, which costs
/// more than having no test, because it also removes the signal that this path was
/// ever meant to be guarded.
#[tokio::test]
async fn live_content_edit_fans_modified_delta_to_subscriber() {
    let (actor, raw_rx, push, pe, dir, _db) = setup().await;
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    // The file has to exist before the subscription so its baseline mtime is part
    // of the snapshot — otherwise the edit below would surface as `added`.
    let file = dir.path().join("watched.md");
    std::fs::write(&file, b"before").unwrap();

    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(1, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
    })
    .unwrap();
    // Let subscribe mount + arm the watch.
    tokio::time::sleep(Duration::from_millis(250)).await;

    std::fs::write(&file, b"after").unwrap();
    let bumped = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(bumped)
        .unwrap();

    assert!(
        wait_until(&push, Duration::from_secs(5), |f| has_delta_modifying(
            f,
            "1",
            "watched.md"
        ))
        .await,
        "an edited file must reach the subscriber as op:'modified' — the signal preview keys its refresh on"
    );

    // The listing did not change, so the edit must not also be reported as a
    // structural change; a client applying both would rebuild the node needlessly.
    assert!(
        !has_delta_adding(&push.frames(), "1", "watched.md"),
        "a content edit must not surface as 'added'"
    );

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn noise_file_change_produces_no_delta() {
    // Creating a noise file in a subscribed dir must not fan any delta (the
    // precise-event stat path and the coarse read_dir rescan both hide noise),
    // while a real sibling file still fans its delta.
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

    // A noise file and a real file land together.
    std::fs::write(dir.path().join(".DS_Store"), b"x").unwrap();
    std::fs::write(dir.path().join("real.ts"), b"x").unwrap();

    // The real file's delta arrives (control) — proving the watch is live.
    assert!(
        wait_until(&push, Duration::from_secs(5), |f| has_delta_adding(f, "1", "real.ts")).await,
        "the real file must fan a delta"
    );
    // The noise file must never appear in any pushed frame.
    assert!(
        !push.frames().iter().any(|(_, f)| f.to_string().contains(".DS_Store")),
        "noise file must not appear in any delta/snapshot"
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

    // Tagged as a rescan. Without this the push is shape-identical to a subscribe
    // reply, and a receiver reading it as "here is the listing" loses every change
    // in the window — overflow supersedes the buffered per-child events during
    // debounce, so those deltas were never sent separately.
    let tagged = push
        .frames()
        .iter()
        .any(|(s, m)| s == "1" && m["method"] == "fs/snapshot" && m["params"]["reason"] == "overflow");
    assert!(tagged, "an overflow snapshot must carry reason:'overflow'");

    drop(tx);
    let _ = handle.await;
}

/// The subscribe reply must *not* be tagged. Both paths build their params from the
/// same snapshot, so a marker leaking onto the first listing would make every
/// freshly-opened directory look like a rescan — the receiver would re-read content
/// it just received, on every subscribe.
#[tokio::test]
async fn subscribe_reply_snapshot_is_not_marked_as_overflow() {
    let (actor, raw_rx, push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.ts"), b"x").unwrap();
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(1, "fs/subscribe", json!({"targets":[dir_ref(&pe, "")]})),
    })
    .unwrap();

    // The reply is a response to id 1, not an fs/snapshot notification.
    let got_reply = wait_until(&push, Duration::from_secs(5), |frames| {
        frames
            .iter()
            .any(|(s, m)| s == "1" && m["id"] == 1 && m["result"]["snapshots"].is_array())
    })
    .await;
    assert!(got_reply, "subscribe must answer with snapshots");

    let leaked = push.frames().iter().any(|(_, m)| m.to_string().contains("overflow"));
    assert!(!leaked, "no frame from a plain subscribe may mention overflow");

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

// ══ filename search — actor state machine (deterministic helpers) ═════════════

fn active(id: i64) -> (ActiveSearch, CancellationToken) {
    let cancel = CancellationToken::new();
    (
        ActiveSearch {
            search_id: json!(id),
            cancel: cancel.clone(),
        },
        cancel,
    )
}

#[tokio::test]
async fn register_search_supersedes_and_cancels_previous() {
    let (mut actor, _rx, _push, _pe, _dir, _db) = setup().await;
    let (first, first_cancel) = active(1);
    let (second, _second_cancel) = active(2);

    actor.register_search("1", first);
    actor.register_search("1", second);

    // Superseded search's token is cancelled; the current entry is the new id.
    assert!(first_cancel.is_cancelled());
    assert_eq!(actor.active_searches.get("1").unwrap().search_id, json!(2));
}

#[tokio::test]
async fn cancel_search_only_when_id_matches() {
    let (mut actor, _rx, _push, _pe, _dir, _db) = setup().await;
    let (search, cancel) = active(7);
    actor.register_search("1", search);

    // Non-matching id → no cancel, entry stays.
    assert!(!actor.cancel_search("1", &json!(8)));
    assert!(!cancel.is_cancelled());
    assert!(actor.active_searches.contains_key("1"));

    // Matching id → cancelled + removed.
    assert!(actor.cancel_search("1", &json!(7)));
    assert!(cancel.is_cancelled());
    assert!(!actor.active_searches.contains_key("1"));
}

#[tokio::test]
async fn disconnect_cascades_cancel_to_running_search() {
    let (mut actor, _rx, _push, _pe, _dir, _db) = setup().await;
    let (search, cancel) = active(1);
    actor.register_search("1", search);

    actor.drop_session_search("1");

    assert!(cancel.is_cancelled());
    assert!(!actor.active_searches.contains_key("1"));
}

#[tokio::test]
async fn on_search_done_clears_only_the_current_search() {
    let (mut actor, _rx, _push, _pe, _dir, _db) = setup().await;
    let (search, _c) = active(7);
    actor.register_search("1", search);

    // Completion of the current search clears its entry.
    actor.on_search_done(SearchDone {
        session: "1".to_owned(),
        search_id: json!(7),
    });
    assert!(!actor.active_searches.contains_key("1"));

    // A stale done (id 7) arriving after a superseding search (id 8) must NOT
    // clobber the newer entry.
    let (newer, _c2) = active(8);
    actor.register_search("1", newer);
    actor.on_search_done(SearchDone {
        session: "1".to_owned(),
        search_id: json!(7),
    });
    assert_eq!(
        actor.active_searches.get("1").unwrap().search_id,
        json!(8),
        "stale done must not clobber the superseding search"
    );
}

// ══ filename search — end-to-end through the real event loop ══════════════════

/// A `fs/searchMatch` hit for `session` naming `file` with `pe_id`.
fn has_search_hit(frames: &[(String, Value)], session: &str, pe_id: &str, name: &str) -> bool {
    frames.iter().any(|(s, f)| {
        s == session
            && f["method"] == "fs/searchMatch"
            && f["params"]["matches"]
                .as_array()
                .map(|ms| ms.iter().any(|m| m["pe_id"] == pe_id && m["name"] == name))
                .unwrap_or(false)
    })
}

/// The terminal `fs/search` response (a `result` for `id`) delivered to `session`.
fn search_terminal(frames: &[(String, Value)], session: &str, id: i64) -> Option<Value> {
    frames
        .iter()
        .find(|(s, f)| s == session && f["id"] == id && f.get("result").is_some())
        .map(|(_, f)| f["result"].clone())
}

#[tokio::test]
async fn search_streams_matches_and_terminal_through_loop() {
    let (actor, raw_rx, push, pe, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("Button.tsx"), b"x").unwrap();
    std::fs::write(dir.path().join("Widget.tsx"), b"x").unwrap();

    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(7, "fs/search", json!({"roots":[dir_ref(&pe, "")],"query":"button"})),
    })
    .unwrap();

    // Terminal arrives (search is spawned off the loop, then streams + finishes).
    let got_terminal = wait_until(&push, Duration::from_secs(5), |f| search_terminal(f, "1", 7).is_some()).await;
    assert!(got_terminal, "search must send a terminal response");

    let frames = push.frames();
    // The matching file streamed with the root's pe_id stamped; the non-match did not.
    assert!(has_search_hit(&frames, "1", &pe, "Button.tsx"));
    assert!(!has_search_hit(&frames, "1", &pe, "Widget.tsx"));
    let result = search_terminal(&frames, "1", 7).unwrap();
    assert_eq!(result["limit_reached"], false);
    assert_eq!(result["total"], 1);

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn search_unknown_pe_is_out_of_scope() {
    // Resolve failure replies synchronously (before any coordinator spawn).
    let (mut actor, _rx, push, _pe, _dir, _db) = setup().await;
    actor
        .dispatch_frame(
            "1",
            "system_default_user",
            request(8, "fs/search", json!({"roots":[dir_ref("pe-nope", "")],"query":"x"})),
        )
        .await;
    let reply = push.last_for("1").unwrap();
    assert_eq!(reply["id"], 8);
    assert_eq!(reply["error"]["code"], -32000);
    assert_eq!(reply["error"]["message"], "out_of_scope");
    assert_eq!(reply["error"]["data"]["pe_id"], "pe-nope");
}

/// A search provider that parks in `search_names` until released, so a test can
/// hold a search provably in-flight. `entered` fires when the walk begins;
/// `release` unblocks it.
struct BarrierSearchProvider {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl IFsSearchProvider for BarrierSearchProvider {
    async fn search_names(
        &self,
        _root_uri: &str,
        _matcher: &NameMatcher,
        sink: &Arc<dyn SearchSink>,
        budget: &Budget,
        _cancel: &CancellationToken,
    ) -> Result<(), FsError> {
        self.entered.notify_one(); // tell the test the search is in-flight
        self.release.notified().await; // park until released
        if budget.try_take() {
            sink.emit("hit.txt".to_owned(), "hit.txt".to_owned());
        }
        Ok(())
    }
}

#[tokio::test]
async fn search_running_keeps_event_loop_responsive() {
    // Inject a barrier provider so the search is provably parked in-flight when
    // the initialize arrives — proving the loop is not blocked by a running walk.
    let (mut actor, raw_rx, push, pe, _dir, _db) = setup().await;
    let barrier = Arc::new(BarrierSearchProvider {
        entered: Notify::new(),
        release: Notify::new(),
    });
    actor.set_search_provider_override(Arc::clone(&barrier) as Arc<dyn IFsSearchProvider>);
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(actor.run(rx, raw_rx));

    // Kick off the search; it will park inside the barrier provider.
    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(7, "fs/search", json!({"roots":[dir_ref(&pe, "")],"query":""})),
    })
    .unwrap();
    // Wait until the walk has actually started (search is in-flight).
    tokio::time::timeout(Duration::from_secs(2), barrier.entered.notified())
        .await
        .expect("search must enter the provider (be in-flight)");

    // With the search parked, send an initialize on the same connection.
    tx.send(FsInbound::Frame {
        session: "1".to_owned(),
        user_id: "system_default_user".to_owned(),
        frame: request(99, "initialize", json!({"protocol_version": 1})),
    })
    .unwrap();

    // The initialize is answered while the search is still parked in-flight.
    let responsive = wait_until(&push, Duration::from_secs(2), |f| {
        f.iter()
            .any(|(s, m)| s == "1" && m["id"] == 99 && m["result"]["protocol_version"] == 1)
    })
    .await;
    assert!(responsive, "event loop must stay responsive during a running search");
    // Prove the search had NOT completed when initialize was served (still parked).
    assert!(
        search_terminal(&push.frames(), "1", 7).is_none(),
        "search must still be in-flight (no terminal) when initialize was answered"
    );

    // Release the barrier → the search finishes and its terminal lands.
    barrier.release.notify_one();
    assert!(
        wait_until(&push, Duration::from_secs(3), |f| search_terminal(f, "1", 7).is_some()).await,
        "search terminal must arrive once released"
    );

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn completion_signals_done_actor_clears_and_later_cancel_is_noop() {
    // End-to-end of the clear-on-done path: a real `run_search` completing sends a
    // SearchDone; the actor clears its active-search entry; a later same-id
    // fs/searchCancel is then a no-op (the completed search is not "in-flight").
    let (mut actor, _rx, _push, pe, dir, _db) = setup().await;
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();

    // Register the search exactly as dispatch would.
    let cancel = CancellationToken::new();
    actor.register_search(
        "1",
        ActiveSearch {
            search_id: json!(5),
            cancel,
        },
    );

    // Drive a real walk (real LocalFsProvider) over the workspace root to natural
    // completion, capturing the done signal it emits.
    let (runtime, _watch_rx) = LocalFsRuntime::new().unwrap();
    let provider = runtime.search_provider().expect("file scheme supports search");
    let sink_push: Arc<dyn FsWirePush> = Arc::new(RecordingPush::default());
    let (done_tx, mut done_rx) = unbounded_channel();
    run_search(
        provider,
        sink_push,
        SearchJob {
            session: "1".to_owned(),
            search_id: json!(5),
            roots: vec![SearchRoot {
                root_uri: to_file_uri(dir.path()).unwrap(),
                pe_id: pe.clone(),
            }],
            matcher: NameMatcher::new("", MatchMode::Substring),
            budget: Budget::new(100),
            cancel: CancellationToken::new(),
        },
        done_tx,
    )
    .await;

    // Natural completion emitted a done for this session + id.
    let done = done_rx.try_recv().expect("done signal on natural completion");
    assert_eq!(done.session, "1");
    assert_eq!(done.search_id, json!(5));

    // Actor clears the entry; a later same-id cancel finds nothing in-flight.
    actor.on_search_done(done);
    assert!(
        !actor.active_searches.contains_key("1"),
        "completed search entry cleared"
    );
    assert!(
        !actor.cancel_search("1", &json!(5)),
        "a completed search must not be treated as in-flight by fs/searchCancel"
    );
}
