use std::sync::Arc;

use aionui_common::constants::AIONUI_FILES_MARKER;
use aionui_db::{IProjectStore, SqliteProjectStore, init_database_memory};
use tempfile::TempDir;

use crate::ProjectService;
use crate::canonical::to_file_uri;
use crate::types::ProjectError;
use aionui_api_types::ChatFileRef;

/// Build a service with a tempdir standard project. Returns (service, pe_id,
/// workspace dir, upload_root dir).
async fn setup() -> (Arc<ProjectService>, String, TempDir, TempDir) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));
    let dir = tempfile::tempdir().unwrap();
    let created = service
        .create_standard("system_default_user", to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    let upload_root = tempfile::tempdir().unwrap();
    (service, created.project_explorer.pe_id, dir, upload_root)
}

#[tokio::test]
async fn resolves_project_file_and_inlines_marker() {
    let (service, pe_id, dir, upload_root) = setup().await;
    std::fs::write(dir.path().join("note.txt"), b"hi").unwrap();

    let out = service
        .resolve_chat_message(
            "system_default_user",
            "please review",
            &[ChatFileRef::Project {
                pe_id: pe_id.clone(),
                relative_path: "note.txt".into(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap();

    // The resolved path is the canonicalized absolute path (case-folded on
    // case-insensitive platforms), so assert on shape/resolution rather than a
    // byte-equal path, and that content re-inlines exactly that path.
    assert_eq!(out.files.len(), 1);
    let abs = &out.files[0];
    assert!(std::path::Path::new(abs).is_file());
    assert!(abs.ends_with("note.txt"));
    assert_eq!(out.content, format!("please review\n\n{AIONUI_FILES_MARKER}\n{abs}"));
}

#[tokio::test]
async fn resolves_project_directory_ref() {
    let (service, pe_id, dir, upload_root) = setup().await;
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    // A folder attachment (tree right-click on a directory) must resolve, not
    // be rejected as a missing file.
    let out = service
        .resolve_chat_message(
            "system_default_user",
            "look here",
            &[ChatFileRef::Project {
                pe_id,
                relative_path: "sub".into(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap();
    assert_eq!(out.files.len(), 1);
    assert!(std::path::Path::new(&out.files[0]).is_dir());
}

#[tokio::test]
async fn resolves_project_root_ref_empty_relative_path() {
    // bug2 (add a pe ROOT to chat): the root node's relative_path is "", which
    // must resolve to the pe root directory itself — not error, not resolve to
    // some parent. Guards the empty-path root case specifically (the sibling
    // test above only covers a named subdirectory).
    let (service, pe_id, dir, upload_root) = setup().await;

    let out = service
        .resolve_chat_message(
            "system_default_user",
            "review this project",
            &[ChatFileRef::Project {
                pe_id,
                relative_path: String::new(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap();

    assert_eq!(out.files.len(), 1);
    let abs = std::path::Path::new(&out.files[0]);
    assert!(abs.exists(), "root ref must resolve to an existing path");
    assert!(abs.is_dir(), "a pe root resolves to a directory");
    // The resolved path is the canonicalized pe root itself.
    assert_eq!(
        std::fs::canonicalize(abs).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
}

#[tokio::test]
async fn empty_files_leaves_content_unchanged() {
    let (service, _pe, _dir, upload_root) = setup().await;
    let out = service
        .resolve_chat_message("system_default_user", "hi", &[], upload_root.path())
        .await
        .unwrap();
    assert_eq!(out.content, "hi");
    assert!(out.files.is_empty());
}

#[tokio::test]
async fn missing_project_file_is_atomic_error() {
    let (service, pe_id, _dir, upload_root) = setup().await;
    let err = service
        .resolve_chat_message(
            "system_default_user",
            "x",
            &[ChatFileRef::Project {
                pe_id,
                relative_path: "nope.txt".into(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::ChatFileMissing { .. }), "got {err:?}");
}

#[tokio::test]
async fn upload_under_root_is_accepted() {
    let (service, _pe, _dir, upload_root) = setup().await;
    let up = upload_root.path().join("u.png");
    std::fs::write(&up, b"x").unwrap();
    let path = up.to_string_lossy().into_owned();

    let out = service
        .resolve_chat_message(
            "system_default_user",
            "",
            &[ChatFileRef::Upload { path: path.clone() }],
            upload_root.path(),
        )
        .await
        .unwrap();
    assert_eq!(out.files, vec![path]);
}

#[tokio::test]
async fn local_readable_file_resolves_and_inlines_marker() {
    let (service, _pe, _dir, upload_root) = setup().await;
    // A file anywhere on disk (outside the managed upload root) — `local` has no
    // managed-directory restriction, only existence + is-file.
    let outside = tempfile::tempdir().unwrap();
    let f = outside.path().join("host.txt");
    std::fs::write(&f, b"hi").unwrap();
    let path = f.to_string_lossy().into_owned();

    let out = service
        .resolve_chat_message(
            "system_default_user",
            "see this",
            &[ChatFileRef::Local { path }],
            upload_root.path(),
        )
        .await
        .unwrap();

    assert_eq!(out.files.len(), 1);
    let abs = &out.files[0];
    // Resolved to the canonicalized absolute path (symlinks/`..` collapsed).
    assert!(std::path::Path::new(abs).is_file());
    assert!(abs.ends_with("host.txt"));
    assert_eq!(out.content, format!("see this\n\n{AIONUI_FILES_MARKER}\n{abs}"));
}

#[cfg(unix)]
#[tokio::test]
async fn local_canonicalizes_symlink_to_target_path() {
    let (service, _pe, _dir, upload_root) = setup().await;
    // A symlink whose name differs from its target, so we can prove the
    // resolved path is the *target* (canonicalized), not the link we were given.
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("real_target.txt");
    std::fs::write(&target, b"hi").unwrap();
    let link = d.path().join("link_name.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let link_path = link.to_string_lossy().into_owned();

    let out = service
        .resolve_chat_message(
            "system_default_user",
            "x",
            &[ChatFileRef::Local {
                path: link_path.clone(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap();

    assert_eq!(out.files.len(), 1);
    let resolved = &out.files[0];
    // `canonicalize` collapses the symlink to the target's real path — this is
    // the behavior a `PathBuf::from(path)` mutation would break.
    let expected = std::fs::canonicalize(&target).unwrap().to_string_lossy().into_owned();
    assert_eq!(resolved, &expected, "expected canonicalized target, got {resolved}");
    assert!(
        resolved.ends_with("real_target.txt"),
        "should be target name, not link name"
    );
    assert_ne!(resolved, &link_path, "must not echo back the raw symlink path");
    assert_eq!(out.content, format!("x\n\n{AIONUI_FILES_MARKER}\n{resolved}"));
}

#[tokio::test]
async fn local_nonexistent_is_rejected() {
    let (service, _pe, _dir, upload_root) = setup().await;
    let missing = upload_root.path().join("nope.txt").to_string_lossy().into_owned();

    let err = service
        .resolve_chat_message(
            "system_default_user",
            "x",
            &[ChatFileRef::Local { path: missing }],
            upload_root.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::LocalPathNotReadable { .. }), "got {err:?}");
}

#[tokio::test]
async fn local_directory_is_rejected() {
    let (service, _pe, _dir, upload_root) = setup().await;
    // A real directory is not a regular file → rejected.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().to_string_lossy().into_owned();

    let err = service
        .resolve_chat_message(
            "system_default_user",
            "x",
            &[ChatFileRef::Local { path }],
            upload_root.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::LocalPathNotReadable { .. }), "got {err:?}");
}

#[tokio::test]
async fn upload_outside_root_is_rejected() {
    let (service, _pe, _dir, upload_root) = setup().await;
    // A real file, but outside the managed upload dir.
    let outside = tempfile::tempdir().unwrap();
    let ext = outside.path().join("secret.txt");
    std::fs::write(&ext, b"x").unwrap();

    let err = service
        .resolve_chat_message(
            "system_default_user",
            "x",
            &[ChatFileRef::Upload {
                path: ext.to_string_lossy().into_owned(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::UploadPathOutsideRoot { .. }), "got {err:?}");
}

// ── upgrade_chat_file_ref ──────────────────────────────────────────────
//
// Turning a `Local` path into `Project{pe_id, relative_path}` when it lives under
// one of the project's roots. Every case below is one-directional on purpose: the
// caller uses the result for addressing, so a wrong upgrade points it at the wrong
// file, while a missed upgrade only costs a stronger identity it could not get.

/// Like `setup`, but also hands back the project id the upgrade path needs.
async fn setup_with_project() -> (Arc<ProjectService>, String, String, TempDir) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));
    let dir = tempfile::tempdir().unwrap();
    let created = service
        .create_standard("system_default_user", to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    (service, created.project.project_id, created.project_explorer.pe_id, dir)
}

#[tokio::test]
async fn upgrade_local_inside_root_becomes_project_ref() {
    let (service, project_id, pe_id, dir) = setup_with_project().await;
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    let file = dir.path().join("docs/report.md");
    std::fs::write(&file, b"x").unwrap();

    let out = service
        .upgrade_chat_file_ref(
            "system_default_user",
            &project_id,
            &ChatFileRef::Local {
                path: file.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        out,
        ChatFileRef::Project {
            pe_id,
            relative_path: "docs/report.md".to_owned(),
        },
        "a file under the workspace root must upgrade to explorer identity"
    );
}

#[tokio::test]
async fn upgrade_local_outside_every_root_is_returned_unchanged() {
    let (service, project_id, _pe_id, _dir) = setup_with_project().await;
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("elsewhere.txt");
    std::fs::write(&file, b"x").unwrap();
    let input = ChatFileRef::Local {
        path: file.to_string_lossy().into_owned(),
    };

    let out = service
        .upgrade_chat_file_ref("system_default_user", &project_id, &input)
        .await
        .unwrap();

    assert_eq!(out, input, "a file outside the project must stay a local ref");
}

/// The missing-file case is the one most likely to be "tidied" into an error by a
/// later reader, since `fs::canonicalize` fails here. It must not: the caller is
/// part-way through opening a file it will render a missing state for, and it still
/// needs a ref to key that state on.
#[tokio::test]
async fn upgrade_nonexistent_path_is_returned_unchanged_not_an_error() {
    let (service, project_id, _pe_id, dir) = setup_with_project().await;
    // Inside the root, so only the missing file can be what stops the upgrade.
    let input = ChatFileRef::Local {
        path: dir.path().join("never-written.md").to_string_lossy().into_owned(),
    };

    let out = service
        .upgrade_chat_file_ref("system_default_user", &project_id, &input)
        .await
        .expect("a missing file must not fail the upgrade");

    assert_eq!(out, input);
}

/// Both non-`Local` kinds must come back untouched, and the files here are real and
/// sitting *inside* a root on purpose: a nonexistent path would also be returned
/// unchanged — via the missing-file branch — so the test would pass without the
/// short-circuit ever running. Placing them where an upgrade *could* succeed is what
/// makes the assertion about the short-circuit rather than about a failed lookup.
#[tokio::test]
async fn upgrade_leaves_project_and_upload_refs_untouched() {
    let (service, project_id, pe_id, dir) = setup_with_project().await;

    // Already terminal — and short-circuiting keeps the explorer's own open path
    // from paying for a lookup whose answer it already has.
    let real = dir.path().join("inside.md");
    std::fs::write(&real, b"x").unwrap();
    let project_ref = ChatFileRef::Project {
        pe_id,
        relative_path: "inside.md".to_owned(),
    };
    assert_eq!(
        service
            .upgrade_chat_file_ref("system_default_user", &project_id, &project_ref)
            .await
            .unwrap(),
        project_ref
    );

    // Uploads belong to the managed upload directory, not to a root. This one is
    // deliberately a real file under the workspace root, so if the short-circuit
    // were dropped the lookup would succeed and rewrite it to a `Project` ref —
    // which is exactly the regression worth catching.
    let upload_ref = ChatFileRef::Upload {
        path: real.to_string_lossy().into_owned(),
    };
    assert_eq!(
        service
            .upgrade_chat_file_ref("system_default_user", &project_id, &upload_ref)
            .await
            .unwrap(),
        upload_ref,
        "an upload ref must not be rewritten even when its path happens to sit under a root"
    );
}

/// Roots cannot nest, so the lookup never has to choose between two containing
/// roots. `attach_folder` focuses the existing entry when the target is a
/// descendant of a root (`service.rs`), and rejects an ancestor as an overlap —
/// this pins that the reverse lookup is built on top of that guarantee, so a
/// change there would surface here rather than silently making the upgrade
/// ambiguous.
#[tokio::test]
async fn attaching_a_subdirectory_does_not_create_a_second_containing_root() {
    let (service, project_id, ws_pe, dir) = setup_with_project().await;
    let inner = dir.path().join("packages/app");
    std::fs::create_dir_all(&inner).unwrap();
    let file = inner.join("main.rs");
    std::fs::write(&file, b"x").unwrap();

    let attached = service
        .attach_folder(
            "system_default_user",
            crate::types::AttachInput {
                project_id: project_id.clone(),
                uri: to_file_uri(&inner).unwrap(),
                display_name: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        attached.pe_id, ws_pe,
        "attaching a descendant must focus the workspace entry, not add a root"
    );

    // One root remains, so the path stays relative to the workspace.
    let out = service
        .upgrade_chat_file_ref(
            "system_default_user",
            &project_id,
            &ChatFileRef::Local {
                path: file.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        out,
        ChatFileRef::Project {
            pe_id: ws_pe,
            relative_path: "packages/app/main.rs".to_owned(),
        }
    );
}

/// Case folding is a compile-time platform fork, so a single-platform pass says
/// nothing about the other two — and the two failure modes are opposites: macOS
/// would miss a match, Linux would conflate distinct files. Both branches are
/// asserted here so whichever host runs this exercises its own rule.
#[tokio::test]
async fn upgrade_case_handling_follows_the_platform_rule() {
    let (service, project_id, pe_id, dir) = setup_with_project().await;
    let file = dir.path().join("Report.md");
    std::fs::write(&file, b"x").unwrap();

    // Ask with the name lowercased. On a case-insensitive host this is the same
    // file; on Linux it is a different (missing) one.
    let requested = dir.path().join("report.md");
    let out = service
        .upgrade_chat_file_ref(
            "system_default_user",
            &project_id,
            &ChatFileRef::Local {
                path: requested.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();

    if crate::canonical::IGNORE_PATH_CASING {
        // macOS / Windows: resolves to the real file and upgrades. The relative
        // path comes from the folded canonical, so compare case-insensitively
        // rather than pinning one host's spelling.
        match out {
            ChatFileRef::Project {
                pe_id: got_pe,
                relative_path,
            } => {
                assert_eq!(got_pe, pe_id);
                assert_eq!(relative_path.to_ascii_lowercase(), "report.md");
            }
            other => panic!("case-insensitive host should upgrade, got {other:?}"),
        }
    } else {
        // Linux: `report.md` does not exist, so it must stay local — upgrading it
        // would hand the caller an identity for a file it did not ask about.
        assert_eq!(
            out,
            ChatFileRef::Local {
                path: requested.to_string_lossy().into_owned(),
            },
            "a case-sensitive host must not match a differently-cased file"
        );
    }
}

/// Both sides are canonicalized, so a symlink resolves to its target before the
/// containment test. A link inside the root pointing outside it therefore does not
/// upgrade. Pinning this keeps the behaviour predictable rather than intuitive, and
/// matches the existing `path_within` primitive.
#[tokio::test]
#[cfg(unix)]
async fn upgrade_follows_symlinks_so_a_link_out_of_the_root_does_not_upgrade() {
    let (service, project_id, _pe_id, dir) = setup_with_project().await;
    let outside = tempfile::tempdir().unwrap();
    let real = outside.path().join("secret.md");
    std::fs::write(&real, b"x").unwrap();

    let link = dir.path().join("link.md");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let input = ChatFileRef::Local {
        path: link.to_string_lossy().into_owned(),
    };
    let out = service
        .upgrade_chat_file_ref("system_default_user", &project_id, &input)
        .await
        .unwrap();

    assert_eq!(
        out, input,
        "the link resolves outside the root, so it must not gain project identity"
    );
}

/// The upgraded ref must be directly usable as an `fs` channel subscription target,
/// which is the whole point of upgrading: a chat-opened file gains the identity the
/// change signal is keyed on.
///
/// This pins the *structural* half only — that the fields an upgraded ref carries
/// are exactly the fields `fs/subscribe` takes (`ResourceRef{pe_id, relative_path}`,
/// `monitor/wire.rs`). Whether a `modified` delta actually arrives for it needs a
/// live WS session plus a real write, which no test at this level can do; that half
/// is unverified here and must not be inferred from this passing.
#[tokio::test]
async fn upgraded_ref_carries_exactly_the_fs_channel_target_fields() {
    let (service, project_id, pe_id, dir) = setup_with_project().await;
    let file = dir.path().join("watched.md");
    std::fs::write(&file, b"x").unwrap();

    let out = service
        .upgrade_chat_file_ref(
            "system_default_user",
            &project_id,
            &ChatFileRef::Local {
                path: file.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();

    let ChatFileRef::Project {
        pe_id: got,
        relative_path,
    } = out
    else {
        panic!("expected an upgraded project ref");
    };
    assert_eq!(got, pe_id);

    // Same shape the subscribe path deserializes into, built from the upgrade's
    // output alone — no extra lookup, no path translation.
    let target = crate::monitor::wire::ResourceRef {
        pe_id: got,
        relative_path,
    };
    assert_eq!(target.pe_id, pe_id);
    assert_eq!(target.relative_path, "watched.md");
}
