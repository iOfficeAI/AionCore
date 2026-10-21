use super::*;
use crate::canonical::canonicalize;

const ROOT_URI: &str = if cfg!(windows) {
    "file:///C:/work/proj"
} else {
    "file:///work/proj"
};

fn root() -> Canonical {
    canonicalize(ROOT_URI).unwrap()
}

#[test]
fn resolves_normal_child_path() {
    let r = resolve_relative(&root(), "src/main.rs", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "src/main.rs");
    let (expected_uri, expected_path) = if cfg!(windows) {
        ("file:///C:/work/proj/src/main.rs", "C:/work/proj/src/main.rs")
    } else {
        ("file:///work/proj/src/main.rs", "/work/proj/src/main.rs")
    };
    assert_eq!(r.resource_uri, expected_uri);
    assert_eq!(r.absolute_path.unwrap(), PathBuf::from(expected_path));
}

#[test]
fn empty_relative_is_root_itself() {
    let r = resolve_relative(&root(), "", FileOp::Browse).unwrap();
    assert_eq!(r.relative_path, "");
    assert_eq!(r.resource_uri, ROOT_URI);
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

#[cfg(windows)]
#[test]
fn windows_drive_and_unc_paths_are_rejected() {
    for absolute in [r"C:\work\other", r"C:relative", r"\\server\share\other"] {
        let err = resolve_relative(&root(), absolute, FileOp::Read).unwrap_err();
        assert_eq!(err.code(), "invalid_relative_path", "path: {absolute}");
    }
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
