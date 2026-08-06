//! Where discarded untracked files go.
//!
//! Discarding an untracked change is the one destructive operation in stage 0:
//! the version-control system holds no copy, so an unlink is unrecoverable. The
//! floor is therefore the platform trash, never a delete.
//!
//! That rule needs a test seam. The real implementation talks to the desktop
//! platform (Finder on macOS, the shell's recycle bin on Windows, the
//! Freedesktop trash spec on Linux), and a test cannot make it fail on demand,
//! nor observe where a file landed — the destination is platform-chosen and not
//! reliably readable. So the operation sits behind this trait: production uses
//! [`PlatformTrash`], tests inject implementations that fail deterministically or
//! record what they were asked to do.

use std::path::Path;

/// Sends a file to the platform trash.
///
/// Deliberately one method and no knowledge of source control: implementations
/// must not decide *whether* a file may be discarded, only carry out the move.
pub(super) trait TrashSink: Send + Sync {
    /// Move `path` to the platform trash.
    ///
    /// An error must mean **the file was left alone** — implementations must
    /// never fall back to deleting it, which would turn a recoverable failure
    /// into data loss.
    fn trash(&self, path: &Path) -> Result<(), String>;
}

/// The production sink: the platform's own trash.
pub(super) struct PlatformTrash;

impl TrashSink for PlatformTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        // `trash` fails before touching the file when the path cannot be
        // resolved, so a failure here leaves the work tree untouched.
        trash::delete(path).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
#[path = "trash_sink_test.rs"]
mod trash_sink_test;
