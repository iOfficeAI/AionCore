use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aionui_common::generate_short_id;
use aionui_db::{FolderRow, IProjectStore, ProjectExplorerRow, ProjectKind, Role};
use chrono::{Datelike, Local};
use tokio::sync::mpsc::UnboundedSender;

use crate::canonical::{self, Canonical};
use crate::containment;
use crate::scm::ScmInbound;
use crate::types::{
    AttachInput, FolderDto, ProjectDetail, ProjectError, ProjectExplorerEntry, ProjectExplorerView, ReferenceInput,
    ResolveOutput, ResolvedResource, RuntimeStatus,
};

/// Orchestrates the three project-bind tables through an injected
/// [`IProjectStore`]. Owns filesystem operations (temp-dir creation,
/// access checks) and the temp-root path; holds no transactions.
#[derive(Clone)]
pub struct ProjectService {
    store: Arc<dyn IProjectStore>,
    temp_root: PathBuf,
    /// Sink for project-root changes to the source-control actor. Shared across
    /// clones (behind `Arc`) so the one instance the scm monitor installs the
    /// sender on is the same one the HTTP handlers see. Set once at startup;
    /// `None` in tests and the HTTP-only assembly path, where a root change simply
    /// notifies nobody.
    scm_roots_tx: Arc<OnceLock<UnboundedSender<ScmInbound>>>,
}

impl ProjectService {
    pub fn new(store: Arc<dyn IProjectStore>, temp_root: PathBuf) -> Self {
        Self {
            store,
            temp_root,
            scm_roots_tx: Arc::new(OnceLock::new()),
        }
    }

    /// Install the sink that carries project-root changes to the source-control
    /// actor. Called once, at startup, by the scm monitor's composition wiring;
    /// later calls are ignored (set-once). Because the field is shared across
    /// clones, setting it on any clone makes it visible to all of them.
    pub fn set_scm_roots_sender(&self, tx: UnboundedSender<ScmInbound>) {
        let _ = self.scm_roots_tx.set(tx);
    }

    /// Tell the source-control actor that a project's attached-folder set changed,
    /// so it can recompute repositories and push a `repositoriesChanged` frame.
    ///
    /// Best effort: if no actor is wired, or it has stopped, source control simply
    /// misses the live update (a manual refresh still recovers) — this never fails
    /// the attach or detach that already succeeded.
    fn notify_roots_changed(&self, project_id: &str, user_id: &str) {
        if let Some(tx) = self.scm_roots_tx.get() {
            let _ = tx.send(ScmInbound::RootsChanged {
                project_id: project_id.to_owned(),
                user_id: user_id.to_owned(),
            });
        }
    }

    // ── creation / backfill ────────────────────────────────────────────

    /// User-selected existing directory → `kind = standard`.
    pub async fn create_standard(&self, user_id: &str, uri: String) -> Result<ResolveOutput, ProjectError> {
        let canonical = canonical::canonicalize(&uri)?;
        self.ensure_accessible(&canonical)?;
        self.resolve_core(user_id, canonical, uri, ProjectKind::Standard, None)
            .await
    }

    /// Create a temp session directory (path owned here) → `kind = temp`.
    /// A caller-supplied `basename` that collides with an existing directory
    /// surfaces `temp_dir_exists`; an auto `short_uuid` collision is retried.
    pub async fn create_temp(&self, user_id: &str, basename: Option<String>) -> Result<ResolveOutput, ProjectError> {
        let dir = match basename.as_deref() {
            Some(name) if !name.is_empty() => self.make_temp_dir(name)?,
            _ => loop {
                match self.make_temp_dir(&generate_short_id()) {
                    Ok(dir) => break dir,
                    Err(ProjectError::TempDirExists { .. }) => continue,
                    Err(err) => return Err(err),
                }
            },
        };
        let leaf = leaf_of(&dir);
        let uri = canonical::to_file_uri(&dir)?;
        let canonical = canonical::canonicalize(&uri)?;
        self.resolve_core(user_id, canonical, uri, ProjectKind::Temp, Some(leaf))
            .await
    }

    /// Lazy backfill of an existing path. Does not create directories; `kind`
    /// is decided here (under `temp_root` ⇒ temp, otherwise standard).
    pub async fn resolve_existing(&self, user_id: &str, uri: String) -> Result<ResolveOutput, ProjectError> {
        let canonical = canonical::canonicalize(&uri)?;
        self.ensure_accessible(&canonical)?;
        let kind = if self.is_under_temp_root(&canonical) {
            ProjectKind::Temp
        } else {
            ProjectKind::Standard
        };
        self.resolve_core(user_id, canonical, uri, kind, None).await
    }

    /// Shared core: upsert folder → (standard) reuse workspace project if any →
    /// otherwise atomically create project + workspace entry.
    async fn resolve_core(
        &self,
        user_id: &str,
        canonical: Canonical,
        uri: String,
        kind: ProjectKind,
        temp_leaf: Option<String>,
    ) -> Result<ResolveOutput, ProjectError> {
        let folder = self.store.upsert_folder(canonical.as_str(), &uri).await?;

        if kind == ProjectKind::Standard
            && let Some(entry) = self
                .store
                .select_workspace_entry_by_folder(user_id, &folder.folder_id)
                .await?
        {
            let project = self
                .store
                .get_project(user_id, &entry.project_id)
                .await?
                .ok_or_else(|| ProjectError::StandardProjectConflict {
                    folder_id: folder.folder_id.clone(),
                })?;
            return Ok(ResolveOutput {
                project,
                folder,
                project_explorer: entry,
            });
        }

        let name = match kind {
            ProjectKind::Standard => canonical::basename(&canonical),
            ProjectKind::Temp => temp_leaf.unwrap_or_else(|| canonical::basename(&canonical)),
        };
        let (project, entry) = self
            .store
            .create_project_with_workspace_entry(user_id, &folder.folder_id, &name, kind)
            .await?;
        Ok(ResolveOutput {
            project,
            folder,
            project_explorer: entry,
        })
    }

    // ── attached folders ───────────────────────────────────────────────

    /// Attach a non-workspace folder. Rejects duplicates and parent-overlap;
    /// a child of an existing entry returns that entry (focus-in-place).
    pub async fn attach_folder(&self, user_id: &str, input: AttachInput) -> Result<ProjectExplorerRow, ProjectError> {
        // Ownership gate: without it a caller could hang entries off another
        // user's project_id (their scoped entry list would just look empty).
        self.store
            .get_project(user_id, &input.project_id)
            .await?
            .ok_or_else(|| ProjectError::ProjectNotFound {
                project_id: input.project_id.clone(),
            })?;
        let canonical = canonical::canonicalize(&input.uri)?;
        self.ensure_accessible(&canonical)?;
        let folder = self.store.upsert_folder(canonical.as_str(), &input.uri).await?;

        let entries = self.store.list_entries(user_id, &input.project_id).await?;
        if entries.iter().any(|(entry, _)| entry.folder_id == folder.folder_id) {
            return Err(ProjectError::ProjectExplorerDuplicate {
                project_id: input.project_id,
                folder_id: folder.folder_id,
            });
        }

        let new_path = canonical::fs_path(&canonical)?;
        for (entry, folder_row) in &entries {
            let existing = canonical::canonicalize(&folder_row.resource_canonical)?;
            let existing_path = canonical::fs_path(&existing)?;
            if new_path.starts_with(&existing_path) {
                // New folder is a descendant of an existing entry → focus it.
                return Ok(entry.clone());
            }
            if existing_path.starts_with(&new_path) {
                // New folder is an ancestor of an existing entry → overlap.
                return Err(ProjectError::ProjectExplorerOverlap {
                    project_id: input.project_id,
                });
            }
        }

        let order_index = entries.len() as i64;
        let row = self
            .store
            .insert_attached_entry(
                user_id,
                &input.project_id,
                &folder.folder_id,
                input.display_name.as_deref(),
                order_index,
            )
            .await?;
        // A newly attached folder can bring a repository into view. The
        // focus-in-place branches above return early without notifying — they add
        // no root, so the repository set is unchanged.
        self.notify_roots_changed(&input.project_id, user_id);
        Ok(row)
    }

    /// Remove an attached entry. The workspace entry cannot be removed here.
    pub async fn remove_attached(&self, user_id: &str, pe_id: &str) -> Result<(), ProjectError> {
        let entry =
            self.store
                .get_entry(user_id, pe_id)
                .await?
                .ok_or_else(|| ProjectError::ProjectExplorerNotFound {
                    pe_id: pe_id.to_owned(),
                })?;
        if entry.role == Role::Workspace.as_str() {
            return Err(ProjectError::WorkspaceEntryImmutable {
                pe_id: pe_id.to_owned(),
            });
        }
        self.store.remove_entry(user_id, pe_id).await?;
        // Detaching a folder can drop a repository from the project's set.
        self.notify_roots_changed(&entry.project_id, user_id);
        Ok(())
    }

    pub async fn reorder(
        &self,
        user_id: &str,
        project_id: &str,
        ordered_pe_ids: &[String],
    ) -> Result<(), ProjectError> {
        self.store.reorder(user_id, project_id, ordered_pe_ids).await?;
        Ok(())
    }

    pub async fn rename_entry(
        &self,
        user_id: &str,
        pe_id: &str,
        display_name: Option<String>,
    ) -> Result<ProjectExplorerRow, ProjectError> {
        match self.store.rename_entry(user_id, pe_id, display_name.as_deref()).await {
            Ok(row) => Ok(row),
            // A row this owner cannot see (missing or another user's) is the
            // same "not found" to the caller — never leak internal_db_error.
            Err(aionui_db::DbError::NotFound(_)) => Err(ProjectError::ProjectExplorerNotFound {
                pe_id: pe_id.to_owned(),
            }),
            Err(err) => Err(err.into()),
        }
    }

    // ── reads ──────────────────────────────────────────────────────────

    pub async fn get_project(&self, user_id: &str, project_id: &str) -> Result<ProjectDetail, ProjectError> {
        let project =
            self.store
                .get_project(user_id, project_id)
                .await?
                .ok_or_else(|| ProjectError::ProjectNotFound {
                    project_id: project_id.to_owned(),
                })?;

        let mut workspace_pe_id = String::new();
        let mut entries = Vec::new();
        for (entry, folder) in self.store.list_entries(user_id, project_id).await? {
            if entry.role == Role::Workspace.as_str() {
                workspace_pe_id = entry.pe_id.clone();
            }
            let folder_dto = self.build_folder_dto(&folder);
            entries.push(ProjectExplorerEntry {
                pe_id: entry.pe_id,
                project_id: entry.project_id,
                folder_id: entry.folder_id,
                role: entry.role,
                display_name: entry.display_name,
                order_index: entry.order_index,
                folder: folder_dto,
            });
        }

        Ok(ProjectDetail {
            id: project.project_id,
            name: project.name,
            kind: project.kind,
            explorer: ProjectExplorerView {
                workspace_pe_id,
                entries,
            },
            created_at: project.created_at,
            updated_at: project.updated_at,
        })
    }

    // ── resource resolution (identity + containment, no IO) ─────────────

    pub async fn resolve_reference(
        &self,
        user_id: &str,
        input: ReferenceInput,
    ) -> Result<ResolvedResource, ProjectError> {
        let entry = self.store.get_entry(user_id, &input.pe_id).await?.ok_or_else(|| {
            ProjectError::ProjectExplorerNotFound {
                pe_id: input.pe_id.clone(),
            }
        })?;
        let folder = self
            .store
            .get_folder(&entry.folder_id)
            .await?
            .ok_or_else(|| aionui_db::DbError::NotFound(format!("folder {}", entry.folder_id)))?;

        let root = canonical::canonicalize(&folder.resource_canonical)?;
        let resolved = containment::resolve_relative(&root, &input.relative_path, input.op)?;

        // The agent-facing absolute path must preserve the real on-disk casing.
        // Derive it from the folder's stored real-case URI (`resource_uri`), NOT
        // the case-folded canonical: `canonical::canonicalize` ASCII-lowercases the
        // path on macOS/Windows (`IGNORE_PATH_CASING`) for dedupe/containment
        // identity, so `resolved.absolute_path` carries a lowercased root. That
        // folded form stays the containment + dedupe key (unchanged); only the
        // outward path handed to agents / clipboard / reveal switches to real
        // casing. The relative segment is already validated (no `..`, contained)
        // by `resolve_relative`, so joining it onto the real root cannot escape.
        let absolute_path = match resolved.absolute_path {
            Some(_) => {
                let real_root = canonical::uri_to_path(&folder.resource_uri)?;
                let abs = if resolved.relative_path.is_empty() {
                    real_root
                } else {
                    real_root.join(&resolved.relative_path)
                };
                Some(abs.to_string_lossy().into_owned())
            }
            None => None,
        };

        Ok(ResolvedResource {
            project_id: entry.project_id,
            pe_id: entry.pe_id,
            folder_id: entry.folder_id,
            root_resource_uri: folder.resource_uri,
            root_resource_canonical: folder.resource_canonical,
            relative_path: resolved.relative_path,
            resource_uri: resolved.resource_uri,
            absolute_path,
        })
    }

    // ── owner binding validation (does not touch owner tables) ──────────

    pub async fn validate_workspace_match(
        &self,
        user_id: &str,
        project_id: &str,
        folder_id: &str,
    ) -> Result<(), ProjectError> {
        let workspace = self
            .store
            .list_entries(user_id, project_id)
            .await?
            .into_iter()
            .find(|(entry, _)| entry.role == Role::Workspace.as_str());
        match workspace {
            Some((entry, _)) if entry.folder_id == folder_id => Ok(()),
            _ => Err(ProjectError::WorkspaceFolderMismatch {
                project_id: project_id.to_owned(),
                folder_id: folder_id.to_owned(),
            }),
        }
    }

    // ── filesystem helpers ──────────────────────────────────────────────

    /// Stat a canonical folder to confirm it is an existing directory.
    fn ensure_accessible(&self, canonical: &Canonical) -> Result<(), ProjectError> {
        let path = canonical::fs_path(canonical)?;
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => Ok(()),
            Ok(_) => Err(ProjectError::FolderNotDirectory {
                path: path.to_string_lossy().into_owned(),
            }),
            Err(err) => Err(match err.kind() {
                std::io::ErrorKind::NotFound => ProjectError::FolderNotFound {
                    path: path.to_string_lossy().into_owned(),
                },
                std::io::ErrorKind::PermissionDenied => ProjectError::FolderPermissionDenied {
                    path: path.to_string_lossy().into_owned(),
                },
                _ => ProjectError::FolderNotFound {
                    path: path.to_string_lossy().into_owned(),
                },
            }),
        }
    }

    /// Create `{temp_root}/YYYY/MM/DD/{leaf}`. Errors `temp_dir_exists` if the
    /// leaf already exists (caller-name collision).
    fn make_temp_dir(&self, leaf: &str) -> Result<PathBuf, ProjectError> {
        let now = Local::now();
        let dir = self
            .temp_root
            .join(format!("{:04}", now.year()))
            .join(format!("{:02}", now.month()))
            .join(format!("{:02}", now.day()))
            .join(leaf);
        if dir.exists() {
            return Err(ProjectError::TempDirExists {
                path: dir.to_string_lossy().into_owned(),
            });
        }
        std::fs::create_dir_all(&dir).map_err(|_| ProjectError::FolderCanonicalizeFailed {
            uri: dir.to_string_lossy().into_owned(),
        })?;
        Ok(dir)
    }

    /// Whether a canonical folder lives under this service's temp root
    /// (both sides canonicalized so the comparison is lexically consistent).
    fn is_under_temp_root(&self, canonical: &Canonical) -> bool {
        let Ok(target) = canonical::fs_path(canonical) else {
            return false;
        };
        let Ok(root_uri) = canonical::to_file_uri(&self.temp_root) else {
            return false;
        };
        let Ok(root_canonical) = canonical::canonicalize(&root_uri) else {
            return false;
        };
        let Ok(root_path) = canonical::fs_path(&root_canonical) else {
            return false;
        };
        target.starts_with(root_path)
    }

    fn build_folder_dto(&self, folder: &FolderRow) -> FolderDto {
        // Blank (empty or whitespace-only) basenames collapse to `None`, matching
        // the source-control seam's `pe_name`/`label` filter. A folder literally
        // named "   " would otherwise surface a whitespace default name, and the
        // scm label fallback (`display_name || default_display_name || pe_id`)
        // would render it blank — breaking the "label is never blank" contract.
        let default_display_name = canonical::canonicalize(&folder.resource_canonical)
            .ok()
            .map(|c| canonical::basename(&c))
            .filter(|name| !name.trim().is_empty());
        let (runtime_status, runtime_error) = runtime_status_of(folder);
        FolderDto {
            folder_id: folder.folder_id.clone(),
            resource_uri: folder.resource_uri.clone(),
            resource_canonical: folder.resource_canonical.clone(),
            default_display_name,
            runtime_status,
            runtime_error,
        }
    }
}

/// The final path segment of a directory, used as a temp project name.
fn leaf_of(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Compute a folder's runtime availability by stat (never persisted).
fn runtime_status_of(folder: &FolderRow) -> (RuntimeStatus, Option<String>) {
    let Ok(canonical) = canonical::canonicalize(&folder.resource_canonical) else {
        return (RuntimeStatus::Missing, Some("invalid canonical resource".to_owned()));
    };
    let Ok(path) = canonical::fs_path(&canonical) else {
        return (RuntimeStatus::Missing, Some("not a file: resource".to_owned()));
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => (RuntimeStatus::Available, None),
        Ok(_) => (RuntimeStatus::Missing, Some("not a directory".to_owned())),
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => (RuntimeStatus::Missing, None),
            std::io::ErrorKind::PermissionDenied => (RuntimeStatus::PermissionDenied, None),
            _ => (RuntimeStatus::Missing, Some(err.to_string())),
        },
    }
}
