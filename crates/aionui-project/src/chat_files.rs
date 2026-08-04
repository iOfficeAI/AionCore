//! Resolve chat-message file attachments to absolute device paths.
//!
//! The single shared point where a message's [`ChatFileRef`] list becomes
//! concrete paths, called at the send boundary by conversation + team. It
//! reproduces the legacy `[[AION_FILES]]` inlined-attachment content form so
//! every downstream consumer — aionrs `build_content_blocks`, the ACP prompt
//! (content-only), message persistence, and the user-message file chips in the
//! UI — is unchanged: only the *origin* of the paths moves from the client to
//! this backend edge.

use std::path::Path;

use aionui_api_types::ChatFileRef;
use aionui_common::constants::AIONUI_FILES_MARKER;

use crate::service::ProjectService;
use crate::types::{FileOp, ProjectError, ReferenceInput};

/// A chat message whose attachments have been resolved to absolute paths and
/// re-inlined into [`content`](Self::content) via the `[[AION_FILES]]` marker.
#[derive(Debug)]
pub struct ResolvedChatMessage {
    /// User text with the attachment block appended (marker + one absolute path
    /// per line). Used verbatim for persistence, broadcast, and agent input.
    pub content: String,
    /// The resolved absolute paths, in order. Kept alongside `content` so
    /// aionrs's `build_content_blocks` strips the marker (files match) and
    /// re-adds its own; clearing this would leak the raw marker to aionrs.
    pub files: Vec<String>,
}

impl ProjectService {
    /// Resolve a message's `files` to absolute paths and return the inlined
    /// content. Atomic: any bad reference (unknown pe, escape, missing file,
    /// out-of-root upload, unreadable local path) fails the whole message.
    ///
    /// `upload_root` is the managed upload directory (`temp_dir()/aionui`);
    /// `Upload` paths must live under it.
    pub async fn resolve_chat_message(
        &self,
        user_id: &str,
        content: &str,
        files: &[ChatFileRef],
        upload_root: &Path,
    ) -> Result<ResolvedChatMessage, ProjectError> {
        let mut paths = Vec::with_capacity(files.len());
        for file in files {
            paths.push(
                self.resolve_chat_file_ref(user_id, file, upload_root, FileOp::Read)
                    .await?,
            );
        }

        let content = if paths.is_empty() {
            content.to_owned()
        } else {
            format!("{content}\n\n{AIONUI_FILES_MARKER}\n{}", paths.join("\n"))
        };
        Ok(ResolvedChatMessage { content, files: paths })
    }

    /// Resolve a single [`ChatFileRef`] to an absolute device path.
    ///
    /// The per-ref identity→path core shared by [`resolve_chat_message`](Self::resolve_chat_message)
    /// (send boundary) and the preview content endpoint (`aionui-file`, cross-crate — hence `pub`).
    /// Guards differ by variant:
    /// - `Project` → [`resolve_reference`](Self::resolve_reference) with the caller's `op` (lexical +
    ///   realpath containment; read paths pass `Read`, the write endpoint passes `Write`); must exist
    ///   (file or folder).
    /// - `Upload` → an existing regular file under the managed `upload_root` (D2 invariant).
    /// - `Local` → a canonicalized existing regular file; **no sandbox** (the host picker that
    ///   produced it already exposes the whole filesystem).
    ///
    /// `op` only affects the `Project` arm's containment mode; `Upload`/`Local` are path-based and
    /// identical regardless of op.
    pub async fn resolve_chat_file_ref(
        &self,
        user_id: &str,
        file: &ChatFileRef,
        upload_root: &Path,
        op: FileOp,
    ) -> Result<String, ProjectError> {
        match file {
            ChatFileRef::Project { pe_id, relative_path } => {
                let resolved = self
                    .resolve_reference(
                        user_id,
                        ReferenceInput {
                            pe_id: pe_id.clone(),
                            relative_path: relative_path.clone(),
                            op,
                        },
                    )
                    .await?;
                let abs = resolved.absolute_path.ok_or_else(|| ProjectError::ChatFileMissing {
                    path: relative_path.clone(),
                })?;
                // A project ref may point at a file or a folder (the tree
                // allows attaching a directory); require only that it exists.
                if !Path::new(&abs).exists() {
                    return Err(ProjectError::ChatFileMissing { path: abs });
                }
                Ok(abs)
            }
            ChatFileRef::Upload { path } => {
                let candidate = Path::new(path);
                if !candidate.is_file() {
                    return Err(ProjectError::ChatFileMissing { path: path.clone() });
                }
                if !path_within(upload_root, candidate) {
                    return Err(ProjectError::UploadPathOutsideRoot { path: path.clone() });
                }
                Ok(path.clone())
            }
            ChatFileRef::Local { path } => {
                // A path the user explicitly picked in the host-file browser,
                // which already exposes the whole filesystem. No managed-root
                // restriction (that is the upload channel's D2 invariant only);
                // just canonicalize (collapsing `..`/symlinks) and require an
                // existing regular file.
                let canonical = std::fs::canonicalize(path)
                    .map_err(|_| ProjectError::LocalPathNotReadable { path: path.clone() })?;
                if !canonical.is_file() {
                    return Err(ProjectError::LocalPathNotReadable { path: path.clone() });
                }
                Ok(canonical.to_string_lossy().into_owned())
            }
        }
    }
}

/// Whether `target` resolves inside `root` (both canonicalized, so `..` and
/// symlinks cannot escape). `target` is expected to exist (checked before).
fn path_within(root: &Path, target: &Path) -> bool {
    let (Ok(root), Ok(target)) = (std::fs::canonicalize(root), std::fs::canonicalize(target)) else {
        return false;
    };
    target.starts_with(root)
}

#[cfg(test)]
#[path = "chat_files_test.rs"]
mod chat_files_test;
