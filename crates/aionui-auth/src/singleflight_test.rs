use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::task::yield_now;

use super::{RefreshCoalescer, RefreshError, RefreshedTokens};

#[tokio::test]
async fn coalesces_concurrent_refreshes_for_same_token() {
    let coalescer = RefreshCoalescer::new();
    let calls = AtomicUsize::new(0);

    let make = || async {
        coalescer
            .run("same-token", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                // Yield so sibling callers pile up on the in-flight cell before
                // this one finishes initializing it.
                yield_now().await;
                Ok(RefreshedTokens {
                    access: "access".into(),
                    refresh: "refresh".into(),
                })
            })
            .await
    };

    let (a, b, c, d) = tokio::join!(make(), make(), make(), make());
    for out in [a, b, c, d] {
        let tokens = out.unwrap();
        assert_eq!(tokens.access, "access");
        assert_eq!(tokens.refresh, "refresh");
    }
    // Four concurrent callers, exactly one execution.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_failures_are_coalesced_into_one_execution() {
    // Regression guard for the failure path: `get_or_try_init` left the cell
    // uninitialized on error, so concurrent waiters each re-ran the closure — a
    // failure stampede that defeats singleflight exactly when the backend is
    // already struggling. Storing the whole `Result` via `get_or_init` fixes it:
    // one execution, shared by all.
    let coalescer = RefreshCoalescer::new();
    let calls = AtomicUsize::new(0);

    let make = || async {
        coalescer
            .run("same-token", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                yield_now().await;
                Err::<RefreshedTokens, _>(RefreshError::Unauthorized("boom"))
            })
            .await
    };

    let (a, b, c, d) = tokio::join!(make(), make(), make(), make());
    for out in [a, b, c, d] {
        assert_eq!(out, Err(RefreshError::Unauthorized("boom")));
    }
    // All four coalesced onto a single failed execution.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn distinct_tokens_run_independently() {
    let coalescer = RefreshCoalescer::new();
    let calls = AtomicUsize::new(0);

    let run = |tok: &'static str| async {
        coalescer
            .run(tok, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(RefreshedTokens {
                    access: tok.into(),
                    refresh: tok.into(),
                })
            })
            .await
    };

    let (a, b) = tokio::join!(run("token-a"), run("token-b"));
    assert_eq!(a.unwrap().access, "token-a");
    assert_eq!(b.unwrap().access, "token-b");
    // Different tokens must not be coalesced together.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failure_is_not_cached_across_bursts_and_can_retry() {
    let coalescer = RefreshCoalescer::new();

    let first = coalescer
        .run("tok", || async { Err(RefreshError::Unauthorized("boom")) })
        .await;
    assert_eq!(first, Err(RefreshError::Unauthorized("boom")));

    // The failed attempt must not poison later refreshes of the same token: a
    // new burst re-executes from scratch.
    let second = coalescer
        .run("tok", || async {
            Ok(RefreshedTokens {
                access: "ok".into(),
                refresh: "ok".into(),
            })
        })
        .await;
    assert_eq!(second.unwrap().access, "ok");
}

#[tokio::test]
async fn inflight_map_is_empty_after_completion() {
    let coalescer = RefreshCoalescer::new();
    coalescer
        .run("tok", || async {
            Ok(RefreshedTokens {
                access: "a".into(),
                refresh: "r".into(),
            })
        })
        .await
        .unwrap();
    // The cell is cleaned up once the refresh completes.
    assert!(coalescer.inflight.is_empty());
}
