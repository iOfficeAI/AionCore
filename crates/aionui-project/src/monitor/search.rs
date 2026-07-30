//! Filename-search orchestration — the layer above the provider that turns one
//! `fs/search` request into a merged, budgeted, cancellable stream.
//!
//! The actor resolves every root atomically, then hands off to [`run_search`],
//! spawned as its own task so the walks never block the actor event loop (which
//! must stay responsive to `fs/searchCancel` and superseding searches). The
//! coordinator drives every root's [`IFsSearchProvider::search_names`]
//! concurrently, all sharing one [`Budget`] + one [`CancellationToken`]; each
//! root's hits flow through a per-root [`RootSink`] that stamps the folder's
//! `pe_id` and merges into one [`MatchCollector`], batched onto `fs/searchMatch`.
//! When every root finishes (or the budget is spent) the terminal response is
//! sent — unless the search was cancelled/superseded, in which case no terminal
//! frame goes out (per protocol.md).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::{Budget, CancellationToken, IFsSearchProvider, NameMatcher, SearchSink};

use super::port::FsWirePush;
use super::wire::{self, SearchHit};

/// Default global hit budget when the request omits `limit`.
pub(super) const DEFAULT_SEARCH_LIMIT: usize = 1000;

/// How many hits accumulate before a `fs/searchMatch` batch is flushed. Bounds
/// frame count without holding hits back long enough to hurt streaming feel.
const SEARCH_MATCH_BATCH: usize = 64;

/// One in-flight search tracked by the actor, so it can be cancelled explicitly
/// (`fs/searchCancel`) or superseded by a newer search on the same connection.
#[derive(Debug, Clone)]
pub(super) struct ActiveSearch {
    /// The originating request `id`, echoed as `search_id`; used to match an
    /// incoming `fs/searchCancel`.
    pub(super) search_id: Value,
    /// Shared cancel signal cascaded to every root walk of this search.
    pub(super) cancel: CancellationToken,
}

/// A resolved search root: the concrete provider URI to walk plus the `pe_id`
/// the orchestration stamps onto every hit found under it.
pub(super) struct SearchRoot {
    pub(super) root_uri: String,
    pub(super) pe_id: String,
}

/// One search's coordinator inputs (beyond the shared provider/push handles):
/// the connection, the correlating `search_id`, the resolved roots, and the
/// shared matcher/budget/cancel that gate every root walk.
pub(super) struct SearchJob {
    pub(super) session: String,
    pub(super) search_id: Value,
    pub(super) roots: Vec<SearchRoot>,
    pub(super) matcher: NameMatcher,
    pub(super) budget: Budget,
    pub(super) cancel: CancellationToken,
}

/// Signalled by a coordinator to the actor when a search finishes *naturally*
/// (not cancelled/superseded), so the actor can clear its `active_searches`
/// entry — guarded on `search_id` so a superseding search is not clobbered.
#[derive(Debug, Clone)]
pub(super) struct SearchDone {
    pub(super) session: String,
    pub(super) search_id: Value,
}

/// Merges every root's hits into one `fs/searchMatch` stream, batched, and emits
/// the terminal response. Owns the outbound push handle + the search identity.
struct MatchCollector {
    push: Arc<dyn FsWirePush>,
    session: String,
    search_id: Value,
    buf: Mutex<Vec<SearchHit>>,
    total: AtomicUsize,
}

impl MatchCollector {
    fn new(push: Arc<dyn FsWirePush>, session: String, search_id: Value) -> Self {
        Self {
            push,
            session,
            search_id,
            buf: Mutex::new(Vec::new()),
            total: AtomicUsize::new(0),
        }
    }

    /// Buffer one hit; flush a full batch to the wire when the threshold is hit.
    /// Sync — called from the blocking walk thread through [`RootSink::emit`].
    fn push_hit(&self, hit: SearchHit) {
        self.total.fetch_add(1, Ordering::Relaxed);
        let batch = {
            let mut buf = self.buf.lock().expect("search buffer mutex poisoned");
            buf.push(hit);
            if buf.len() >= SEARCH_MATCH_BATCH {
                Some(std::mem::take(&mut *buf))
            } else {
                None
            }
        };
        if let Some(batch) = batch {
            self.emit_batch(batch);
        }
    }

    /// Flush any buffered hits (called once every root walk has finished).
    fn flush(&self) {
        let batch = {
            let mut buf = self.buf.lock().expect("search buffer mutex poisoned");
            if buf.is_empty() {
                return;
            }
            std::mem::take(&mut *buf)
        };
        self.emit_batch(batch);
    }

    /// Push one `fs/searchMatch` notification carrying `batch`.
    fn emit_batch(&self, batch: Vec<SearchHit>) {
        let params = wire::search_match_params(&self.search_id, &batch);
        self.push
            .push(&self.session, wire::notification("fs/searchMatch", params));
    }

    fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Push the terminal `fs/search` response for the originating request `id`.
    fn emit_terminal(&self, limit_reached: bool) {
        let result = wire::search_result(limit_reached, self.total());
        self.push
            .push(&self.session, wire::success(Some(self.search_id.clone()), result));
    }
}

/// Per-root hit outlet: stamps the root's `pe_id` and forwards into the shared
/// collector. `emit` is sync (called from the blocking walk), so it hands the
/// hit to the async collector via `blocking_lock` on its buffer mutex.
struct RootSink {
    pe_id: String,
    collector: Arc<MatchCollector>,
}

impl SearchSink for RootSink {
    fn emit(&self, relative_path: String, name: String) {
        self.collector.push_hit(SearchHit {
            pe_id: self.pe_id.clone(),
            relative_path,
            name,
        });
    }
}

/// Drive one search to completion: walk every root concurrently under a shared
/// budget + cancel, streaming batched `fs/searchMatch`, then the terminal frame
/// (unless cancelled/superseded). Spawned by the actor so it never blocks the
/// event loop. `total` counts hits emitted; `limit_reached` reflects the budget.
pub(super) async fn run_search(
    provider: Arc<dyn IFsSearchProvider>,
    push: Arc<dyn FsWirePush>,
    job: SearchJob,
    done: UnboundedSender<SearchDone>,
) {
    let SearchJob {
        session,
        search_id,
        roots,
        matcher,
        budget,
        cancel,
    } = job;
    let collector = Arc::new(MatchCollector::new(push, session.clone(), search_id.clone()));
    let matcher = Arc::new(matcher);

    // Each root walks concurrently; the walk itself is a `spawn_blocking` inside
    // `search_names`, so N roots occupy N blocking threads in parallel.
    let mut handles = Vec::with_capacity(roots.len());
    for root in roots {
        let provider = Arc::clone(&provider);
        let matcher = Arc::clone(&matcher);
        let budget = budget.clone();
        let cancel = cancel.clone();
        let sink: Arc<dyn SearchSink> = Arc::new(RootSink {
            pe_id: root.pe_id,
            collector: Arc::clone(&collector),
        });
        let root_uri = root.root_uri;
        handles.push(tokio::spawn(async move {
            provider
                .search_names(&root_uri, &matcher, &sink, &budget, &cancel)
                .await
        }));
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            // A single root failing (unreadable, provider error) must not sink
            // the others or the terminal frame — surface and continue.
            Ok(Err(err)) => tracing::warn!(error = %err, "fs search: root walk failed"),
            Err(join) => tracing::warn!(error = %join, "fs search: root task join failed"),
        }
    }

    // Contract (protocol.md): on cancel/supersede, DROP the un-pushed buffer and
    // send neither more matches nor a terminal frame. Checked BEFORE `flush` so
    // the buffered remainder is discarded rather than pushed — do not rely on the
    // frontend's stale-search_id guard to swallow a late batch.
    if cancel.is_cancelled() {
        tracing::debug!(session = %session, "fs search cancelled/superseded: dropping buffered matches, no terminal frame");
        return;
    }

    collector.flush();
    let limit_reached = budget.limit_reached();
    tracing::info!(session = %session, total = collector.total(), limit_reached, "fs search complete");

    // Signal completion to the actor BEFORE the terminal frame: the actor clears
    // its active-search entry first, so a client that reacts to the terminal by
    // sending fs/searchCancel cannot race a not-yet-cleared entry. The actor
    // drops the signal if a superseding search already replaced this id (its id
    // differs), so the ordering does not weaken the anti-clobber guard.
    //
    // KNOWN, HARMLESS RESIDUAL RACE: `done` only enqueues onto the actor's mpsc;
    // the actor processes it in a separate `select!` arm with no ordering barrier
    // against the wire, so there is still a window where the terminal has been
    // pushed but `active_searches` is not yet cleared. Enqueuing before the
    // terminal only narrows this window; it does not close it. This is
    // deliberately accepted (not covered by a scheduling test) because it is
    // observably harmless: (1) `search_id` is monotonic per connection and a
    // completed id is never reused, so a stale entry can never clobber a newer
    // search; (2) `fs/searchCancel` is a notification with no reply, so a client
    // hitting the window sees no erroneous response; (3) cancelling an
    // already-finished search is a no-op and `active_searches` removal is
    // idempotent.
    let _ = done.send(SearchDone { session, search_id });
    collector.emit_terminal(limit_reached);
}

#[cfg(test)]
#[path = "search_test.rs"]
mod search_test;
