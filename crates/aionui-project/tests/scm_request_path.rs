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
use aionui_project::types::AttachInput;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Collects `(session, frame)` pairs the actor pushes, so a test can read its
/// replies and assert *which* connection each frame went to.
struct CollectingPush {
    sent: std::sync::Mutex<Vec<(String, Value)>>,
}

impl ScmWirePush for CollectingPush {
    fn push(&self, session: &str, frame: Value) {
        self.sent
            .lock()
            .expect("push sink poisoned")
            .push((session.to_owned(), frame));
    }
}

/// A project with one attached repository, plus a live actor over it.
struct Fixture {
    _db: Database,
    _repo_dir: tempfile::TempDir,
    service: Arc<ProjectService>,
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
    // Wire the service's root-change notifications into the same actor inbound, so
    // an attach/detach through the service drives a `repositoriesChanged` push —
    // exactly the composition the app installs at startup.
    service.set_scm_roots_sender(inbound.clone());

    Fixture {
        _db: db,
        _repo_dir: repo_dir,
        service,
        push,
        inbound,
        pe_id,
        project_id,
    }
}

impl Fixture {
    /// Send one request as `conn-1` and wait for the actor's reply.
    async fn call(&self, id: u64, method: &str, params: Value) -> Value {
        self.call_as("conn-1", id, method, params).await
    }

    /// Send one request as `session` and wait for the actor's reply to it.
    async fn call_as(&self, session: &str, id: u64, method: &str, params: Value) -> Value {
        let before = self.push.sent.lock().expect("sink").len();
        self.inbound
            .send(ScmInbound::Frame {
                session: session.to_owned(),
                user_id: "system_default_user".to_owned(),
                frame: json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            })
            .expect("actor alive");

        // Bounded wait: the reply is asynchronous, and a fixed sleep would either
        // be slow or flaky. Match on session too, so a reply meant for another
        // connection cannot be mistaken for this one's.
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let sent = self.push.sent.lock().expect("sink");
            if let Some((_, frame)) = sent
                .iter()
                .skip(before)
                .find(|(sess, f)| sess == session && f.get("id").and_then(Value::as_u64) == Some(id))
            {
                return frame.clone();
            }
        }
        panic!("no reply for {method} to {session} within the deadline");
    }

    /// Enqueue a `Disconnect` for `session`. The single-consumer actor processes it
    /// in FIFO order relative to later inbound events, so anything sent afterwards
    /// (e.g. an attach's `RootsChanged`) observes the session as already gone.
    fn disconnect(&self, session: &str) {
        self.inbound
            .send(ScmInbound::Disconnect {
                session: session.to_owned(),
            })
            .expect("actor alive");
    }

    /// Wait for a server-initiated notification (no `id`) with `method`, delivered
    /// to `session`, appearing after index `before`. Returns its `params`.
    async fn wait_for_notification(&self, session: &str, method: &str, before: usize) -> Value {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let sent = self.push.sent.lock().expect("sink");
            if let Some((_, frame)) = sent
                .iter()
                .skip(before)
                .find(|(sess, f)| sess == session && f.get("id").is_none() && f["method"] == method)
            {
                return frame["params"].clone();
            }
        }
        panic!("no {method} notification to {session} within the deadline");
    }

    /// How many frames have been pushed so far — a `before` cursor for the waits.
    fn frame_count(&self) -> usize {
        self.push.sent.lock().expect("sink").len()
    }

    /// Whether any frame with `method` was ever pushed to `session`.
    fn pushed_to(&self, session: &str, method: &str) -> bool {
        self.push
            .sent
            .lock()
            .expect("sink")
            .iter()
            .any(|(sess, f)| sess == session && f["method"] == method)
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

    /// The first discovered repository's wire object, so a test can inspect the
    /// fields `scm/listRepositories` actually serialized (not just the id).
    async fn first_repository(&self, id: u64) -> Value {
        let listed = self
            .call(id, "scm/listRepositories", json!({ "project_id": self.project_id }))
            .await;
        let repo = &listed["result"]["repositories"][0];
        assert!(repo.is_object(), "one repository is discovered: {listed}");
        repo.clone()
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

/// An entry with no name of its own carries no `pe_name`: the field is absent
/// from the wire, not an empty string, so the client's `pe_name || label`
/// falls straight through to `label`.
#[tokio::test]
async fn pe_name_is_absent_when_the_entry_has_no_name_of_its_own() {
    let fx = fixture().await;
    // The workspace root created by `create_standard` has no explicit entry name.
    let repo = fx.first_repository(70).await;

    assert_eq!(
        repo["root"]["pe_id"].as_str(),
        Some(fx.pe_id.as_str()),
        "the discovered repository is the workspace root: {repo}"
    );
    assert!(
        repo.get("pe_name").is_none(),
        "no name of its own means the field is omitted, not empty: {repo}"
    );
    assert!(
        repo["label"].as_str().is_some_and(|l| !l.is_empty()),
        "label always has a value to fall back to: {repo}"
    );
}

/// When the entry has an explicit name, it is carried through to the wire as
/// `pe_name`, distinct from the always-present `label`.
#[tokio::test]
async fn pe_name_is_carried_when_the_entry_has_a_name() {
    let fx = fixture().await;
    fx.service
        .rename_entry("system_default_user", &fx.pe_id, Some("My Repo".to_owned()))
        .await
        .expect("rename");

    let repo = fx.first_repository(71).await;
    assert_eq!(
        repo["pe_name"].as_str(),
        Some("My Repo"),
        "the entry's own name reaches the repository wire: {repo}"
    );
}

/// A blank name is dropped to absent, not carried as whitespace — otherwise the
/// client's `pe_name || label` would pick a blank string over the real label.
/// Removing the blank filter turns this red (the field comes back as `"   "`).
#[tokio::test]
async fn a_blank_entry_name_is_dropped_rather_than_carried() {
    let fx = fixture().await;
    fx.service
        .rename_entry("system_default_user", &fx.pe_id, Some("   ".to_owned()))
        .await
        .expect("rename");

    let repo = fx.first_repository(72).await;
    assert!(
        repo.get("pe_name").is_none(),
        "a whitespace-only name is filtered to absent: {repo}"
    );
}

/// A blank explicit name never survives as the label either: it falls through to
/// the folder's derived name (or the id). This pins "label is never blank" as a
/// backend contract, which is what makes the client's `pe_name || label` safe.
/// Removing the label's blank filter turns this red (label comes back `"   "`).
#[tokio::test]
async fn a_blank_entry_name_does_not_become_a_blank_label() {
    let fx = fixture().await;
    fx.service
        .rename_entry("system_default_user", &fx.pe_id, Some("   ".to_owned()))
        .await
        .expect("rename");

    let repo = fx.first_repository(73).await;
    let label = repo["label"].as_str().expect("label present");
    assert!(!label.trim().is_empty(), "label must not be blank, got {label:?}");
    assert!(
        repo.get("pe_name").is_none(),
        "and the blank pe_name is dropped too: {repo}"
    );
}

/// Initialise a git repository with one committed file (git2 only, no CLI), so it
/// runs identically on every platform.
fn init_committed_repo(dir: &std::path::Path) {
    let repo = git2::Repository::init(dir).expect("init repo");
    {
        let mut cfg = repo.config().expect("config");
        cfg.set_str("user.name", "scm test").expect("name");
        cfg.set_str("user.email", "scm@test.local").expect("email");
    }
    std::fs::write(dir.join("readme.txt"), "hi\n").expect("write");
    let mut index = repo.index().expect("index");
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add");
    index.write().expect("write index");
    let tree = repo.find_tree(index.write_tree().expect("tree")).expect("find tree");
    let sig = repo.signature().expect("sig");
    repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
        .expect("commit");
}

/// Attaching a repository then detaching it drives a `repositoriesChanged` frame
/// each way, carrying `project_id`, the added descriptor, and (on detach) the
/// removed id — the whole 乙-wired path from service mutation to actor push.
#[tokio::test]
async fn attaching_and_detaching_a_repo_pushes_repositories_changed() {
    let fx = fixture().await;
    // conn-1 lists the project: registers interest and seeds the baseline (the one
    // workspace repository).
    let _ = fx.repo_id().await;

    // A second git repo outside the workspace root, so it attaches as a new root.
    let second_dir = tempfile::tempdir().expect("tempdir");
    init_committed_repo(second_dir.path());

    let before = fx.frame_count();
    let row = fx
        .service
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: fx.project_id.clone(),
                uri: to_file_uri(second_dir.path()).expect("uri"),
                display_name: Some("Second".to_owned()),
            },
        )
        .await
        .expect("attach");
    let added_repo_id = format!("scm:{}", row.pe_id);

    let params = fx
        .wait_for_notification("conn-1", "scm/repositoriesChanged", before)
        .await;
    assert_eq!(
        params["project_id"].as_str(),
        Some(fx.project_id.as_str()),
        "the frame carries project_id for client-side filtering: {params}"
    );
    let added = params["added"].as_array().expect("added present on attach");
    let added_obj = added
        .iter()
        .find(|r| r["repo_id"] == json!(added_repo_id))
        .unwrap_or_else(|| panic!("the new repo is in added: {params}"));
    assert_eq!(
        added_obj["pe_name"].as_str(),
        Some("Second"),
        "pe_name flows through the delta too"
    );
    assert!(
        params.get("removed").is_none(),
        "nothing removed on a pure attach: {params}"
    );

    // Detach → removed carries the id, added is absent.
    let before = fx.frame_count();
    fx.service
        .remove_attached("system_default_user", &row.pe_id)
        .await
        .expect("detach");
    let params = fx
        .wait_for_notification("conn-1", "scm/repositoriesChanged", before)
        .await;
    let removed: Vec<&str> = params["removed"]
        .as_array()
        .expect("removed present on detach")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        removed.contains(&added_repo_id.as_str()),
        "the detached repo is reported removed: {params}"
    );
    assert!(
        params.get("added").is_none(),
        "nothing added on a pure detach: {params}"
    );
}

/// A `repositoriesChanged` frame reaches only sessions that expressed interest by
/// listing the project — never a connection that did not (no leakage).
#[tokio::test]
async fn repositories_changed_reaches_only_interested_sessions() {
    let fx = fixture().await;
    // conn-1 registers interest; conn-2 never lists this project.
    let _ = fx.repo_id().await;

    let second_dir = tempfile::tempdir().expect("tempdir");
    init_committed_repo(second_dir.path());
    let before = fx.frame_count();
    fx.service
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: fx.project_id.clone(),
                uri: to_file_uri(second_dir.path()).expect("uri"),
                display_name: None,
            },
        )
        .await
        .expect("attach");

    // conn-1 receives it...
    let _ = fx
        .wait_for_notification("conn-1", "scm/repositoriesChanged", before)
        .await;
    // ...and conn-2, uninterested, never does.
    assert!(
        !fx.pushed_to("conn-2", "scm/repositoriesChanged"),
        "an uninterested connection receives no repositoriesChanged frame"
    );
}

/// Removing a repository then re-adding the same folder re-discovers it. This is
/// the race the release-on-remove could break: because a recompute discovers
/// *before* it releases, a folder that is present again is never torn down, so the
/// re-add surfaces normally instead of vanishing behind a stale release.
#[tokio::test]
async fn a_removed_repo_can_be_re_added() {
    let fx = fixture().await;
    let _ = fx.repo_id().await;

    let second_dir = tempfile::tempdir().expect("tempdir");
    init_committed_repo(second_dir.path());
    let uri = to_file_uri(second_dir.path()).expect("uri");

    // Attach, wait for the add.
    let before = fx.frame_count();
    let row = fx
        .service
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: fx.project_id.clone(),
                uri: uri.clone(),
                display_name: None,
            },
        )
        .await
        .expect("attach");
    let _ = fx
        .wait_for_notification("conn-1", "scm/repositoriesChanged", before)
        .await;

    // Detach, wait for the removal.
    let before = fx.frame_count();
    fx.service
        .remove_attached("system_default_user", &row.pe_id)
        .await
        .expect("detach");
    let _ = fx
        .wait_for_notification("conn-1", "scm/repositoriesChanged", before)
        .await;

    // Re-attach the same folder (a fresh entry, hence a fresh repo_id) and confirm
    // it re-appears rather than staying released.
    let before = fx.frame_count();
    let row2 = fx
        .service
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: fx.project_id.clone(),
                uri,
                display_name: None,
            },
        )
        .await
        .expect("re-attach");
    let repo_id2 = format!("scm:{}", row2.pe_id);
    let params = fx
        .wait_for_notification("conn-1", "scm/repositoriesChanged", before)
        .await;
    let added: Vec<&str> = params["added"]
        .as_array()
        .expect("added present on re-attach")
        .iter()
        .filter_map(|r| r["repo_id"].as_str())
        .collect();
    assert!(
        added.contains(&repo_id2.as_str()),
        "the re-added repository re-appears: {params}"
    );
}

/// Interest is released when a connection drops: a session that listed a project
/// then disconnected must not keep receiving its `repositoriesChanged` frames.
///
/// A second, still-connected session proves the change actually fired, so the
/// disconnected one's silence is conclusive rather than a race. The single-
/// consumer actor processes the `Disconnect` before the attach's `RootsChanged`
/// (both enter one FIFO channel, disconnect first), so interest is already gone
/// by the time the recompute fans out. Removing the drop_session interest cleanup
/// turns this red (the dead session receives the frame).
#[tokio::test]
async fn repositories_changed_stops_after_the_session_disconnects() {
    let fx = fixture().await;
    // conn-1 and conn-2 both register interest by listing the project.
    let _ = fx
        .call_as(
            "conn-1",
            1,
            "scm/listRepositories",
            json!({ "project_id": fx.project_id }),
        )
        .await;
    let _ = fx
        .call_as(
            "conn-2",
            2,
            "scm/listRepositories",
            json!({ "project_id": fx.project_id }),
        )
        .await;

    // conn-1 drops.
    fx.disconnect("conn-1");

    // A repository is attached: the change must reach the still-connected conn-2
    // and never the dropped conn-1.
    let second_dir = tempfile::tempdir().expect("tempdir");
    init_committed_repo(second_dir.path());
    let before = fx.frame_count();
    fx.service
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: fx.project_id.clone(),
                uri: to_file_uri(second_dir.path()).expect("uri"),
                display_name: None,
            },
        )
        .await
        .expect("attach");

    // conn-2 receives it (this also proves the recompute ran)...
    let _ = fx
        .wait_for_notification("conn-2", "scm/repositoriesChanged", before)
        .await;
    // ...and conn-1, disconnected, never did.
    assert!(
        !fx.pushed_to("conn-1", "scm/repositoriesChanged"),
        "a disconnected session must not keep receiving repositoriesChanged"
    );
}

/// A folder whose directory basename is blank (literally named "   ") must not
/// yield a blank label. The derived `default_display_name` has to filter blanks
/// the same way the scm seam does, or `display_name || default_display_name ||
/// pe_id` renders whitespace. Reverting `build_folder_dto` to a non-trim filter
/// turns this red — which is also what proves the defect was real.
#[tokio::test]
async fn a_blank_basename_folder_does_not_become_a_blank_label() {
    let fx = fixture().await;

    // A git repo whose own directory name is three spaces.
    let base = tempfile::tempdir().expect("tempdir");
    let blank_dir = base.path().join("   ");
    std::fs::create_dir(&blank_dir).expect("mkdir blank");
    init_committed_repo(&blank_dir);

    let row = fx
        .service
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: fx.project_id.clone(),
                uri: to_file_uri(&blank_dir).expect("uri"),
                display_name: None,
            },
        )
        .await
        .expect("attach");

    let listed = fx
        .call(90, "scm/listRepositories", json!({ "project_id": fx.project_id }))
        .await;
    let repos = listed["result"]["repositories"].as_array().expect("repositories");
    let repo = repos
        .iter()
        .find(|r| r["root"]["pe_id"] == json!(row.pe_id))
        .unwrap_or_else(|| panic!("the attached repo is listed: {listed}"));
    let label = repo["label"].as_str().expect("label present");
    assert!(
        !label.trim().is_empty(),
        "a whitespace-basename folder must not yield a blank label, got {label:?}"
    );
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
