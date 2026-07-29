use super::*;
use crate::canonical::canonicalize;

fn root() -> Canonical {
    canonicalize("file:///work/proj").unwrap()
}

#[test]
fn resolves_normal_child_path() {
    let r = resolve_relative(&root(), "src/main.rs", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "src/main.rs");
    assert_eq!(r.resource_uri, "file:///work/proj/src/main.rs");
    assert_eq!(r.absolute_path.unwrap().to_string_lossy(), "/work/proj/src/main.rs");
}

#[test]
fn empty_relative_is_root_itself() {
    let r = resolve_relative(&root(), "", FileOp::Browse).unwrap();
    assert_eq!(r.relative_path, "");
    assert_eq!(r.resource_uri, "file:///work/proj");
}

#[test]
fn strips_single_dot_and_trailing_slash() {
    let r = resolve_relative(&root(), "./a/b/", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "a/b");
}

#[test]
fn interior_dot_dot_that_stays_inside_is_ok() {
    let r = resolve_relative(&root(), "a/../b", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "b");
}

#[test]
fn absolute_path_is_rejected() {
    let err = resolve_relative(&root(), "/etc/passwd", FileOp::Read).unwrap_err();
    assert_eq!(err.code(), "invalid_relative_path");
}

#[test]
fn dot_dot_escape_is_rejected() {
    assert_eq!(
        resolve_relative(&root(), "..", FileOp::Read).unwrap_err().code(),
        "invalid_relative_path"
    );
    assert_eq!(
        resolve_relative(&root(), "a/../../b", FileOp::Read).unwrap_err().code(),
        "invalid_relative_path"
    );
}
