/// File crate application errors.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{message}")]
    PathOutsideSandbox {
        message: String,
        field: Option<&'static str>,
        operation: Option<&'static str>,
    },

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Internal(String),

    /// Revealing an item in the OS file manager failed (the shell reveal command
    /// errored). Distinct from `NotFound` (missing path) so the frontend can tell
    /// "couldn't open the file manager" from "the item is gone". Maps to the
    /// stable API code `REVEAL_FAILED`.
    ///
    /// The payload is the underlying cause, kept **for logs only**. Boundary
    /// mappers must not copy it into the response: it originates in the shell
    /// layer and can quote a subprocess's stderr or an absolute path.
    #[error("failed to reveal item: {0}")]
    RevealFailed(String),

    /// A backend-resolved target does not exist on disk.
    ///
    /// Deliberately payload-free, unlike [`Self::NotFound`]. It serves routes
    /// addressed by identity (`{pe_id, relative_path}` or `ChatFileRef`) rather
    /// than by client-supplied path, where the absolute path is resolved on the
    /// server and the client has never seen it. Having no field to carry a path
    /// means no boundary mapper can forward one, which is what went wrong before:
    /// the shell error's path was threaded through `NotFound(String)` all the way
    /// into the response body. Maps to the stable API code `FILE_NOT_FOUND`.
    #[error("target not found")]
    TargetNotFound,
}
