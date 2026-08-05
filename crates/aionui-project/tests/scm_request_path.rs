//! Source-control request-path tests.
//!
//! These go through the actor, not the provider, because the behaviour under test
//! belongs to the seam between them: the orchestration layer resolves a reference
//! and the engine consumes the result. A provider-level test cannot show whether
//! the path that was authorized is the path that got used.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use aionui_project::ProjectService;
use aionui_project::canonical::to_file_uri;
use aionui_project::scm::{ScmActor, ScmInbound, ScmWirePush};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Collects the frames the actor pushes, so a test can read its replies.
struct CollectingPush {
    sent: std::sync::Mutex<Vec<Value>>,
}

impl ScmWirePush for CollectingPush {
    fn push(&self, _session: &str, frame: Value) {
        self.sent.lock().expect("push sink poisoned").push(frame);
    }
}

/// A project with one attached repository, plus a live actor over it.
struct Fixture {
    _db: Database,
    _repo_dir: tempfile::TempDir,
    push: Arc<CollectingPush>,
    inbound: tokio::sync::mpsc::UnboundedSender<ScmInbound>,
    pe_id: String,
    project_id: String,
}

async fn fixture() -> Fixture {
    let db = init_database_memory().await.expect("db");
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));

    // A real repository with a file one directory deep, so `dir/sub/../file`-style
    // spellings have something to resolve against.
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let repo = git2::Repository::init(repo_dir.path()).expect("init repo");
    {
        let mut cfg = repo.config().expect("config");
        cfg.set_str("user.name", "scm test").expect("name");
        cfg.set_str("user.email", "scm@test.local").expect("email");
    }
    std::fs::create_dir_all(repo_dir.path().join("dir").join("sub")).expect("mkdir");
    std::fs::write(repo_dir.path().join("dir").join("file.txt"), "content\n").expect("write");
    std::fs::write(repo_dir.path().join("dir").join("sub").join("other.txt"), "x\n").expect("write");
    let mut index = repo.index().expect("index");
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add");
    index.write().expect("write index");
    let tree = repo.find_tree(index.write_tree().expect("tree")).expect("find tree");
    let sig = repo.signature().expect("sig");
    repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
        .expect("commit");
    // Binding it as a project creates the explorer entry we need, so there is
    // nothing further to attach.
    let created = service
        .create_standard("system_default_user", to_file_uri(repo_dir.path()).expect("uri"))
        .await
        .expect("create project");
    let pe_id = created.project_explorer.pe_id.clone();
    let project_id = created.project.project_id.clone();

    let push = Arc::new(CollectingPush {
        sent: std::sync::Mutex::new(Vec::new()),
    });
    let actor = ScmActor::new(Arc::clone(&service), Arc::clone(&push) as Arc<dyn ScmWirePush>).expect("actor");
    let (inbound, inbound_rx) = unbounded_channel();
    tokio::spawn(actor.run(inbound_rx));

    Fixture {
        _db: db,
        _repo_dir: repo_dir,
        push,
        inbound,
        pe_id,
        project_id,
    }
}

impl Fixture {
    /// Send one request and wait for the actor's reply.
    async fn call(&self, id: u64, method: &str, params: Value) -> Value {
        let before = self.push.sent.lock().expect("sink").len();
        self.inbound
            .send(ScmInbound::Frame {
                session: "conn-1".to_owned(),
                user_id: "system_default_user".to_owned(),
                frame: json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            })
            .expect("actor alive");

        // Bounded wait: the reply is asynchronous, and a fixed sleep would either
        // be slow or flaky.
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let sent = self.push.sent.lock().expect("sink");
            if let Some(frame) = sent
                .iter()
                .skip(before)
                .find(|f| f.get("id").and_then(Value::as_u64) == Some(id))
            {
                return frame.clone();
            }
        }
        panic!("no reply for {method} within the deadline");
    }

    async fn repo_id(&self) -> String {
        let listed = self
            .call(1, "scm/listRepositories", json!({ "project_id": self.project_id }))
            .await;
        listed["result"]["repositories"][0]["repo_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no repository discovered: {listed}"))
            .to_owned()
    }
}

/// Spellings of one path that all denote `dir/file.txt`. Containment accepts every
/// one of them, so all of them reach the engine — and the engine treats them
/// differently, which is why the orchestration layer must normalize first.
const EQUIVALENT_SPELLINGS: &[&str] = &[
    "dir/file.txt",
    "dir/./file.txt",
    "dir/sub/../file.txt",
    "./dir/file.txt",
];

/// Every spelling of the same file must read the same content, at every anchor.
///
/// Before the orchestration layer passed on its normalized path, these diverged:
/// one spelling silently returned "nothing" for the staged anchor, another failed
/// to match at all, and a leading `./` reached a library call that rejects such
/// paths — surfacing as an opaque internal failure rather than a result.
#[tokio::test]
async fn every_spelling_of_a_path_reads_the_same_content_at_every_anchor() {
    let fx = fixture().await;
    let repo_id = fx.repo_id().await;

    for anchor in ["working", "committed", "staged"] {
        let mut answers = Vec::new();
        for spelling in EQUIVALENT_SPELLINGS {
            let reply = fx
                .call(
                    10,
                    "scm/original",
                    json!({
                        "repository": repo_id,
                        "file": { "pe_id": fx.pe_id, "relative_path": spelling },
                        "at": anchor,
                    }),
                )
                .await;
            assert!(
                reply.get("error").is_none(),
                "anchor {anchor} rejected {spelling:?}: {reply}"
            );
            answers.push((*spelling, reply["result"]["content"].as_str().map(str::to_owned)));
        }

        let (_, first) = &answers[0];
        assert!(
            first.is_some(),
            "the plain spelling reads content at {anchor}: {answers:?}"
        );
        for (spelling, content) in &answers {
            assert_eq!(
                content, first,
                "{spelling:?} must read the same content as the plain spelling at {anchor}: {answers:?}"
            );
        }
    }
}

/// A leading `./` used to reach a library call that rejects it outright, so the
/// request came back as an internal failure. It must simply work.
#[tokio::test]
async fn a_leading_dot_slash_is_normalized_rather_than_reaching_the_engine() {
    let fx = fixture().await;
    let repo_id = fx.repo_id().await;

    let reply = fx
        .call(
            20,
            "scm/original",
            json!({
                "repository": repo_id,
                "file": { "pe_id": fx.pe_id, "relative_path": "./dir/file.txt" },
                "at": "staged",
            }),
        )
        .await;

    assert!(reply.get("error").is_none(), "no failure for a `./` spelling: {reply}");
    assert_eq!(
        reply["result"]["content"].as_str(),
        Some("content\n"),
        "and it resolves to the real file: {reply}"
    );
}

/// The same normalization must apply to actions, and to **every** entry in a
/// batch — not just the first one.
#[tokio::test]
async fn staging_normalizes_every_entry_in_the_batch() {
    let fx = fixture().await;
    let repo_id = fx.repo_id().await;

    // Two files, each named in a form the engine would not match verbatim.
    std::fs::write(fx._repo_dir.path().join("dir").join("file.txt"), "edited\n").expect("edit");
    std::fs::write(
        fx._repo_dir.path().join("dir").join("sub").join("other.txt"),
        "edited\n",
    )
    .expect("edit");

    let reply = fx
        .call(
            30,
            "scm/stage",
            json!({
                "repository": repo_id,
                "files": [
                    { "pe_id": fx.pe_id, "relative_path": "./dir/file.txt" },
                    { "pe_id": fx.pe_id, "relative_path": "dir/sub/./other.txt" },
                ],
            }),
        )
        .await;

    assert!(reply.get("error").is_none(), "the batch is accepted: {reply}");
    assert!(
        reply["result"]["failed"].is_null(),
        "and every entry succeeded — a batch must normalize all of them, not only the first: {reply}"
    );

    // Confirm against the repository itself: both files are staged.
    let status = fx.call(31, "scm/status", json!({ "repository": repo_id })).await;
    let staged: Vec<&str> = status["result"]["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter(|r| r["staged"].as_bool() == Some(true))
        .filter_map(|r| r["repo_relative_path"].as_str())
        .collect();
    assert!(
        staged.contains(&"dir/file.txt") && staged.contains(&"dir/sub/other.txt"),
        "both files are staged under their normalized paths, got {staged:?}"
    );
}

/// Escaping the root is still refused — normalizing must not become a way in.
#[tokio::test]
async fn a_path_escaping_the_root_is_still_refused() {
    let fx = fixture().await;
    let repo_id = fx.repo_id().await;

    for escape in ["../outside.txt", "dir/../../outside.txt"] {
        let reply = fx
            .call(
                40,
                "scm/original",
                json!({
                    "repository": repo_id,
                    "file": { "pe_id": fx.pe_id, "relative_path": escape },
                    "at": "working",
                }),
            )
            .await;
        assert!(reply.get("error").is_some(), "{escape:?} must be refused, got {reply}");
    }
}

/// Silence the unused-field warnings for handles kept only to own their lifetime.
#[allow(dead_code)]
fn _lifetimes_are_owned(_: &Fixture, _: &UnboundedReceiver<ScmInbound>, _: PathBuf) {}

/// Subscribing to several repositories reports per repository, like the multi-file
/// actions do.
///
/// The point is agreement about state: the server arms a watch and records a
/// subscriber for each one that works. Failing the whole call would hide those from
/// the client, which would then never unsubscribe them and would receive pushes for
/// repositories it does not believe it subscribed to.
#[tokio::test]
async fn subscribing_reports_per_repository_and_keeps_the_ones_that_worked() {
    let fx = fixture().await;
    let good = fx.repo_id().await;

    let reply = fx
        .call(
            50,
            "scm/subscribe",
            json!({ "repositories": [good, "scm:does-not-exist"] }),
        )
        .await;

    assert!(
        reply.get("error").is_none(),
        "one bad entry must not fail the whole request: {reply}"
    );
    let statuses = reply["result"]["statuses"].as_array().expect("statuses");
    assert_eq!(statuses.len(), 1, "the good repository is subscribed: {reply}");
    assert_eq!(statuses[0]["repository"]["repo_id"].as_str(), Some(good.as_str()));

    let failed = reply["result"]["failed"].as_array().expect("failed listed");
    assert_eq!(failed.len(), 1, "and the bad one is reported: {reply}");
    assert_eq!(failed[0]["repo_id"].as_str(), Some("scm:does-not-exist"));
    assert!(
        failed[0]["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "with a reason to show: {reply}"
    );
}

/// When every repository subscribes, the frame is exactly what it was before
/// per-item reporting existed — a client that ignores `failed` is unaffected.
#[tokio::test]
async fn a_fully_successful_subscribe_omits_the_failure_list() {
    let fx = fixture().await;
    let good = fx.repo_id().await;

    let reply = fx.call(60, "scm/subscribe", json!({ "repositories": [good] })).await;

    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["result"]["statuses"].as_array().expect("statuses").len(), 1);
    assert!(
        reply["result"]["failed"].is_null(),
        "no `failed` key when nothing failed: {reply}"
    );
}
