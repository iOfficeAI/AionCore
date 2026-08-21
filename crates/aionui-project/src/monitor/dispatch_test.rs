//! Unit tests for the pure transfer helpers (`fs/copy` / `fs/move` name
//! resolution). Behavioral, cross-layer coverage of the handlers themselves
//! lives in `actor_test.rs`, driven through `dispatch_frame`.

use super::{candidate_name, join_rel, last_segment, parent_uri, split_ext, uri_within_or_equal};

#[test]
fn candidate_zero_is_the_original_name() {
    assert_eq!(candidate_name("report.txt", 0, false), "report.txt");
    assert_eq!(candidate_name("src", 0, true), "src");
}

#[test]
fn candidate_files_keep_extension_and_count_from_copy() {
    // macOS-style sequence: "copy", then "copy 2", "copy 3", …
    assert_eq!(candidate_name("report.txt", 1, false), "report copy.txt");
    assert_eq!(candidate_name("report.txt", 2, false), "report copy 2.txt");
    assert_eq!(candidate_name("report.txt", 3, false), "report copy 3.txt");
}

#[test]
fn candidate_dirs_take_suffix_wholesale() {
    assert_eq!(candidate_name("assets", 1, true), "assets copy");
    assert_eq!(candidate_name("assets", 2, true), "assets copy 2");
    // A dot in a directory name is not an extension.
    assert_eq!(candidate_name("my.config", 1, true), "my.config copy");
}

#[test]
fn candidate_dotfiles_have_no_extension() {
    // Leading dot is not an extension separator.
    assert_eq!(candidate_name(".gitignore", 1, false), ".gitignore copy");
    assert_eq!(candidate_name(".env", 2, false), ".env copy 2");
}

#[test]
fn candidate_multi_dot_splits_at_last_dot() {
    assert_eq!(candidate_name("archive.tar.gz", 1, false), "archive.tar copy.gz");
}

#[test]
fn split_ext_boundaries() {
    assert_eq!(split_ext("a.txt"), ("a", ".txt"));
    assert_eq!(split_ext("noext"), ("noext", ""));
    assert_eq!(split_ext(".hidden"), (".hidden", ""));
    assert_eq!(split_ext("a.b.c"), ("a.b", ".c"));
}

#[test]
fn join_rel_handles_empty_root() {
    assert_eq!(join_rel("", "a.txt"), "a.txt");
    assert_eq!(join_rel("src", "a.txt"), "src/a.txt");
    assert_eq!(join_rel("src/util", "a.txt"), "src/util/a.txt");
}

#[test]
fn last_segment_is_the_entry_name() {
    assert_eq!(last_segment("a.txt"), "a.txt");
    assert_eq!(last_segment("src/util/a.txt"), "a.txt");
}

#[test]
fn parent_uri_strips_last_segment() {
    assert_eq!(parent_uri("file:///a/b/c").as_deref(), Some("file:///a/b"));
    assert_eq!(parent_uri("noseparator"), None);
}

#[test]
fn within_or_equal_guards_descendant_transfer() {
    let a = "file:///root/a";
    // A dir into itself, or into a descendant → blocked.
    assert!(uri_within_or_equal(a, a));
    assert!(uri_within_or_equal("file:///root/a/b", a));
    // A sibling with a shared name prefix is NOT a descendant.
    assert!(!uri_within_or_equal("file:///root/abc", a));
    // The reverse (ancestor into descendant's parent) is fine.
    assert!(!uri_within_or_equal("file:///root", a));
}
