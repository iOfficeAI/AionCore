//! Wire tests: frame shapes and the error mapping.
//!
//! The mapping matters because it is what a client branches on: a stale
//! repository reference, a refusal to act on a conflict, and an engine
//! malfunction must not arrive looking the same.

use super::super::types::{
    FileRef, RepoRef, ScmActionFailure, ScmActionOutcome, ScmCapabilities, ScmHead, ScmRepository, ScmRepositoryState,
    ScmStatus,
};
use super::*;

/// Build a repository descriptor with the worktree fields set, to lock how the
/// new optional fields ride the wire (they are what a client branches on).
fn repository(is_worktree: bool, worktree_of: Option<&str>) -> ScmRepository {
    ScmRepository {
        repo_id: "scm:ws/feature".into(),
        provider_id: "git".into(),
        root: FileRef {
            pe_id: "ws".into(),
            relative_path: "feature".into(),
        },
        label: "feature".into(),
        pe_name: None,
        head: None,
        is_worktree,
        worktree_of: worktree_of.map(str::to_owned),
        capabilities: ScmCapabilities {
            staging: true,
            local_branches: true,
            history_graph: false,
            remote_ops: false,
        },
        state: ScmRepositoryState::Idle,
    }
}

#[test]
fn repository_frame_carries_worktree_ownership() {
    let value = serde_json::to_value(repository(true, Some("scm:ws/main"))).expect("serialize");
    assert_eq!(value["is_worktree"], true);
    assert_eq!(value["worktree_of"], "scm:ws/main");
    // A child repo carries no pe entry name; it is omitted, not null.
    assert!(value.get("pe_name").is_none(), "absent pe_name is omitted");
}

#[test]
fn repository_frame_omits_worktree_fields_by_default() {
    // A primary clone: `is_worktree` false and `worktree_of` None must both drop
    // off the wire so the common one-repo case stays byte-for-byte as before.
    let value = serde_json::to_value(repository(false, None)).expect("serialize");
    assert!(value.get("is_worktree").is_none(), "false is_worktree is omitted");
    assert!(value.get("worktree_of").is_none(), "None worktree_of is omitted");
}

/// Build the notification a status broadcast puts on the wire, so the assertions
/// exercise the exact serialization a client receives (see `ScmActor` refresh).
fn status_frame(head: Option<ScmHead>) -> serde_json::Value {
    let status = ScmStatus {
        repository: RepoRef {
            repo_id: "scm:pe1".into(),
        },
        resources: vec![],
        head,
        seq: 3,
        truncated: false,
        degraded: false,
    };
    notification(
        "scm/statusChanged",
        serde_json::to_value(&status).expect("serialize status"),
    )
}

#[test]
fn status_frame_carries_head_branch_name() {
    // The branch name reaches a status-only subscriber solely through this field;
    // a terminal `git checkout` produces exactly this shape.
    let frame = status_frame(Some(ScmHead {
        name: Some("feature/x".into()),
        detached: None,
    }));
    let params = &frame["params"];
    assert_eq!(params["head"]["name"], "feature/x");
    // `detached: None` is omitted, not serialized as null.
    assert!(params["head"].get("detached").is_none(), "absent detached is omitted");
}

#[test]
fn status_frame_carries_detached_head() {
    let frame = status_frame(Some(ScmHead {
        name: None,
        detached: Some(true),
    }));
    let head = &frame["params"]["head"];
    assert_eq!(head["detached"], true);
    assert!(head.get("name").is_none(), "a detached head serializes no name");
}

#[test]
fn status_frame_omits_head_when_unreadable() {
    // `None` head is a distinct wire state from a present-but-empty head; the
    // client must not see a null branch and mistake it for detached/unborn.
    let frame = status_frame(None);
    assert!(
        frame["params"].get("head").is_none(),
        "an unreadable head is omitted from the frame entirely"
    );
}

#[test]
fn frames_carry_the_jsonrpc_envelope() {
    let ok = success(Some(json!(7)), json!({ "statuses": [] }));
    assert_eq!(ok["jsonrpc"], "2.0");
    assert_eq!(ok["id"], 7);
    assert_eq!(ok["result"]["statuses"], json!([]));

    let note = notification("scm/statusChanged", json!({ "seq": 3 }));
    assert_eq!(note["jsonrpc"], "2.0");
    assert_eq!(note["method"], "scm/statusChanged");
    assert!(note.get("id").is_none(), "notifications carry no id");
}

#[test]
fn errors_omit_data_when_there_is_none() {
    let err = error(Some(json!(1)), CODE_INVALID_PARAMS, "invalid_params", Value::Null);
    assert_eq!(err["error"]["code"], CODE_INVALID_PARAMS);
    assert_eq!(err["error"]["message"], "invalid_params");
    assert!(
        err["error"].get("data").is_none(),
        "a null context is left out rather than sent as null"
    );
}

#[test]
fn a_scope_violation_is_distinct_from_not_found() {
    // A pe the connection may not reach must not look like a missing file: the
    // client should not go hunting for something it was never allowed to name.
    let (code, name, data) = map_error(&ScmError::OutOfScope {
        pe_id: "pe-other".to_owned(),
    });
    assert_eq!(code, CODE_OUT_OF_SCOPE);
    assert_eq!(name, "out_of_scope");
    assert_eq!(data["pe_id"], "pe-other");
    assert_ne!(code, CODE_RESOURCE_NOT_FOUND);
}

#[test]
fn a_malformed_request_is_distinct_from_an_unsupported_capability() {
    // "You sent no repository" and "this provider has no staging area" are
    // different problems with different fixes.
    let (code, name, data) = map_error(&ScmError::InvalidParams { what: "repository" });
    assert_eq!(code, CODE_INVALID_PARAMS);
    assert_eq!(name, "invalid_params");
    assert_eq!(data["param"], "repository");
    assert_ne!(code, CODE_CAPABILITY_UNSUPPORTED);
}

#[test]
fn a_missing_repository_maps_to_not_a_repository() {
    let (code, name, data) = map_error(&ScmError::NotARepository {
        root: "/tmp/plain".to_owned(),
    });
    assert_eq!(code, CODE_NOT_A_REPOSITORY);
    assert_eq!(name, "not_a_repository");
    assert_eq!(data["root"], "/tmp/plain");
}

#[test]
fn a_stale_repository_reference_is_a_lookup_failure_not_an_engine_failure() {
    // The client is holding an id for a repository that was released or never
    // existed; conflating this with an operation failure would send clients
    // hunting for a git problem that is not there.
    let (code, name, _) = map_error(&ScmError::UnknownRepository {
        repo_id: "scm:gone".to_owned(),
    });
    assert_eq!(code, CODE_RESOURCE_NOT_FOUND);
    assert_eq!(name, "resource_not_found");
}

#[test]
fn an_unsupported_capability_reports_which_one() {
    let (code, name, data) = map_error(&ScmError::CapabilityUnsupported { capability: "staging" });
    assert_eq!(code, CODE_CAPABILITY_UNSUPPORTED);
    assert_eq!(name, "capability_unsupported");
    assert_eq!(data["capability"], "staging");
}

#[test]
fn refusing_a_blocked_resource_is_not_an_operation_failure() {
    let (code, name, data) = map_error(&ScmError::OpaqueResource {
        path: "c.txt".to_owned(),
    });
    // A policy refusal (nothing ran) must be distinguishable from an action that
    // ran and broke: retrying the former can never succeed until the user resolves
    // the conflict, so a client needs to branch on the code, not parse a message.
    assert_eq!(code, CODE_RESOURCE_BLOCKED);
    assert_eq!(name, "resource_blocked");
    assert_ne!(code, CODE_SCM_OPERATION_FAILED, "a refusal is not an operation failure");
    assert_eq!(data["reason"], "conflicted");
    assert_eq!(data["path"], "c.txt");
}

#[test]
fn an_engine_failure_carries_its_operation_context() {
    let (code, name, data) = map_error(&ScmError::OperationFailed {
        context: "stage",
        message: "index locked".to_owned(),
    });
    assert_eq!(code, CODE_SCM_OPERATION_FAILED);
    assert_eq!(name, "scm_operation_failed");
    assert_eq!(data["context"], "stage");
}

#[test]
fn local_io_failure_maps_to_provider_unavailable() {
    let (code, name, _) = map_error(&ScmError::Io {
        path: "a.txt".to_owned(),
        message: "move to trash failed".to_owned(),
    });
    assert_eq!(code, CODE_PROVIDER_UNAVAILABLE);
    assert_eq!(name, "provider_unavailable");
}

#[test]
fn scm_codes_do_not_collide_with_the_shared_protocol_codes() {
    // The scm-specific codes sit apart from the codes the explorer link already
    // defines, so a client can tell feature-specific failures from shared ones.
    let shared = [
        CODE_INVALID_REQUEST,
        CODE_METHOD_NOT_FOUND,
        CODE_INVALID_PARAMS,
        CODE_OUT_OF_SCOPE,
        CODE_RESOURCE_NOT_FOUND,
        CODE_PROVIDER_UNAVAILABLE,
    ];
    for code in [
        CODE_NOT_A_REPOSITORY,
        CODE_SCM_OPERATION_FAILED,
        CODE_CAPABILITY_UNSUPPORTED,
    ] {
        assert!(!shared.contains(&code), "{code} must be unique to scm");
    }
}

#[test]
fn content_refs_parse_from_their_wire_spelling() {
    assert_eq!(parse_content_ref(&json!("working")), Some(ContentRef::Working));
    assert_eq!(parse_content_ref(&json!("committed")), Some(ContentRef::Committed));
    assert_eq!(parse_content_ref(&json!("staged")), Some(ContentRef::Staged));
}

#[test]
fn an_unknown_content_ref_is_rejected_rather_than_guessed() {
    // Defaulting an unrecognised anchor would silently diff the wrong pair of
    // versions, so it must be refused.
    assert_eq!(parse_content_ref(&json!("head")), None);
    assert_eq!(parse_content_ref(&json!("")), None);
    assert_eq!(parse_content_ref(&json!(null)), None);
    assert_eq!(parse_content_ref(&json!(3)), None);
}

#[test]
fn an_inbound_frame_decodes_with_optional_id_and_params() {
    let request: IncomingFrame = serde_json::from_value(json!({
        "jsonrpc": "2.0", "id": 4, "method": "scm/status", "params": { "repository": "scm:pe1" }
    }))
    .expect("request decodes");
    assert_eq!(request.id, Some(json!(4)));
    assert_eq!(request.method, "scm/status");

    let note: IncomingFrame = serde_json::from_value(json!({
        "jsonrpc": "2.0", "method": "scm/unsubscribe"
    }))
    .expect("notification decodes");
    assert!(note.id.is_none(), "no id on a notification");
    assert!(note.params.is_null(), "absent params default to null");
}

#[test]
fn an_all_success_action_serializes_to_the_old_shape() {
    // Back-compatibility: a client written before per-file results existed sees
    // exactly what it saw before, so nothing breaks by adding the field.
    let outcome = ScmActionOutcome::default();
    let value = serde_json::to_value(&outcome).expect("serializes");
    assert_eq!(value, json!({}), "no `failed` key at all when everything succeeded");
    assert!(outcome.is_complete());
}

#[test]
fn a_partial_failure_lists_only_the_failures() {
    let outcome = ScmActionOutcome {
        failed: vec![ScmActionFailure {
            file: FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "u2.txt".to_owned(),
            },
            reason: "move to trash failed: refused".to_owned(),
        }],
    };
    let value = serde_json::to_value(&outcome).expect("serializes");

    // Successful files are implied by absence — listing them would grow the frame
    // with information the client already has from the status it gets pushed.
    assert_eq!(value["failed"].as_array().expect("array").len(), 1);
    assert_eq!(value["failed"][0]["file"]["pe_id"], "pe1");
    assert_eq!(value["failed"][0]["file"]["relative_path"], "u2.txt");
    assert!(
        value["failed"][0]["reason"].as_str().expect("reason").contains("trash"),
        "the reason travels to the client"
    );
    assert!(!outcome.is_complete());
}
