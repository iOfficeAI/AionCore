use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};

use aionui_api_types::{
    InstallOfficialSkillRequest, OfficialSkillDetail, OfficialSkillFile, OfficialSkillInstallStatus,
    OfficialSkillInstallationResponse, OfficialSkillSearchQuery, OfficialSkillSearchResponse, OfficialSkillSummary,
    OfficialSkillVersionResponse,
};
use aionui_db::{
    ISkillRepository, SkillRegistryInstallRow, SkillRow, UpsertSkillParams, UpsertSkillRegistryInstallParams,
};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::skill_service::{self, SkillPaths};

use super::client::{SkillHubClient, SkillHubClientError};

pub const CSBU_SKILLHUB_REGISTRY_KEY: &str = "csbu-skillhub";
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SkillRegistryError {
    #[error("Invalid SkillHub request")]
    InvalidRequest,
    #[error("SkillHub is unavailable")]
    Unavailable,
    #[error("SkillHub request timed out")]
    Timeout,
    #[error("The requested SkillHub version is unavailable")]
    VersionNotFound,
    #[error("The SkillHub package is invalid")]
    PackageInvalid,
    #[error("The SkillHub package failed integrity verification")]
    HashMismatch,
    #[error("A local skill already uses this name")]
    NameConflict { skill_name: String },
    #[error("A SkillHub operation is already running")]
    OperationInProgress,
    #[error("The SkillHub installation was not found")]
    InstallationNotFound,
    #[error("SkillHub persistence failed")]
    Persistence,
}

impl From<SkillHubClientError> for SkillRegistryError {
    fn from(value: SkillHubClientError) -> Self {
        match value {
            SkillHubClientError::Timeout => Self::Timeout,
            SkillHubClientError::NotFound => Self::VersionNotFound,
            SkillHubClientError::PackageTooLarge | SkillHubClientError::Io => Self::PackageInvalid,
            SkillHubClientError::Unavailable | SkillHubClientError::InvalidResponse => Self::Unavailable,
        }
    }
}

#[derive(Clone)]
pub struct SkillRegistryService {
    client: SkillHubClient,
    paths: SkillPaths,
    repo: Arc<dyn ISkillRepository>,
    operations: Arc<Mutex<HashSet<String>>>,
}

struct OperationGuard {
    operations: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Ok(mut operations) = self.operations.lock() {
            operations.remove(&self.key);
        }
    }
}

impl SkillRegistryService {
    pub fn new(client: SkillHubClient, paths: SkillPaths, repo: Arc<dyn ISkillRepository>) -> Self {
        Self {
            client,
            paths,
            repo,
            operations: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn search(
        &self,
        user_id: &str,
        query: &OfficialSkillSearchQuery,
    ) -> Result<OfficialSkillSearchResponse, SkillRegistryError> {
        validate_query(query)?;
        let mut response = self.client.search(query).await?;
        let installs = self.install_map(user_id).await?;
        for item in &mut response.items {
            apply_install_status(item, installs.get(&(item.namespace.clone(), item.slug.clone())));
        }
        Ok(response)
    }

    pub async fn detail(
        &self,
        user_id: &str,
        namespace: &str,
        slug: &str,
    ) -> Result<OfficialSkillDetail, SkillRegistryError> {
        validate_identity(namespace, slug)?;
        let install = self
            .repo
            .find_registry_install_for_user(user_id, CSBU_SKILLHUB_REGISTRY_KEY, namespace, slug)
            .await
            .map_err(|_| SkillRegistryError::Persistence)?;
        match self.client.detail(namespace, slug).await {
            Ok(mut detail) => {
                apply_install_status(&mut detail.skill, install.as_ref());
                Ok(detail)
            }
            Err(SkillHubClientError::NotFound) => match install.as_ref() {
                Some(install) => self.unavailable_detail(user_id, install).await,
                None => Err(SkillRegistryError::VersionNotFound),
            },
            Err(error) => Err(error.into()),
        }
    }

    pub async fn files(
        &self,
        namespace: &str,
        slug: &str,
        version: &str,
    ) -> Result<Vec<OfficialSkillFile>, SkillRegistryError> {
        validate_identity(namespace, slug)?;
        validate_segment(version)?;
        let detail = self.client.detail(namespace, slug).await?;
        if detail.skill.published_version.version != version {
            return Err(SkillRegistryError::VersionNotFound);
        }
        self.client.files(namespace, slug, version).await.map_err(Into::into)
    }

    pub async fn updates(&self, user_id: &str) -> Result<Vec<OfficialSkillSummary>, SkillRegistryError> {
        let installs = self
            .repo
            .list_registry_installs_for_user(user_id)
            .await
            .map_err(|_| SkillRegistryError::Persistence)?;
        let mut updates = Vec::new();
        for install in installs {
            match self.client.detail(&install.namespace, &install.slug).await {
                Ok(mut detail) => {
                    apply_install_status(&mut detail.skill, Some(&install));
                    if detail.skill.install_status == OfficialSkillInstallStatus::UpdateAvailable {
                        updates.push(detail.skill);
                    }
                }
                Err(SkillHubClientError::NotFound) => {
                    updates.push(self.unavailable_summary(user_id, &install).await?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(updates)
    }

    async fn unavailable_detail(
        &self,
        user_id: &str,
        install: &SkillRegistryInstallRow,
    ) -> Result<OfficialSkillDetail, SkillRegistryError> {
        Ok(OfficialSkillDetail {
            skill: self.unavailable_summary(user_id, install).await?,
            owner_display_name: "CSBU SkillHub".into(),
            labels: Vec::new(),
        })
    }

    async fn unavailable_summary(
        &self,
        user_id: &str,
        install: &SkillRegistryInstallRow,
    ) -> Result<OfficialSkillSummary, SkillRegistryError> {
        let local = self
            .repo
            .list_for_user(user_id)
            .await
            .map_err(|_| SkillRegistryError::Persistence)?
            .into_iter()
            .find(|row| row.id == install.skill_id);
        Ok(OfficialSkillSummary {
            id: install.remote_skill_id,
            namespace: install.namespace.clone(),
            slug: install.slug.clone(),
            display_name: local
                .as_ref()
                .map(|row| row.name.clone())
                .unwrap_or_else(|| install.slug.clone()),
            summary: local.and_then(|row| row.description).unwrap_or_default(),
            download_count: 0,
            star_count: 0,
            updated_at: String::new(),
            published_version: OfficialSkillVersionResponse {
                id: install.remote_version_id,
                version: install.installed_version.clone(),
                status: "UNAVAILABLE".into(),
            },
            install_status: OfficialSkillInstallStatus::Unavailable,
            installed_version: Some(install.installed_version.clone()),
        })
    }

    pub async fn install(
        &self,
        user_id: &str,
        request: &InstallOfficialSkillRequest,
    ) -> Result<OfficialSkillInstallationResponse, SkillRegistryError> {
        self.run_operation(user_id, request, false).await
    }

    pub async fn update(
        &self,
        user_id: &str,
        namespace: &str,
        slug: &str,
        version: &str,
    ) -> Result<OfficialSkillInstallationResponse, SkillRegistryError> {
        let request = InstallOfficialSkillRequest {
            namespace: namespace.to_owned(),
            slug: slug.to_owned(),
            version: version.to_owned(),
        };
        self.run_operation(user_id, &request, true).await
    }

    async fn run_operation(
        &self,
        user_id: &str,
        request: &InstallOfficialSkillRequest,
        is_update: bool,
    ) -> Result<OfficialSkillInstallationResponse, SkillRegistryError> {
        validate_identity(&request.namespace, &request.slug)?;
        validate_segment(&request.version)?;
        let operation_key = format!("{user_id}:{}:{}", request.namespace, request.slug);
        {
            let mut operations = self.operations.lock().map_err(|_| SkillRegistryError::Persistence)?;
            if !operations.insert(operation_key.clone()) {
                return Err(SkillRegistryError::OperationInProgress);
            }
        }
        let _guard = OperationGuard {
            operations: self.operations.clone(),
            key: operation_key,
        };
        self.run_operation_inner(user_id, request, is_update).await
    }

    async fn run_operation_inner(
        &self,
        user_id: &str,
        request: &InstallOfficialSkillRequest,
        is_update: bool,
    ) -> Result<OfficialSkillInstallationResponse, SkillRegistryError> {
        let existing_install = self
            .repo
            .find_registry_install_for_user(user_id, CSBU_SKILLHUB_REGISTRY_KEY, &request.namespace, &request.slug)
            .await
            .map_err(|_| SkillRegistryError::Persistence)?;
        if is_update && existing_install.is_none() {
            return Err(SkillRegistryError::InstallationNotFound);
        }
        if !is_update && existing_install.is_some() {
            return Err(SkillRegistryError::NameConflict {
                skill_name: request.slug.clone(),
            });
        }

        let detail = self.client.detail(&request.namespace, &request.slug).await?;
        if detail.skill.published_version.version != request.version {
            return Err(SkillRegistryError::VersionNotFound);
        }
        let files = self
            .client
            .files(&request.namespace, &request.slug, &request.version)
            .await?;
        if files.is_empty() {
            return Err(SkillRegistryError::PackageInvalid);
        }

        let temp_root = self.paths.user_skills_dir.join(".import-tmp");
        tokio::fs::create_dir_all(&temp_root)
            .await
            .map_err(|_| SkillRegistryError::PackageInvalid)?;
        let archive_path = temp_root.join(format!(
            "registry-{}-{}.zip",
            std::process::id(),
            aionui_common::generate_short_id()
        ));
        let operation = async {
            self.client
                .download(&request.namespace, &request.slug, &request.version, &archive_path)
                .await?;
            let archive = archive_path.clone();
            let expected = files.clone();
            let skill_name = tokio::task::spawn_blocking(move || verify_archive(&archive, &expected))
                .await
                .map_err(|_| SkillRegistryError::PackageInvalid)??;

            let old_row = if let Some(install) = existing_install.as_ref() {
                self.repo
                    .list_for_user(user_id)
                    .await
                    .map_err(|_| SkillRegistryError::Persistence)?
                    .into_iter()
                    .find(|row| row.id == install.skill_id)
            } else {
                self.repo
                    .find_by_name_for_user(user_id, &skill_name)
                    .await
                    .map_err(|_| SkillRegistryError::Persistence)?
            };
            if is_update && old_row.is_none() {
                return Err(SkillRegistryError::InstallationNotFound);
            }
            if let Some(row) = old_row.as_ref() {
                let is_same_install = existing_install
                    .as_ref()
                    .is_some_and(|install| install.skill_id == row.id);
                if !is_same_install {
                    return Err(SkillRegistryError::NameConflict {
                        skill_name: skill_name.clone(),
                    });
                }
                if is_update && row.name != skill_name {
                    return Err(SkillRegistryError::PackageInvalid);
                }
            }

            let target_path = skill_service::user_skill_root_for_user(&self.paths, user_id).join(&skill_name);
            let backup_path = if is_update {
                let backup = temp_root.join(format!(
                    "registry-backup-{}-{}",
                    std::process::id(),
                    aionui_common::generate_short_id()
                ));
                tokio::fs::rename(&target_path, &backup)
                    .await
                    .map_err(|_| SkillRegistryError::PackageInvalid)?;
                Some(backup)
            } else {
                None
            };

            let apply_result = async {
                let outcome = skill_service::import_skills_with_repo_for_user(
                    &self.paths,
                    self.repo.as_ref(),
                    user_id,
                    &archive_path,
                )
                .await
                .map_err(|_| SkillRegistryError::PackageInvalid)?;
                if !outcome.failed.is_empty() || outcome.imported.len() != 1 || outcome.imported[0] != skill_name {
                    return Err(SkillRegistryError::PackageInvalid);
                }
                let skill_row = self
                    .repo
                    .find_by_name_for_user(user_id, &skill_name)
                    .await
                    .map_err(|_| SkillRegistryError::Persistence)?
                    .ok_or(SkillRegistryError::Persistence)?;
                self.repo
                    .upsert_registry_install_for_user(
                        user_id,
                        UpsertSkillRegistryInstallParams {
                            skill_id: &skill_row.id,
                            registry_key: CSBU_SKILLHUB_REGISTRY_KEY,
                            namespace: &request.namespace,
                            slug: &request.slug,
                            remote_skill_id: detail.skill.id,
                            remote_version_id: detail.skill.published_version.id,
                            installed_version: &request.version,
                        },
                    )
                    .await
                    .map_err(|_| SkillRegistryError::Persistence)?;
                Ok::<(), SkillRegistryError>(())
            }
            .await;

            if let Err(error) = apply_result {
                self.rollback_operation(
                    user_id,
                    &skill_name,
                    &target_path,
                    backup_path.as_deref(),
                    old_row.as_ref(),
                    existing_install.as_ref(),
                )
                .await;
                return Err(error);
            }
            if let Some(backup) = backup_path.as_ref()
                && let Err(error) = remove_path(backup).await
            {
                warn!(path = %backup.display(), error = %error, "failed to clean SkillHub update backup");
            }
            info!(
                namespace = %request.namespace,
                slug = %request.slug,
                version = %request.version,
                operation = if is_update { "update" } else { "install" },
                "official SkillHub skill operation completed"
            );
            Ok(OfficialSkillInstallationResponse {
                skill_name,
                namespace: request.namespace.clone(),
                slug: request.slug.clone(),
                installed_version: request.version.clone(),
            })
        }
        .await;
        if let Err(error) = tokio::fs::remove_file(&archive_path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(error = %error, "failed to clean SkillHub download archive");
        }
        operation
    }

    async fn rollback_operation(
        &self,
        user_id: &str,
        skill_name: &str,
        target_path: &Path,
        backup_path: Option<&Path>,
        old_row: Option<&SkillRow>,
        old_install: Option<&SkillRegistryInstallRow>,
    ) {
        if let Err(error) = remove_path(target_path).await {
            warn!(path = %target_path.display(), error = %error, "failed to remove invalid SkillHub install");
        }
        if let Some(backup) = backup_path
            && let Err(error) = tokio::fs::rename(backup, target_path).await
        {
            warn!(path = %backup.display(), error = %error, "failed to restore SkillHub update backup");
        }
        if let Some(row) = old_row {
            if let Err(error) = self
                .repo
                .upsert_for_user(
                    user_id,
                    UpsertSkillParams {
                        name: &row.name,
                        description: row.description.as_deref(),
                        path: &row.path,
                        source: &row.source,
                        enabled: row.enabled,
                    },
                )
                .await
            {
                warn!(skill = %skill_name, error = %error, "failed to restore SkillHub skill metadata");
            }
        } else if self
            .repo
            .find_by_name_for_user(user_id, skill_name)
            .await
            .ok()
            .flatten()
            .is_some()
            && let Err(error) = self.repo.delete_by_name_for_user(user_id, skill_name).await
        {
            warn!(skill = %skill_name, error = %error, "failed to roll back SkillHub skill metadata");
        }
        if let Some(install) = old_install
            && let Err(error) = self
                .repo
                .upsert_registry_install_for_user(
                    user_id,
                    UpsertSkillRegistryInstallParams {
                        skill_id: &install.skill_id,
                        registry_key: &install.registry_key,
                        namespace: &install.namespace,
                        slug: &install.slug,
                        remote_skill_id: install.remote_skill_id,
                        remote_version_id: install.remote_version_id,
                        installed_version: &install.installed_version,
                    },
                )
                .await
        {
            warn!(skill = %skill_name, error = %error, "failed to restore SkillHub provenance metadata");
        }
    }

    async fn install_map(
        &self,
        user_id: &str,
    ) -> Result<HashMap<(String, String), aionui_db::SkillRegistryInstallRow>, SkillRegistryError> {
        Ok(self
            .repo
            .list_registry_installs_for_user(user_id)
            .await
            .map_err(|_| SkillRegistryError::Persistence)?
            .into_iter()
            .map(|install| ((install.namespace.clone(), install.slug.clone()), install))
            .collect())
    }
}

fn validate_query(query: &OfficialSkillSearchQuery) -> Result<(), SkillRegistryError> {
    if query.size == 0
        || query.size > 50
        || !matches!(query.sort.as_str(), "newest" | "downloads" | "stars" | "relevance")
    {
        return Err(SkillRegistryError::InvalidRequest);
    }
    Ok(())
}

async fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    }
}

fn validate_identity(namespace: &str, slug: &str) -> Result<(), SkillRegistryError> {
    if namespace != "global" {
        return Err(SkillRegistryError::InvalidRequest);
    }
    validate_segment(slug)
}

fn validate_segment(value: &str) -> Result<(), SkillRegistryError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(SkillRegistryError::InvalidRequest);
    }
    Ok(())
}

fn apply_install_status(summary: &mut OfficialSkillSummary, install: Option<&aionui_db::SkillRegistryInstallRow>) {
    if let Some(install) = install {
        summary.installed_version = Some(install.installed_version.clone());
        summary.install_status = if install.installed_version == summary.published_version.version {
            OfficialSkillInstallStatus::Installed
        } else {
            OfficialSkillInstallStatus::UpdateAvailable
        };
    }
}

fn verify_archive(archive_path: &Path, expected_files: &[OfficialSkillFile]) -> Result<String, SkillRegistryError> {
    let file = std::fs::File::open(archive_path).map_err(|_| SkillRegistryError::PackageInvalid)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| SkillRegistryError::PackageInvalid)?;
    let mut expected = HashMap::new();
    let mut expected_portable_paths = HashSet::new();
    for file in expected_files {
        let path = normalize_zip_path(&file.file_path)?;
        let portable_path = portable_path_key(&path)?;
        if !expected_portable_paths.insert(portable_path) || expected.insert(path, file).is_some() {
            return Err(SkillRegistryError::PackageInvalid);
        }
    }

    let mut raw_names = Vec::new();
    let mut archive_paths = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| SkillRegistryError::PackageInvalid)?;
        let name = normalize_zip_path(entry.name())?;
        let portable_path = portable_path_key(&name)?;
        if !archive_paths.insert(portable_path) {
            return Err(SkillRegistryError::PackageInvalid);
        }
        if !entry.is_dir() {
            raw_names.push(name);
        }
    }
    let prefix = shared_wrapper_prefix(&raw_names, &expected);
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    let mut manifest = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| SkillRegistryError::PackageInvalid)?;
        if entry.is_dir() {
            continue;
        }
        if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
            return Err(SkillRegistryError::PackageInvalid);
        }
        let raw_name = normalize_zip_path(entry.name())?;
        let name = prefix
            .as_deref()
            .and_then(|prefix| raw_name.strip_prefix(prefix))
            .unwrap_or(&raw_name)
            .trim_start_matches('/')
            .to_owned();
        if !seen.insert(portable_path_key(&name)?) {
            return Err(SkillRegistryError::PackageInvalid);
        }
        let expected_file = expected.get(&name).ok_or(SkillRegistryError::PackageInvalid)?;
        if entry.size() != expected_file.file_size || entry.size() > MAX_FILE_BYTES {
            return Err(SkillRegistryError::PackageInvalid);
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return Err(SkillRegistryError::PackageInvalid);
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|_| SkillRegistryError::PackageInvalid)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        if !digest.eq_ignore_ascii_case(&expected_file.sha256) {
            return Err(SkillRegistryError::HashMismatch);
        }
        let portable_name = portable_path_key(&name)?;
        if portable_name.ends_with("/skill.md") || (portable_name == "skill.md" && name != "SKILL.md") {
            return Err(SkillRegistryError::PackageInvalid);
        }
        if name == "SKILL.md" {
            if manifest.is_some() {
                return Err(SkillRegistryError::PackageInvalid);
            }
            manifest = Some(String::from_utf8(bytes).map_err(|_| SkillRegistryError::PackageInvalid)?);
        }
    }
    if seen.len() != expected.len() {
        return Err(SkillRegistryError::PackageInvalid);
    }
    let manifest = manifest.ok_or(SkillRegistryError::PackageInvalid)?;
    skill_service::read_skill_info_from_content(&manifest)
        .map(|(name, _)| name)
        .map_err(|_| SkillRegistryError::PackageInvalid)
}

fn normalize_zip_path(path: &str) -> Result<String, SkillRegistryError> {
    let normalized = path.replace('\\', "/");
    let parsed = Path::new(&normalized);
    if parsed.is_absolute()
        || parsed.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(SkillRegistryError::PackageInvalid);
    }
    Ok(normalized.trim_start_matches("./").trim_start_matches('/').to_owned())
}

fn portable_path_key(path: &str) -> Result<String, SkillRegistryError> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err(SkillRegistryError::PackageInvalid);
    }
    let mut normalized = Vec::new();
    for segment in path.split('/') {
        let segment = segment.trim_end_matches(|character| character == '.' || character == ' ');
        if segment.is_empty()
            || segment == "."
            || segment
                .chars()
                .any(|character| character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return Err(SkillRegistryError::PackageInvalid);
        }
        let device_name = segment.split('.').next().unwrap_or_default().to_ascii_uppercase();
        if matches!(device_name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (device_name.len() == 4
                && (device_name.starts_with("COM") || device_name.starts_with("LPT"))
                && matches!(device_name.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(SkillRegistryError::PackageInvalid);
        }
        normalized.push(segment.to_lowercase());
    }
    Ok(normalized.join("/"))
}

fn shared_wrapper_prefix(raw_names: &[String], expected: &HashMap<String, &OfficialSkillFile>) -> Option<String> {
    if raw_names.iter().all(|name| expected.contains_key(name)) {
        return None;
    }
    let first = raw_names.first()?.split('/').next()?;
    let prefix = format!("{first}/");
    if raw_names.iter().all(|name| {
        name.strip_prefix(&prefix)
            .is_some_and(|stripped| expected.contains_key(stripped))
    }) {
        Some(prefix)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    use aionui_db::SqliteSkillRepository;
    use axum::{Json, Router, body::Bytes, http::StatusCode, response::IntoResponse, routing::get};
    use serde_json::json;

    fn write_archive(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let writer = std::fs::File::create(file.path()).unwrap();
        let mut archive = zip::ZipWriter::new(writer);
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            archive.start_file(*name, options).unwrap();
            archive.write_all(content).unwrap();
        }
        archive.finish().unwrap();
        file
    }

    fn manifest_file(content: &[u8]) -> OfficialSkillFile {
        OfficialSkillFile {
            id: 1,
            file_path: "SKILL.md".into(),
            file_size: content.len() as u64,
            content_type: "text/markdown".into(),
            sha256: hex::encode(Sha256::digest(content)),
        }
    }

    fn test_paths(root: &Path) -> SkillPaths {
        SkillPaths {
            data_dir: root.to_path_buf(),
            user_skills_dir: root.join("skills"),
            cron_skills_dir: root.join("cron-skills"),
            builtin_skills_dir: root.join("builtin-skills"),
            builtin_rules_dir: root.join("builtin-rules"),
            assistant_rules_dir: root.join("assistant-rules"),
            assistant_skills_dir: root.join("assistant-skills"),
        }
    }

    fn archive_bytes(manifest: &[u8]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        archive
            .start_file("SKILL.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(manifest).unwrap();
        archive.finish().unwrap().into_inner()
    }

    async fn registry_fixture(version: &str, manifest: &[u8], advertised_hash: String) -> String {
        let version = version.to_owned();
        let manifest_size = manifest.len() as u64;
        let archive = archive_bytes(manifest);
        let detail_version = version.clone();
        let files_version = version.clone();
        let app = Router::new()
            .route(
                "/api/web/skills/global/fixture",
                get(move || {
                    let version = detail_version.clone();
                    async move {
                        Json(json!({
                            "data": {
                                "id": 1, "slug": "fixture", "displayName": "Fixture", "ownerDisplayName": "CSBU",
                                "summary": "Fixture", "visibility": "PUBLIC", "status": "ACTIVE",
                                "downloadCount": 1, "starCount": 1, "namespace": "global",
                                "updatedAt": "2026-01-01T00:00:00Z", "labels": [],
                                "publishedVersion": { "id": 10, "version": version, "status": "PUBLISHED" }
                            }
                        }))
                    }
                }),
            )
            .route(
                "/api/web/skills/global/fixture/versions/{version}/files",
                get(move |axum::extract::Path(requested): axum::extract::Path<String>| {
                    let expected_version = files_version.clone();
                    let hash = advertised_hash.clone();
                    async move {
                        if requested != expected_version {
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        Json(json!({
                            "data": [{
                                "id": 10, "filePath": "SKILL.md", "fileSize": manifest_size,
                                "contentType": "text/markdown", "sha256": hash
                            }]
                        }))
                        .into_response()
                    }
                }),
            )
            .route(
                "/api/web/skills/global/fixture/versions/{version}/download",
                get(move || {
                    let archive = archive.clone();
                    async move { Bytes::from(archive) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    #[test]
    fn validates_supported_search_options() {
        let valid = OfficialSkillSearchQuery {
            q: String::new(),
            sort: "newest".into(),
            page: 0,
            size: 20,
        };
        assert!(validate_query(&valid).is_ok());
        let invalid = OfficialSkillSearchQuery {
            sort: "unknown".into(),
            ..valid
        };
        assert!(validate_query(&invalid).is_err());
    }

    #[test]
    fn rejects_non_global_or_unsafe_identity() {
        assert!(validate_identity("global", "safe-skill").is_ok());
        assert!(validate_identity("private", "safe-skill").is_err());
        assert!(validate_identity("global", "../unsafe").is_err());
    }

    #[test]
    fn verifies_manifest_size_and_hash() {
        let manifest = b"---\nname: fixture-skill\ndescription: Fixture\n---\n";
        let archive = write_archive(&[("SKILL.md", manifest)]);
        assert_eq!(
            verify_archive(archive.path(), &[manifest_file(manifest)]).unwrap(),
            "fixture-skill"
        );

        let mut wrong_hash = manifest_file(manifest);
        wrong_hash.sha256 = "00".repeat(32);
        assert!(matches!(
            verify_archive(archive.path(), &[wrong_hash]),
            Err(SkillRegistryError::HashMismatch)
        ));
    }

    #[test]
    fn rejects_zip_slip_and_unlisted_files() {
        let manifest = b"---\nname: fixture-skill\ndescription: Fixture\n---\n";
        let slipped = write_archive(&[("../SKILL.md", manifest)]);
        assert!(matches!(
            verify_archive(slipped.path(), &[manifest_file(manifest)]),
            Err(SkillRegistryError::PackageInvalid)
        ));

        let extra = write_archive(&[("SKILL.md", manifest), ("secret.txt", b"secret")]);
        assert!(matches!(
            verify_archive(extra.path(), &[manifest_file(manifest)]),
            Err(SkillRegistryError::PackageInvalid)
        ));
    }

    #[test]
    fn rejects_windows_path_collisions_and_reserved_names() {
        let manifest = b"---\nname: fixture-skill\ndescription: Fixture\n---\n";
        let lower_manifest = b"not a valid manifest";
        let collision = write_archive(&[("SKILL.md", manifest), ("skill.md", lower_manifest)]);
        let files = [
            manifest_file(manifest),
            OfficialSkillFile {
                id: 2,
                file_path: "skill.md".into(),
                file_size: lower_manifest.len() as u64,
                content_type: "text/markdown".into(),
                sha256: hex::encode(Sha256::digest(lower_manifest)),
            },
        ];
        assert!(matches!(
            verify_archive(collision.path(), &files),
            Err(SkillRegistryError::PackageInvalid)
        ));
        assert!(portable_path_key("assets/CON.txt").is_err());
        assert!(portable_path_key("assets/file. ").is_ok());
        assert_eq!(
            portable_path_key("Assets/FILE.txt").unwrap(),
            portable_path_key("assets/file.txt").unwrap()
        );
    }

    #[tokio::test]
    async fn installs_and_updates_a_registry_skill_with_provenance() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(temp.path());
        let database = aionui_db::init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteSkillRepository::new(database.pool().clone()));
        let first = b"---\nname: fixture-skill\ndescription: First\n---\nfirst\n";
        let first_fixture = registry_fixture("1.0", first, hex::encode(Sha256::digest(first))).await;
        let service = SkillRegistryService::new(
            SkillHubClient::for_test(first_fixture, Duration::from_secs(2)).unwrap(),
            paths.clone(),
            repo.clone(),
        );
        service
            .install(
                "system_default_user",
                &InstallOfficialSkillRequest {
                    namespace: "global".into(),
                    slug: "fixture".into(),
                    version: "1.0".into(),
                },
            )
            .await
            .unwrap();

        let second = b"---\nname: fixture-skill\ndescription: Second\n---\nsecond\n";
        let second_fixture = registry_fixture("2.0", second, hex::encode(Sha256::digest(second))).await;
        let update_service = SkillRegistryService::new(
            SkillHubClient::for_test(second_fixture, Duration::from_secs(2)).unwrap(),
            paths.clone(),
            repo.clone(),
        );
        update_service
            .update("system_default_user", "global", "fixture", "2.0")
            .await
            .unwrap();

        let install = repo
            .find_registry_install_for_user("system_default_user", CSBU_SKILLHUB_REGISTRY_KEY, "global", "fixture")
            .await
            .unwrap()
            .unwrap();
        let content = tokio::fs::read_to_string(
            skill_service::user_skill_root_for_user(&paths, "system_default_user").join("fixture-skill/SKILL.md"),
        )
        .await
        .unwrap();
        assert_eq!(install.installed_version, "2.0");
        assert!(content.contains("second"));
    }

    #[tokio::test]
    async fn files_are_only_exposed_for_the_current_published_version() {
        let temp = tempfile::TempDir::new().unwrap();
        let database = aionui_db::init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteSkillRepository::new(database.pool().clone()));
        let manifest = b"---\nname: fixture-skill\ndescription: Fixture\n---\n";
        let fixture = registry_fixture("2.0", manifest, hex::encode(Sha256::digest(manifest))).await;
        let service = SkillRegistryService::new(
            SkillHubClient::for_test(fixture, Duration::from_secs(2)).unwrap(),
            test_paths(temp.path()),
            repo,
        );

        let result = service.files("global", "fixture", "1.0").await;

        assert!(matches!(result, Err(SkillRegistryError::VersionNotFound)));
    }

    #[tokio::test]
    async fn failed_update_keeps_old_files_and_version() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(temp.path());
        let database = aionui_db::init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteSkillRepository::new(database.pool().clone()));
        let first = b"---\nname: fixture-skill\ndescription: First\n---\nfirst\n";
        let first_fixture = registry_fixture("1.0", first, hex::encode(Sha256::digest(first))).await;
        SkillRegistryService::new(
            SkillHubClient::for_test(first_fixture, Duration::from_secs(2)).unwrap(),
            paths.clone(),
            repo.clone(),
        )
        .install(
            "system_default_user",
            &InstallOfficialSkillRequest {
                namespace: "global".into(),
                slug: "fixture".into(),
                version: "1.0".into(),
            },
        )
        .await
        .unwrap();

        let second = b"---\nname: fixture-skill\ndescription: Second\n---\nsecond\n";
        let bad_fixture = registry_fixture("2.0", second, "00".repeat(32)).await;
        let result = SkillRegistryService::new(
            SkillHubClient::for_test(bad_fixture, Duration::from_secs(2)).unwrap(),
            paths.clone(),
            repo.clone(),
        )
        .update("system_default_user", "global", "fixture", "2.0")
        .await;

        let install = repo
            .find_registry_install_for_user("system_default_user", CSBU_SKILLHUB_REGISTRY_KEY, "global", "fixture")
            .await
            .unwrap()
            .unwrap();
        let content = tokio::fs::read_to_string(
            skill_service::user_skill_root_for_user(&paths, "system_default_user").join("fixture-skill/SKILL.md"),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(SkillRegistryError::HashMismatch)));
        assert_eq!(install.installed_version, "1.0");
        assert!(content.contains("first"));
    }

    #[tokio::test]
    async fn returns_unavailable_status_for_withdrawn_installed_skill() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(temp.path());
        let database = aionui_db::init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteSkillRepository::new(database.pool().clone()));
        let local_path = temp.path().join("fixture-skill").to_string_lossy().into_owned();
        let skill = repo
            .upsert_for_user(
                "system_default_user",
                UpsertSkillParams {
                    name: "fixture-skill",
                    description: Some("Offline copy"),
                    path: &local_path,
                    source: "user",
                    enabled: true,
                },
            )
            .await
            .unwrap();
        repo.upsert_registry_install_for_user(
            "system_default_user",
            UpsertSkillRegistryInstallParams {
                skill_id: &skill.id,
                registry_key: CSBU_SKILLHUB_REGISTRY_KEY,
                namespace: "global",
                slug: "fixture",
                remote_skill_id: 1,
                remote_version_id: 10,
                installed_version: "1.0",
            },
        )
        .await
        .unwrap();
        let app = Router::new().route(
            "/api/web/skills/global/fixture",
            get(|| async { StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let service = SkillRegistryService::new(
            SkillHubClient::for_test(format!("http://{address}"), Duration::from_secs(2)).unwrap(),
            paths,
            repo,
        );

        let detail = service
            .detail("system_default_user", "global", "fixture")
            .await
            .unwrap();
        assert_eq!(detail.skill.install_status, OfficialSkillInstallStatus::Unavailable);
        assert_eq!(detail.skill.installed_version.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn rejects_a_second_operation_for_the_same_user_and_skill() {
        let temp = tempfile::TempDir::new().unwrap();
        let database = aionui_db::init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteSkillRepository::new(database.pool().clone()));
        let service = SkillRegistryService::new(
            SkillHubClient::for_test("http://127.0.0.1:1", Duration::from_millis(20)).unwrap(),
            test_paths(temp.path()),
            repo,
        );
        service
            .operations
            .lock()
            .unwrap()
            .insert("system_default_user:global:fixture".into());

        let result = service
            .install(
                "system_default_user",
                &InstallOfficialSkillRequest {
                    namespace: "global".into(),
                    slug: "fixture".into(),
                    version: "1.0".into(),
                },
            )
            .await;

        assert!(matches!(result, Err(SkillRegistryError::OperationInProgress)));
    }
}
