use serde_json::{Value, json};

use crate::runtime::{Change, DeltaBatch, EntryFact, FsError, Kind, Snapshot};
use crate::types::ProjectError;

use super::*;

fn fact(kind: Kind) -> EntryFact {
    EntryFact {
        kind,
        inode: 7,
        symlink_target: None,
        // Non-`None` on purpose: the wire-projection tests below assert the
        // serialized entry carries no mtime, which only proves anything if the
        // fact going in had one.
        mtime_ms: Some(1_700_000_000_000),
    }
}

// ── IncomingFrame ─────────────────────────────────────────────────────────

#[test]
fn incoming_request_parses_id_method_params() {
    let v = json!({"jsonrpc":"2.0","id":3,"method":"fs/mkdir","params":{"dir":{"pe_id":"pe1","relative_path":"a"}}});
    let frame: IncomingFrame = serde_json::from_value(v).unwrap();
    assert_eq!(frame.id, Some(json!(3)));
    assert_eq!(frame.method, "fs/mkdir");
    assert_eq!(frame.params["dir"]["pe_id"], "pe1");
}

#[test]
fn incoming_notification_has_no_id() {
    let v = json!({"jsonrpc":"2.0","method":"fs/unsubscribe","params":{"targets":[]}});
    let frame: IncomingFrame = serde_json::from_value(v).unwrap();
    assert_eq!(frame.id, None);
    assert_eq!(frame.method, "fs/unsubscribe");
}

#[test]
fn incoming_params_default_null_when_absent() {
    let v = json!({"jsonrpc":"2.0","id":0,"method":"initialize"});
    let frame: IncomingFrame = serde_json::from_value(v).unwrap();
    assert!(frame.params.is_null());
}

#[test]
fn incoming_missing_method_is_error() {
    let v = json!({"jsonrpc":"2.0","id":1,"params":{}});
    assert!(serde_json::from_value::<IncomingFrame>(v).is_err());
}

// ── params ──────────────────────────────────────────────────────────────

#[test]
fn subscribe_params_parse_targets() {
    let v = json!({"targets":[{"pe_id":"pe1","relative_path":""},{"pe_id":"pe2","relative_path":"src"}]});
    let p: SubscribeParams = serde_json::from_value(v).unwrap();
    assert_eq!(p.targets.len(), 2);
    assert_eq!(p.targets[1].relative_path, "src");
}

#[test]
fn remove_params_recursive_defaults_false() {
    let p: RemoveParams = serde_json::from_value(json!({"target":{"pe_id":"pe1","relative_path":"d"}})).unwrap();
    assert!(!p.recursive);
}

// ── entry / kind ──────────────────────────────────────────────────────────

#[test]
fn wire_kind_serializes_lowercase() {
    assert_eq!(serde_json::to_value(WireKind::File).unwrap(), json!("file"));
    assert_eq!(serde_json::to_value(WireKind::Dir).unwrap(), json!("dir"));
    assert_eq!(serde_json::to_value(WireKind::Symlink).unwrap(), json!("symlink"));
}

#[test]
fn wire_entry_from_fact_maps_kind_and_drops_inode() {
    let e = WireEntry::from_fact("a.txt", &fact(Kind::File));
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v, json!({"name":"a.txt","kind":"file"}));
    // inode is internal and must never appear on the wire.
    assert!(v.get("inode").is_none());
    // Nor may mtime: it exists only to detect modification, and a subscriber that
    // fed it back as a write precondition would defeat conflict detection.
    assert!(v.get("mtime_ms").is_none());
}

#[test]
fn wire_entry_symlink_includes_target() {
    let ef = EntryFact {
        kind: Kind::Symlink,
        inode: 1,
        symlink_target: Some("target".to_owned()),
        mtime_ms: Some(1_700_000_000_000),
    };
    let v = serde_json::to_value(WireEntry::from_fact("link", &ef)).unwrap();
    assert_eq!(v["kind"], "symlink");
    assert_eq!(v["symlink_target"], "target");
}

// ── snapshot / delta params ───────────────────────────────────────────────

fn target() -> ResourceRef {
    ResourceRef {
        pe_id: "pe1".to_owned(),
        relative_path: "src".to_owned(),
    }
}

#[test]
fn snapshot_params_carries_target_and_entries() {
    let snap = Snapshot {
        canonical: "file:///x".to_owned(),
        entries: vec![
            ("main.ts".to_owned(), fact(Kind::File)),
            ("sub".to_owned(), fact(Kind::Dir)),
        ],
    };
    let v = snapshot_params(&snap, &target());
    assert_eq!(v["target"], json!({"pe_id":"pe1","relative_path":"src"}));
    assert_eq!(v["entries"][0], json!({"name":"main.ts","kind":"file"}));
    assert_eq!(v["entries"][1], json!({"name":"sub","kind":"dir"}));
    // canonical must never leak to the wire.
    assert!(v.get("canonical").is_none());
}

#[test]
fn delta_params_tags_each_change_op() {
    let delta = DeltaBatch {
        canonical: "file:///x".to_owned(),
        changes: vec![
            Change::Added {
                name: "new.ts".to_owned(),
                kind: Kind::File,
            },
            Change::Removed {
                name: "old.ts".to_owned(),
            },
            Change::Renamed {
                from: "a".to_owned(),
                to: "b".to_owned(),
            },
            Change::Modified {
                name: "edited.ts".to_owned(),
            },
        ],
    };
    let v = delta_params(&delta, &target());
    assert_eq!(v["target"]["pe_id"], "pe1");
    assert_eq!(v["changes"][0], json!({"op":"added","name":"new.ts","kind":"file"}));
    assert_eq!(v["changes"][1], json!({"op":"removed","name":"old.ts"}));
    assert_eq!(v["changes"][2], json!({"op":"renamed","from":"a","to":"b"}));
    // Exact equality, not a field probe: it pins that `modified` carries *only* a
    // name. A timestamp here would be the value an external write just left, and a
    // subscriber replaying it as a write precondition would make its own save pass
    // conflict detection against the edit this op exists to warn about.
    assert_eq!(v["changes"][3], json!({"op":"modified","name":"edited.ts"}));
}

// ── frame builders ────────────────────────────────────────────────────────

#[test]
fn success_frame_shape() {
    let v = success(Some(json!(5)), json!({"ok":true}));
    assert_eq!(v, json!({"jsonrpc":"2.0","id":5,"result":{"ok":true}}));
}

#[test]
fn error_frame_includes_data_when_present() {
    let v = error(
        Some(json!(6)),
        CODE_RESOURCE_NOT_FOUND,
        "resource_not_found",
        json!({"pe_id":"pe1"}),
    );
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 6);
    assert_eq!(v["error"]["code"], -32002);
    assert_eq!(v["error"]["message"], "resource_not_found");
    assert_eq!(v["error"]["data"]["pe_id"], "pe1");
}

#[test]
fn error_frame_omits_data_when_null() {
    let v = error(None, CODE_INVALID_REQUEST, "invalid_request", Value::Null);
    assert_eq!(v["error"]["code"], -32600);
    assert!(v["error"].get("data").is_none());
    assert!(v["id"].is_null());
}

#[test]
fn notification_frame_has_no_id() {
    let v = notification("fs/delta", json!({"target":{}}));
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "fs/delta");
    assert!(v.get("id").is_none());
}

// ── filename search params / builders ─────────────────────────────────────

#[test]
fn search_params_parse_roots_query_and_default_limit() {
    let v = json!({"roots":[{"pe_id":"pe1","relative_path":""}],"query":"button","limit":200});
    let p: SearchParams = serde_json::from_value(v).unwrap();
    assert_eq!(p.roots.len(), 1);
    assert_eq!(p.query, "button");
    assert_eq!(p.limit, Some(200));

    // limit is optional (server picks a default when omitted).
    let p2: SearchParams = serde_json::from_value(json!({"roots":[],"query":""})).unwrap();
    assert_eq!(p2.limit, None);
}

#[test]
fn search_cancel_params_echo_search_id_value() {
    let p: SearchCancelParams = serde_json::from_value(json!({"search_id":8})).unwrap();
    assert_eq!(p.search_id, json!(8));
}

#[test]
fn search_hit_serializes_project_identity() {
    let hit = SearchHit {
        pe_id: "pe1".to_owned(),
        relative_path: "src/components/Button.tsx".to_owned(),
        name: "Button.tsx".to_owned(),
    };
    let v = serde_json::to_value(&hit).unwrap();
    assert_eq!(
        v,
        json!({"pe_id":"pe1","relative_path":"src/components/Button.tsx","name":"Button.tsx"})
    );
}

#[test]
fn search_match_params_batches_hits_under_search_id() {
    let hits = vec![SearchHit {
        pe_id: "pe2".to_owned(),
        relative_path: "widgets/iconButton.ts".to_owned(),
        name: "iconButton.ts".to_owned(),
    }];
    let v = search_match_params(&json!(7), &hits);
    assert_eq!(v["search_id"], 7);
    assert_eq!(v["matches"][0]["pe_id"], "pe2");
    assert_eq!(v["matches"][0]["name"], "iconButton.ts");
}

#[test]
fn search_result_carries_limit_reached_and_total() {
    assert_eq!(search_result(false, 2), json!({"limit_reached":false,"total":2}));
    assert_eq!(search_result(true, 200), json!({"limit_reached":true,"total":200}));
}

// ── error mapping ─────────────────────────────────────────────────────────

#[test]
fn project_error_maps_to_protocol_codes() {
    let cases: Vec<(ProjectError, i64, &str)> = vec![
        (
            ProjectError::ProjectExplorerNotFound { pe_id: "pe1".into() },
            CODE_OUT_OF_SCOPE,
            "out_of_scope",
        ),
        (
            ProjectError::InvalidRelativePath {
                relative_path: "/x".into(),
            },
            CODE_INVALID_RELATIVE_PATH,
            "invalid_relative_path",
        ),
        (
            ProjectError::ResourceOutsideFolder {
                relative_path: "../x".into(),
            },
            CODE_RESOURCE_OUTSIDE_FOLDER,
            "resource_outside_folder",
        ),
        (
            ProjectError::UnsupportedResourceScheme { scheme: "ssh".into() },
            CODE_UNSUPPORTED_RESOURCE_SCHEME,
            "unsupported_resource_scheme",
        ),
    ];
    for (err, code, msg) in cases {
        assert_eq!(project_error_to_rpc(&err), (code, msg), "for {err:?}");
    }
}

#[test]
fn fs_error_maps_to_protocol_codes() {
    assert_eq!(
        fs_error_to_rpc(&FsError::NotFound {
            uri: "file:///x".into()
        }),
        (CODE_RESOURCE_NOT_FOUND, "resource_not_found")
    );
    assert_eq!(
        fs_error_to_rpc(&FsError::UnsupportedScheme { scheme: "ssh".into() }),
        (CODE_UNSUPPORTED_RESOURCE_SCHEME, "unsupported_resource_scheme")
    );
    assert_eq!(
        fs_error_to_rpc(&FsError::PermissionDenied {
            uri: "file:///x".into()
        }),
        (CODE_PROVIDER_UNAVAILABLE, "provider_unavailable")
    );
    assert_eq!(
        fs_error_to_rpc(&FsError::Io {
            uri: "file:///x".into(),
            message: "boom".into(),
        }),
        (CODE_PROVIDER_UNAVAILABLE, "provider_unavailable")
    );
}
