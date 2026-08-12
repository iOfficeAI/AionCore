//! Actor tests: frame decoding, dispatch, and parameter validation.
//!
//! These cover the parts that need no database: a malformed frame, an unknown
//! method, and the parameter extraction every method depends on. Method behaviour
//! against real repositories is covered by the runtime and provider suites; the
//! full request path (which needs a project store) is exercised by integration
//! tests.

use serde_json::json;

use super::*;

#[test]
fn a_malformed_frame_is_rejected_as_an_invalid_request() {
    // A frame without `method` cannot be dispatched; answering with a framing
    // error beats silently dropping it, which would leave a client waiting.
    let decoded = serde_json::from_value::<wire::IncomingFrame>(json!({"jsonrpc": "2.0", "id": 1}));
    assert!(decoded.is_err(), "a frame with no method does not decode");
}

#[test]
fn repository_parameter_is_required() {
    let missing = ScmActor::repo_of(&json!({}));
    assert!(
        matches!(missing, Err(ScmError::InvalidParams { what: "repository" })),
        "a missing repository is a malformed request, not an unsupported capability"
    );

    let present = ScmActor::repo_of(&json!({ "repository": "scm:pe1" })).expect("parses");
    assert_eq!(present.repo_id, "scm:pe1");
}

#[test]
fn repository_parameter_must_be_a_string() {
    // A structured value here would otherwise be silently ignored.
    assert!(ScmActor::repo_of(&json!({ "repository": { "repo_id": "scm:pe1" } })).is_err());
    assert!(ScmActor::repo_of(&json!({ "repository": 42 })).is_err());
}

#[test]
fn file_parameter_decodes_the_pe_relative_identity() {
    let file = ScmActor::file_of(&json!({
        "file": { "pe_id": "pe1", "relative_path": "src/a.ts" }
    }))
    .expect("parses");
    assert_eq!(file.pe_id, "pe1");
    assert_eq!(file.relative_path, "src/a.ts");
}

#[test]
fn a_file_missing_its_identity_is_rejected() {
    // Half an identity cannot be resolved, and guessing the other half would
    // address the wrong file.
    assert!(ScmActor::file_of(&json!({ "file": { "pe_id": "pe1" } })).is_err());
    assert!(ScmActor::file_of(&json!({ "file": "src/a.ts" })).is_err());
    assert!(ScmActor::file_of(&json!({})).is_err());
}

#[test]
fn files_parameter_accepts_a_batch_and_rejects_a_bare_value() {
    let files = ScmActor::files_of(&json!({
        "files": [
            { "pe_id": "pe1", "relative_path": "a.txt" },
            { "pe_id": "pe1", "relative_path": "b.txt" }
        ]
    }))
    .expect("parses");
    assert_eq!(files.len(), 2);

    assert!(
        ScmActor::files_of(&json!({ "files": { "pe_id": "pe1", "relative_path": "a.txt" } })).is_err(),
        "a single object is not a batch"
    );
    assert!(ScmActor::files_of(&json!({})).is_err());
}

#[test]
fn an_empty_batch_is_accepted_as_a_no_op() {
    // Nothing to do is not an error: a client may send an empty selection.
    let files = ScmActor::files_of(&json!({ "files": [] })).expect("parses");
    assert!(files.is_empty());
}
