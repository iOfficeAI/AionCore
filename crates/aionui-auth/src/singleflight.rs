//! Refresh-storm protection: coalesce concurrent refreshes.
//!
//! When an access token expires, every in-flight request from the same client
//! can fail near-simultaneously and trigger a burst of refresh calls carrying
//! the *same* refresh token. Without coordination each call would hit the
//! database, re-sign a token pair, and (because refresh is sliding) bump the
//! session — a self-inflicted stampede on the hottest auth path.
//!
//! [`RefreshCoalescer`] collapses that burst: concurrent refreshes presenting
//! the same refresh token share a single execution and all receive the same
//! result. A different token runs independently.
//!
//! Design notes:
//! - **Key = the full refresh token, not a hash.** A truncated/hashed key could
//!   collide, and a collision here is a security defect: two *different* tokens
//!   would be coalesced and one caller would receive the other's freshly minted
//!   credentials. The in-flight map only holds entries for the brief window a
//!   refresh is running, so the memory cost of full-string keys is negligible.
//! - **Success and failure are both coalesced.** The cell stores the whole
//!   `Result`, so a single execution's outcome — `Ok` or `Err` — is shared by
//!   every caller that piled onto it. A refresh failing under a storm therefore
//!   hits the backend once, not once per coalesced caller. (This is why the
//!   error type is `Clone`: the shared outcome is cloned out to each caller.)
//! - **Failure is not cached across bursts.** The in-flight entry is removed as
//!   soon as the execution completes, so a later refresh with the same token
//!   starts a fresh execution rather than inheriting a stale failure.
//! - **No lock is held across the await.** The DashMap guard is dropped before
//!   awaiting the refresh future, so shard locks never span suspension points.

use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::OnceCell;

/// Freshly minted token pair produced by a coalesced refresh.
///
/// Internal to the crate: only the refresh route consumes it. The refresh token
/// is deliberately not exposed beyond the auth crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshedTokens {
    /// New short-lived access token.
    pub(crate) access: String,
    /// New long-lived refresh token (sliding).
    pub(crate) refresh: String,
}

/// Why a coalesced refresh failed.
///
/// Crate-owned and `Clone` so a single failed execution can be shared by every
/// caller that coalesced onto it — the whole point of failure-path singleflight.
/// It carries only a static reason string (no HTTP status, no `ApiError`), which
/// keeps this module free of any API-boundary coupling; the refresh route maps
/// each variant to the appropriate `ApiError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RefreshError {
    /// Refresh token missing, invalid, expired, wrong type, or its session was
    /// revoked. Maps to `401 Unauthorized`.
    Unauthorized(&'static str),
    /// aionpro mode but the authenticated subject is not an aionpro user.
    UserContextRequired,
    /// Backend failure — user lookup or token signing. Maps to `500`.
    Internal(&'static str),
}

/// The shared outcome of one coalesced refresh execution: a freshly minted
/// token pair, or the reason it failed. Stored whole in the in-flight cell so
/// that success and failure alike are shared by every coalesced caller.
type RefreshOutcome = Result<RefreshedTokens, RefreshError>;

/// Coalesces concurrent refreshes that present the same refresh token into a
/// single execution.
///
/// Cheap to clone (an `Arc` handle) and safe to share across the router state.
#[derive(Clone, Default)]
pub struct RefreshCoalescer {
    /// Refresh token -> the shared cell holding that refresh's single result
    /// (whether it succeeded or failed).
    inflight: Arc<DashMap<String, Arc<OnceCell<RefreshOutcome>>>>,
}

impl RefreshCoalescer {
    /// Create an empty coalescer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `refresh` for `token`, coalescing with any concurrent call for the
    /// same token.
    ///
    /// The first caller for a given token executes `refresh`; callers that
    /// arrive while it is in flight await the *same* outcome — success or
    /// failure — instead of running their own. The outcome is not cached beyond
    /// the burst: the in-flight entry is dropped once the execution completes,
    /// so a later call re-executes rather than inheriting a stale outcome.
    pub(crate) async fn run<F, Fut>(&self, token: &str, refresh: F) -> Result<RefreshedTokens, RefreshError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<RefreshedTokens, RefreshError>>,
    {
        // Obtain (or create) the shared cell for this token. The DashMap guard
        // is scoped to this block and dropped before the await below, so no
        // shard lock is ever held across the refresh future.
        let cell = {
            let entry = self
                .inflight
                .entry(token.to_owned())
                .or_insert_with(|| Arc::new(OnceCell::new()));
            Arc::clone(&entry)
        };

        // `get_or_init` stores the whole `Result` as the cell value, so the
        // first caller executes `refresh` exactly once and every coalesced
        // caller clones out the same outcome — including the error case, which
        // `get_or_try_init` would instead leave uninitialized, letting each
        // waiter re-run the closure and defeating singleflight on failures.
        let outcome = cell.get_or_init(refresh).await.clone();

        // Drop our cell from the map so the token does not accumulate entries
        // and the next burst re-executes instead of reusing this outcome.
        // Guard with a pointer check: a later caller may have installed a fresh
        // cell after ours completed, and that one must not be removed.
        self.inflight
            .remove_if(token, |_, existing| Arc::ptr_eq(existing, &cell));

        outcome
    }
}

#[cfg(test)]
#[path = "singleflight_test.rs"]
mod singleflight_test;
