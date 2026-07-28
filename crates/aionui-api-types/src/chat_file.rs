//! Chat-message file attachments.
//!
//! A message's attachments are a tagged union discriminated by `kind`, decided
//! purely by *source* (not by any save-to-workspace setting):
//! - explorer tree selections → [`ChatFileRef::Project`] (resolved server-side
//!   via `resolve_reference(op = Read)`),
//! - upload-button files → [`ChatFileRef::Upload`] (always `upload`, carrying
//!   the absolute path returned by `POST /api/fs/upload`).

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
}
