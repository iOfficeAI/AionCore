//! Chat-message file attachments.
//!
//! A message's attachments are a tagged union discriminated by `kind`, decided
//! purely by *source* (not by any save-to-workspace setting):
//! - explorer tree selections → [`ChatFileRef::Project`] (resolved server-side
//!   via `resolve_reference(op = Read)`),
//! - upload-button files → [`ChatFileRef::Upload`] (always `upload`, carrying
//!   the absolute path returned by `POST /api/fs/upload`),
//! - host-filesystem picker selections → [`ChatFileRef::Local`] (an absolute
//!   path the user explicitly chose in the backend-machine file browser).

use serde::{Deserialize, Serialize};

/// A single file attached to a chat message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatFileRef {
    /// A file inside a bound project folder, addressed by explorer identity
    /// (`pe_id` + `relative_path`). The backend resolves it to an absolute path
    /// via `resolve_reference` with lexical + realpath containment.
    Project { pe_id: String, relative_path: String },
    /// An uploaded file, carried as the absolute path returned by
    /// `POST /api/fs/upload`. The backend requires it to live under the managed
    /// upload directory before use.
    Upload { path: String },
    /// A file on the backend machine's filesystem, chosen by the user in the
    /// host-file browser (`/api/fs/browse`, which already exposes the whole
    /// filesystem). Carries an absolute path; the backend only checks it exists
    /// and is a regular file — no managed-directory restriction, since the
    /// picker that produced it already exposes this surface and the agent reads
    /// the path through its own filesystem tools.
    Local { path: String },
}
