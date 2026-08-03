#![warn(clippy::disallowed_types)]

//! File system operations: read/write, path safety, file watching, and snapshots.
pub mod error;
pub mod path_safety;
pub mod routes;
pub mod service;
pub mod snapshot_service;
pub mod traits;
pub mod types;
pub mod watch_service;

pub use error::FileError;
pub use path_safety::{has_traversal, validate_path, validate_path_for_write};
pub use routes::{FileRouterState, file_routes};
pub use service::FileService;
pub use snapshot_service::SnapshotService;
pub use traits::{
    FileServiceRef, FileWatchServiceRef, IFileService, IFileWatchService, IItemRevealer, ISnapshotService,
    ItemRevealerRef, SnapshotServiceRef,
};
pub use types::{
    CompareResult, ContentUpdateEvent, ContentUpdateOperation, CopyResult, DirOrFile, FileChangeInfo, FileMetadata,
    FileWatchEvent, OfficeFileAddedEvent, SnapshotInfo, SnapshotMode, WorkspaceFlatFile,
};
pub use watch_service::FileWatchService;
