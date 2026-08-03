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

    /// The file-watch backend is unavailable (the platform watcher could not be
    /// created, e.g. the inotify instance/watch limit was reached). Non-fatal:
    /// the server runs without file watching. `errno` carries the originating OS
    /// error when known, for telemetry attribution. Maps to the stable API code
    /// `FILE_WATCH_UNAVAILABLE`.
    #[error("file watch service is unavailable")]
    WatchUnavailable { errno: Option<i32> },

    /// Revealing an item in the OS file manager failed (the shell reveal command
    /// errored). Distinct from `NotFound` (missing path) so the frontend can tell
    /// "couldn't open the file manager" from "the item is gone". Maps to the
    /// stable API code `REVEAL_FAILED`.
    #[error("failed to reveal item: {0}")]
    RevealFailed(String),
}
