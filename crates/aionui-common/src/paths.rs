//! Shared filesystem locations.
//!
//! The HTTP upload staging area (`POST /api/fs/upload`, `POST /api/fs/temp`)
//! and the agent spawn path (claude `--add-dir`) must agree on where uploaded
//! attachments live. Every expression of that layout goes through these
//! helpers — hand-rolling `temp_dir().join("aionui")` elsewhere reintroduces
//! the drift this module exists to prevent.

use std::path::PathBuf;

/// Root of the upload staging area: `<OS temp>/aionui`.
pub fn uploads_root() -> PathBuf {
    std::env::temp_dir().join("aionui")
}

/// Per-conversation upload directory: `<OS temp>/aionui/<conversation_id>`,
/// or `<OS temp>/aionui/general` when no conversation exists yet (uploads
/// from the home/guid page happen before the conversation is created).
pub fn uploads_dir(conversation_id: Option<&str>) -> PathBuf {
    let mut dir = uploads_root();
    match conversation_id {
        Some(id) if !id.is_empty() => dir.push(id),
        _ => dir.push("general"),
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_dir_scopes_by_conversation() {
        assert_eq!(uploads_dir(Some("conv-1")), uploads_root().join("conv-1"));
    }

    #[test]
    fn uploads_dir_falls_back_to_general() {
        assert_eq!(uploads_dir(None), uploads_root().join("general"));
        assert_eq!(uploads_dir(Some("")), uploads_root().join("general"));
    }
}
