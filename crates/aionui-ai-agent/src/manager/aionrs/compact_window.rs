//! Size the engine's compaction thresholds to the model's real context window.
//!
//! aionrs compacts before every provider call, driven by
//! [`aion_config::compact::CompactConfig`]. Its defaults are tuned for a 200K
//! window (`output_reserve` 20K, `autocompact_buffer` 13K), which puts the
//! autocompact trigger at ~167K — long past the limit of a smaller model.
//! AionCore knows the window configured on the provider row, so it re-scales
//! those thresholds when the agent is built; the engine then compacts at the
//! right point for the model actually in use instead of letting the provider
//! reject the request first.
//!
//! The engine also narrows the window on its own when a provider reports an
//! overflow, so this is the starting point rather than a hard cap.

use aion_config::compact::CompactConfig;

/// Window assumed when neither the provider row nor the environment names one.
/// Deliberately below the common 200K so a forgotten setting compacts slightly
/// early (cheap) instead of overflowing the provider (a failed turn).
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 128_000;

/// Resolve the model's real context window.
///
/// Resolution order (first hit wins):
/// 1. `provider_context_limit` — the `context_limit` stored on the provider row
///    for the model actually being used. This is the per-model source of truth;
///    a single process-wide env value is wrong for every other model in a
///    multi-model / multi-user deployment.
/// 2. `AIONUI_CONTEXT_WINDOW_TOKENS` — operator fallback when the provider row
///    carries no limit (clamped to > 0).
/// 3. [`DEFAULT_CONTEXT_WINDOW_TOKENS`] — conservative last resort.
pub fn resolve_context_window(provider_context_limit: Option<i64>) -> u64 {
    provider_context_limit
        .filter(|limit| *limit > 0)
        .map(|limit| limit as u64)
        .or_else(|| {
            std::env::var("AIONUI_CONTEXT_WINDOW_TOKENS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|window| *window > 0)
        })
        .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
}

/// Point the engine's own multi-level compaction at the model's real window.
///
/// The reserves are scaled to the resolved window while keeping aionrs'
/// documented ratios (10% / 6.5% / 1.5%), so a 200K window keeps aionrs'
/// defaults exactly and a smaller window triggers proportionally earlier.
///
/// [`COMPACT_MAX_OUTPUT_TOKENS`] additionally pins the trigger through
/// `autocompact_threshold_pct`. aionrs summarizes by re-sending the *whole*
/// conversation plus a fixed 20K completion request, so a trigger derived only
/// from `output_reserve`/`autocompact_buffer` can fire while the summary request
/// itself no longer fits: on a 131,077-token model that produced
/// `Requested token count exceeds the model's maximum context length` and
/// `Autocompact failed`. Pinning the percentage keeps
/// `trigger + 20K + headroom <= window`.
pub fn apply_engine_compact_window(compact: &mut CompactConfig, provider_context_limit: Option<i64>) {
    let window = resolve_context_window(provider_context_limit) as usize;
    compact.context_window = window;
    compact.output_reserve = scale_budget(window, 100, 1_024, 20_000);
    compact.autocompact_buffer = scale_budget(window, 65, 512, 13_000);
    compact.emergency_buffer = scale_budget(window, 15, 128, 3_000);
    compact.autocompact_threshold_pct = autocompact_threshold_pct(window);
}

/// Fixed completion budget aionrs requests for its summarization call
/// (`COMPACT_MAX_OUTPUT_TOKENS` in `aion-agent/src/compact/prompt.rs`).
const COMPACT_MAX_OUTPUT_TOKENS: usize = 20_000;

/// Percentage of the window at which autocompact should fire.
///
/// Leaves [`COMPACT_MAX_OUTPUT_TOKENS`] plus 5% of the window free, so the
/// summarization request stays inside the model's window even when the token
/// estimate is a little optimistic. `None` when the window cannot hold the
/// fixed completion budget at all — the engine default is then no worse.
fn autocompact_threshold_pct(window: usize) -> Option<u8> {
    let headroom = COMPACT_MAX_OUTPUT_TOKENS + window / 20;
    if window <= headroom {
        return None;
    }
    let pct = (window - headroom).saturating_mul(100) / window;
    Some(pct.clamp(20, 90) as u8)
}

/// `window * per_mille / 1000`, clamped to `[min, max]`.
fn scale_budget(window: usize, per_mille: usize, min: usize, max: usize) -> usize {
    (window.saturating_mul(per_mille) / 1_000).clamp(min, max)
}

#[cfg(test)]
#[path = "compact_window_test.rs"]
mod compact_window_test;
