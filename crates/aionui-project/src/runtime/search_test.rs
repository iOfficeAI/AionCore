//! Unit tests for the search provider primitives: `NameMatcher`, `Budget`,
//! `CancellationToken`. The `LocalFsProvider` walk itself is covered against a
//! real temp tree in `local_provider_test.rs`.

use super::*;

// ── NameMatcher ────────────────────────────────────────────────────────────

#[test]
fn matcher_empty_query_matches_everything() {
    let m = NameMatcher::new("", MatchMode::Substring);
    assert!(m.matches("anything.rs"));
    assert!(m.matches(""));
}

#[test]
fn matcher_substring_is_case_insensitive() {
    let m = NameMatcher::new("button", MatchMode::Substring);
    assert!(m.matches("Button.tsx"));
    assert!(m.matches("iconButton.ts"));
    assert!(!m.matches("Widget.tsx"));
}

#[test]
fn matcher_substring_requires_contiguous() {
    let m = NameMatcher::new("btn", MatchMode::Substring);
    // "btn" is not a contiguous run inside "button".
    assert!(!m.matches("Button.tsx"));
    assert!(m.matches("my_btn.tsx"));
}

#[test]
fn matcher_subsequence_allows_gaps() {
    let m = NameMatcher::new("btn", MatchMode::Subsequence);
    // b..t..n appear in order within "Button".
    assert!(m.matches("Button.tsx"));
    assert!(!m.matches("nbt.rs")); // wrong order
}

// ── Budget ─────────────────────────────────────────────────────────────────

#[test]
fn budget_takes_up_to_limit_then_records_cap() {
    let b = Budget::new(2);
    assert!(b.try_take());
    assert!(b.try_take());
    assert!(!b.limit_reached()); // exactly consumed, no over-attempt yet
    assert!(!b.try_take()); // third attempt fails
    assert!(b.limit_reached());
}

#[test]
fn budget_zero_limit_immediately_caps() {
    let b = Budget::new(0);
    assert!(!b.try_take());
    assert!(b.limit_reached());
}

#[test]
fn budget_is_shared_across_clones() {
    let a = Budget::new(1);
    let b = a.clone();
    assert!(a.try_take());
    // The clone draws from the same pool — no slots left.
    assert!(!b.try_take());
    assert!(a.limit_reached());
}

// ── CancellationToken ────────────────────────────────────────────────────────

#[test]
fn token_starts_uncancelled_and_flips_once_cancelled() {
    let t = CancellationToken::new();
    assert!(!t.is_cancelled());
    t.cancel();
    assert!(t.is_cancelled());
}

#[test]
fn token_cancel_is_visible_through_clones() {
    let a = CancellationToken::new();
    let b = a.clone();
    a.cancel();
    assert!(b.is_cancelled());
}
