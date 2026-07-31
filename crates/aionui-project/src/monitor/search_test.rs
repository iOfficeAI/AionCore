//! Unit tests for the search orchestration coordinator ([`run_search`]): merge
//! across roots with per-root `pe_id` stamping, batched `fs/searchMatch`, the
//! terminal frame, global-budget cap, the natural-completion done signal, and
//! the cancel contract — drop the un-pushed buffer and send no terminal frame,
//! whether cancelled before start or mid-run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::unbounded_channel;

use crate::runtime::{FsError, MatchMode};

use super::*;

/// Collects every pushed frame as `(session, frame)`.
#[derive(Default)]
struct CapturePush(Mutex<Vec<(String, Value)>>);

impl FsWirePush for CapturePush {
    fn push(&self, session: &str, frame: Value) {
        self.0.lock().unwrap().push((session.to_owned(), frame));
    }
}

impl CapturePush {
    fn frames(&self) -> Vec<Value> {
        self.0.lock().unwrap().iter().map(|(_, f)| f.clone()).collect()
    }
}

/// A scripted search provider: per root URI, a fixed list of `(rel, name)` hits.
/// Honors matcher/budget/cancel exactly like the real provider's inner loop.
/// `cancel_after` (if set) flips the shared token after that many *emitted* hits
/// to simulate an external cancel arriving mid-walk.
struct ScriptedProvider {
    by_root: HashMap<String, Vec<(String, String)>>,
    cancel_after: Option<usize>,
}

#[async_trait]
impl IFsSearchProvider for ScriptedProvider {
    async fn search_names(
        &self,
        root_uri: &str,
        matcher: &NameMatcher,
        sink: &Arc<dyn SearchSink>,
        budget: &Budget,
        cancel: &CancellationToken,
    ) -> Result<(), FsError> {
        let Some(hits) = self.by_root.get(root_uri) else {
            return Ok(());
        };
        let mut emitted = 0usize;
        for (rel, name) in hits {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if !matcher.matches(name) {
                continue;
            }
            if !budget.try_take() {
                return Ok(());
            }
            sink.emit(rel.clone(), name.clone());
            emitted += 1;
            if self.cancel_after == Some(emitted) {
                cancel.cancel();
            }
        }
        Ok(())
    }
}

/// Collect all `fs/searchMatch` hits across every emitted batch frame.
fn collected_hits(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f["method"] == "fs/searchMatch")
        .flat_map(|f| f["params"]["matches"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// The single terminal response frame (a `result` for the request id), if any.
fn terminal(frames: &[Value]) -> Option<Value> {
    frames.iter().find(|f| f.get("result").is_some()).cloned()
}

fn provider(by_root: HashMap<String, Vec<(String, String)>>) -> Arc<dyn IFsSearchProvider> {
    Arc::new(ScriptedProvider {
        by_root,
        cancel_after: None,
    })
}

fn one_root(uri: &str, pe_id: &str) -> Vec<SearchRoot> {
    vec![SearchRoot {
        root_uri: uri.to_owned(),
        pe_id: pe_id.to_owned(),
    }]
}

#[tokio::test]
async fn merges_roots_stamps_pe_id_sends_terminal_and_signals_done() {
    let mut by_root = HashMap::new();
    by_root.insert(
        "file:///a".to_owned(),
        vec![("Button.tsx".to_owned(), "Button.tsx".to_owned())],
    );
    by_root.insert(
        "file:///b".to_owned(),
        vec![("widgets/iconButton.ts".to_owned(), "iconButton.ts".to_owned())],
    );
    let push = Arc::new(CapturePush::default());
    let (done_tx, mut done_rx) = unbounded_channel();
    let roots = vec![
        SearchRoot {
            root_uri: "file:///a".to_owned(),
            pe_id: "pe1".to_owned(),
        },
        SearchRoot {
            root_uri: "file:///b".to_owned(),
            pe_id: "pe2".to_owned(),
        },
    ];

    run_search(
        provider(by_root),
        push.clone(),
        SearchJob {
            session: "sess".to_owned(),
            search_id: json!(7),
            roots,
            matcher: NameMatcher::new("button", MatchMode::Substring),
            budget: Budget::new(100),
            cancel: CancellationToken::new(),
        },
        done_tx,
    )
    .await;

    let frames = push.frames();
    let hits = collected_hits(&frames);
    assert_eq!(hits.len(), 2);
    // Each hit carries the pe_id of the root it came from (backend-stamped).
    let by_pe: HashMap<&str, &str> = hits
        .iter()
        .map(|h| (h["pe_id"].as_str().unwrap(), h["name"].as_str().unwrap()))
        .collect();
    assert_eq!(by_pe.get("pe1"), Some(&"Button.tsx"));
    assert_eq!(by_pe.get("pe2"), Some(&"iconButton.ts"));

    // Terminal response for the originating id, total = hits, not capped.
    let term = terminal(&frames).expect("terminal frame");
    assert_eq!(term["id"], 7);
    assert_eq!(term["result"], json!({"limit_reached": false, "total": 2}));

    // Natural completion signals done for exactly this session + search_id.
    let done = done_rx.try_recv().expect("done signal");
    assert_eq!(done.session, "sess");
    assert_eq!(done.search_id, json!(7));
}

#[tokio::test]
async fn multi_root_shares_one_global_budget() {
    // Two roots, five candidate hits each, but a global budget of 3 → exactly 3
    // hits total across both roots, and the cap is reported.
    let mut by_root = HashMap::new();
    for root in ["file:///a", "file:///b"] {
        by_root.insert(
            root.to_owned(),
            (0..5).map(|i| (format!("{root}-f{i}"), format!("f{i}.txt"))).collect(),
        );
    }
    let push = Arc::new(CapturePush::default());
    let (done_tx, _done_rx) = unbounded_channel();

    run_search(
        provider(by_root),
        push.clone(),
        SearchJob {
            session: "sess".to_owned(),
            search_id: json!(1),
            roots: vec![
                SearchRoot {
                    root_uri: "file:///a".to_owned(),
                    pe_id: "pe1".to_owned(),
                },
                SearchRoot {
                    root_uri: "file:///b".to_owned(),
                    pe_id: "pe2".to_owned(),
                },
            ],
            matcher: NameMatcher::new("", MatchMode::Substring),
            budget: Budget::new(3),
            cancel: CancellationToken::new(),
        },
        done_tx,
    )
    .await;

    let frames = push.frames();
    assert_eq!(
        collected_hits(&frames).len(),
        3,
        "global budget caps total across roots"
    );
    assert_eq!(
        terminal(&frames).unwrap()["result"],
        json!({"limit_reached": true, "total": 3})
    );
}

#[tokio::test]
async fn cancelled_before_start_sends_no_terminal_and_no_done() {
    let mut by_root = HashMap::new();
    by_root.insert("file:///a".to_owned(), vec![("a.txt".to_owned(), "a.txt".to_owned())]);
    let push = Arc::new(CapturePush::default());
    let (done_tx, mut done_rx) = unbounded_channel();
    let cancel = CancellationToken::new();
    cancel.cancel(); // superseded / explicitly cancelled before running

    run_search(
        provider(by_root),
        push.clone(),
        SearchJob {
            session: "sess".to_owned(),
            search_id: json!(2),
            roots: one_root("file:///a", "pe1"),
            matcher: NameMatcher::new("", MatchMode::Substring),
            budget: Budget::new(100),
            cancel,
        },
        done_tx,
    )
    .await;

    // No terminal for a cancelled search, and no done signal (nothing to clear
    // via this path — the canceller already removed the entry).
    assert!(terminal(&push.frames()).is_none());
    assert!(done_rx.try_recv().is_err(), "cancelled search must not signal done");
}

#[tokio::test]
async fn cancel_during_run_drops_buffered_matches_and_no_terminal() {
    // Provider emits 3 hits (well under the batch threshold, so none is flushed
    // mid-walk) then flips the shared cancel token. The coordinator must DROP the
    // buffered remainder — not flush it — and send no terminal frame.
    let mut by_root = HashMap::new();
    by_root.insert(
        "file:///a".to_owned(),
        (0..10).map(|i| (format!("f{i}"), format!("f{i}.txt"))).collect(),
    );
    let provider: Arc<dyn IFsSearchProvider> = Arc::new(ScriptedProvider {
        by_root,
        cancel_after: Some(3),
    });
    let push = Arc::new(CapturePush::default());
    let (done_tx, mut done_rx) = unbounded_channel();

    run_search(
        provider,
        push.clone(),
        SearchJob {
            session: "sess".to_owned(),
            search_id: json!(3),
            roots: one_root("file:///a", "pe1"),
            matcher: NameMatcher::new("", MatchMode::Substring),
            budget: Budget::new(100),
            cancel: CancellationToken::new(),
        },
        done_tx,
    )
    .await;

    let frames = push.frames();
    // The 3 buffered (sub-threshold) hits were dropped, not pushed.
    assert!(
        collected_hits(&frames).is_empty(),
        "un-pushed buffer must be dropped on cancel, got {frames:?}"
    );
    assert!(terminal(&frames).is_none(), "cancelled mid-run must send no terminal");
    assert!(done_rx.try_recv().is_err(), "cancelled mid-run must not signal done");
}
