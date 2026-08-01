//! Noise-file predicate — OS-junk and VCS-internal entries hidden from *every*
//! listing so the tree, search, and incremental-delta pipelines stay consistent.
//!
//! The same [`should_hide`] gate is applied at all three entry points that turn
//! filesystem names into tree/search results: the provider's `read_dir` (tree
//! baseline + rescan), the search `ignore` walk (`filter_entry`), and the
//! precise-event apply path (per-name `stat`). Hidden means the entry is never
//! emitted at all (no wire change; distinct from the protocol's `excluded`
//! marker, which is a visible-but-not-expanded directory).
//!
//! v1 uses a hardcoded default set (mirrors how ripgrep ships built-in
//! defaults). A user-configurable overlay can layer on later without changing
//! call sites — they only depend on [`should_hide`].

/// Names hidden by exact match (compared case-insensitively). Covers VCS
/// internals plus the common macOS / Windows / Linux junk files.
const HIDDEN_EXACT: &[&str] = &[
    // VCS internals — hiding `.git` also stops its objects/refs leaking into
    // search results (the `ignore` crate does not skip `.git` on its own).
    ".git",
    // macOS
    ".ds_store",
    ".spotlight-v100",
    ".trashes",
    ".fseventsd",
    ".appledouble",
    ".apdisk",
    // Windows
    "thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    "$recycle.bin",
    // Linux
    ".directory",
];

/// Names hidden by (case-insensitive) prefix.
const HIDDEN_PREFIX: &[&str] = &[
    "._",      // macOS AppleDouble resource-fork sidecars (`._file`)
    ".trash-", // Linux trash directories (`.Trash-1000`)
];

/// Whether `name` is a noise entry to hide entirely from listings.
///
/// Case-insensitive: Windows and macOS fold case, and a case-sensitive Linux
/// filesystem can still carry these names (synced/mounted volumes), so folding
/// everywhere is the safe superset. `name` is a single path segment (file name),
/// never a full path.
pub(crate) fn should_hide(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HIDDEN_EXACT.contains(&lower.as_str()) || HIDDEN_PREFIX.iter().any(|p| lower.starts_with(p))
}

#[cfg(test)]
#[path = "noise_test.rs"]
mod noise_test;
