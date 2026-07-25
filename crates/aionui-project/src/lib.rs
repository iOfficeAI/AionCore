//! Project / Folder creation-binding authority.
//!
//! `aionui-project` is the single authority for the consistency of the three
//! project-bind tables (`projects` / `folders` / `project_explorer`) and for
//! resource identity + containment. It holds only logic: SQL and transactions
//! live in `aionui-db` behind `IProjectStore`, injected as `Arc<dyn IProjectStore>`.
//!
//! Two pure modules ([`canonical`], [`containment`]) are safety/correctness
//! critical and filesystem-free; the rest is service orchestration.

pub mod canonical;
pub mod containment;
mod service;
pub mod types;

pub use service::ProjectService;
pub use types::{
    AttachInput, FileOp, FolderDto, ProjectDetail, ProjectError, ProjectExplorerEntry, ProjectExplorerView,
    ReferenceInput, ResolveOutput, ResolvedResource, RuntimeStatus,
};
