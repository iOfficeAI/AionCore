//! Trash-sink tests: the "discard never deletes outright" guarantee.
//!
//! These exercise the seam itself. The provider-level counterparts (a failing
//! sink leaves the file in place; discard routes through the sink rather than an
//! unlink) live in `git_provider_test.rs`, which can inject these doubles.

use std::path::Path;

use super::*;

#[test]
fn platform_trash_reports_failure_instead_of_deleting() {
    // A path whose parent does not exist cannot be resolved, so the platform
    // sink rejects it before touching the filesystem. This is the one failure
    // mode that is deterministic and fast on every platform (no desktop-shell
    // round trip), which is why it stands in for "the move cannot be performed".
    let missing_parent = Path::new("/this-path-does-not-exist-scm-test/nested/file.txt");

    let err = PlatformTrash
        .trash(missing_parent)
        .expect_err("an unresolvable path must be rejected");
    assert!(!err.is_empty(), "the failure must carry a reason");
}

#[test]
fn platform_trash_moves_a_real_file_out_of_the_work_tree() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let victim = tmp.path().join("discard-me.txt");
    std::fs::write(&victim, "content").expect("write victim");

    // Where it lands is the platform's business (per-volume trash on macOS,
    // `$XDG_DATA_HOME/Trash` on Linux, the recycle bin on Windows), so this only
    // asserts it left — the "not an unlink" half is pinned by the recording sink
    // in the provider tests.
    PlatformTrash.trash(&victim).expect("trashing a real file succeeds");
    assert!(!victim.exists(), "the file left its original location");
}
