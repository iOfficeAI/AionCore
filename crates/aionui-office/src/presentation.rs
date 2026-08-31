use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    PresentationAssetImportResponse, PresentationCatalogResponse, PresentationRenderJob, PresentationRenderStatus,
    PresentationValidationResponse,
};
use aionui_runtime::Builder as CmdBuilder;
use dashmap::DashMap;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::error::OfficeError;
use crate::officecli_runtime::resolve_officecli_path;

const MAX_SPEC_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 25 * 1024 * 1024;
const MAX_TOTAL_ASSET_BYTES: u64 = 200 * 1024 * 1024;
const MAX_SAFE_REVISION: u64 = 9_007_199_254_740_991;
const OFFICECLI_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct PresentationService {
    officecli_path: Option<PathBuf>,
    catalog_cache: Arc<RwLock<Option<PresentationCatalogResponse>>>,
    jobs: Arc<DashMap<String, PresentationRenderJob>>,
    job_owners: Arc<DashMap<String, String>>,
    job_decks: Arc<DashMap<String, PathBuf>>,
    active_by_deck: Arc<DashMap<PathBuf, String>>,
    abort_handles: Arc<DashMap<String, AbortHandle>>,
}

impl PresentationService {
    pub fn new(officecli_path: Option<PathBuf>) -> Self {
        Self {
            officecli_path,
            catalog_cache: Arc::new(RwLock::new(None)),
            jobs: Arc::new(DashMap::new()),
            job_owners: Arc::new(DashMap::new()),
            job_decks: Arc::new(DashMap::new()),
            active_by_deck: Arc::new(DashMap::new()),
            abort_handles: Arc::new(DashMap::new()),
        }
    }

    pub async fn catalog(&self) -> Result<PresentationCatalogResponse, OfficeError> {
        if let Some(catalog) = self.catalog_cache.read().await.clone() {
            return Ok(catalog);
        }
        let value = self.run_json(["deck", "catalog", "--json"]).await?;
        let catalog: PresentationCatalogResponse = serde_json::from_value(value).map_err(OfficeError::Json)?;
        *self.catalog_cache.write().await = Some(catalog.clone());
        Ok(catalog)
    }

    pub async fn validate(&self, spec_path: &Path) -> Result<PresentationValidationResponse, OfficeError> {
        validate_spec_file(spec_path)?;
        let spec = spec_path.to_string_lossy().into_owned();
        let value = self.run_json(["deck", "validate", &spec, "--json"]).await?;
        serde_json::from_value(value).map_err(OfficeError::Json)
    }

    pub fn import_asset(
        &self,
        spec_path: &Path,
        source_path: &Path,
        asset_id: &str,
    ) -> Result<PresentationAssetImportResponse, OfficeError> {
        validate_spec_file(spec_path)?;
        validate_asset_id(asset_id)?;
        let source_metadata = std::fs::metadata(source_path)?;
        if !source_metadata.is_file() {
            return Err(OfficeError::Presentation(
                "presentation asset source is not a file".into(),
            ));
        }
        if source_metadata.len() > MAX_ASSET_BYTES {
            return Err(OfficeError::Presentation(
                "presentation asset exceeds the 25 MB limit".into(),
            ));
        }
        let extension = validate_image_file(source_path)?;
        let (asset_directory, relative_directory) = asset_directory_for(spec_path)?;
        std::fs::create_dir_all(&asset_directory)?;
        ensure_asset_directory_contained(spec_path, &asset_directory)?;
        let asset_file_name = format!("{asset_id}-{}.{extension}", Uuid::now_v7());
        let target = asset_directory.join(&asset_file_name);
        let current_total = asset_directory_size(&asset_directory)?;
        if current_total.saturating_add(source_metadata.len()) > MAX_TOTAL_ASSET_BYTES {
            return Err(OfficeError::Presentation(
                "presentation assets exceed the 200 MB total limit".into(),
            ));
        }
        copy_asset_atomic(source_path, &target)?;
        Ok(PresentationAssetImportResponse {
            asset_path: format!("{relative_directory}/{asset_file_name}"),
            byte_size: source_metadata.len(),
        })
    }

    pub fn start_render(
        &self,
        user_id: &str,
        spec_path: PathBuf,
        expected_revision: u64,
    ) -> Result<PresentationRenderJob, OfficeError> {
        self.prune_finished_jobs();
        validate_spec_file(&spec_path)?;
        let actual_revision = read_revision(&spec_path)?;
        if expected_revision > MAX_SAFE_REVISION {
            return Err(OfficeError::Presentation(
                "expected revision exceeds the cross-runtime safe integer limit".into(),
            ));
        }
        if actual_revision != expected_revision {
            return Err(OfficeError::Presentation(format!(
                "stale revision: expected {expected_revision}, found {actual_revision}"
            )));
        }

        if let Some((_, old_id)) = self.active_by_deck.remove(&spec_path) {
            self.cancel_job(&old_id);
        }

        let job_id = Uuid::now_v7().to_string();
        let job = PresentationRenderJob {
            job_id: job_id.clone(),
            revision: expected_revision,
            status: PresentationRenderStatus::Queued,
            output_file: None,
            error_code: None,
        };
        self.jobs.insert(job_id.clone(), job.clone());
        self.job_owners.insert(job_id.clone(), user_id.to_owned());
        self.job_decks.insert(job_id.clone(), spec_path.clone());
        self.active_by_deck.insert(spec_path.clone(), job_id.clone());

        let service = self.clone();
        let task_job_id = job_id.clone();
        let handle = tokio::spawn(async move {
            service.update_status(&task_job_id, PresentationRenderStatus::Running, None, None);
            let output = output_path_for(&spec_path);
            let spec_arg = spec_path.to_string_lossy().into_owned();
            let output_arg = output.to_string_lossy().into_owned();
            let revision_arg = expected_revision.to_string();
            let result = service
                .run_json([
                    "deck",
                    "build",
                    &spec_arg,
                    "--output",
                    &output_arg,
                    "--expected-revision",
                    &revision_arg,
                    "--json",
                ])
                .await;
            match result {
                Ok(value) => match validate_build_result(&value, &output, expected_revision) {
                    Ok(()) => service.update_status(
                        &task_job_id,
                        PresentationRenderStatus::Completed,
                        output.file_name().map(|name| name.to_string_lossy().into_owned()),
                        None,
                    ),
                    Err(error) => {
                        tracing::warn!(target: "presentation", job_id = %task_job_id, error = %error, "OfficeCLI returned an invalid build result");
                        service.update_status(
                            &task_job_id,
                            PresentationRenderStatus::Failed,
                            None,
                            Some("PRESENTATION_RENDER_OUTPUT_INVALID".into()),
                        );
                    }
                },
                Err(error) => {
                    tracing::warn!(target: "presentation", job_id = %task_job_id, error = %error, "presentation render failed");
                    service.update_status(
                        &task_job_id,
                        PresentationRenderStatus::Failed,
                        None,
                        Some("PRESENTATION_RENDER_FAILED".into()),
                    );
                }
            }
            service.abort_handles.remove(&task_job_id);
            service.job_decks.remove(&task_job_id);
            if service
                .active_by_deck
                .get(&spec_path)
                .is_some_and(|entry| entry.value() == &task_job_id)
            {
                service.active_by_deck.remove(&spec_path);
            }
        });
        self.abort_handles.insert(job_id.clone(), handle.abort_handle());
        if self.jobs.get(&job_id).is_some_and(|entry| {
            matches!(
                entry.status,
                PresentationRenderStatus::Completed
                    | PresentationRenderStatus::Failed
                    | PresentationRenderStatus::Cancelled
            )
        }) {
            self.abort_handles.remove(&job_id);
        }
        Ok(job)
    }

    pub fn get_job(&self, user_id: &str, job_id: &str) -> Option<PresentationRenderJob> {
        if self.job_owners.get(job_id).is_none_or(|owner| owner.value() != user_id) {
            return None;
        }
        self.jobs.get(job_id).map(|entry| entry.clone())
    }

    pub fn cancel(&self, user_id: &str, job_id: &str) -> bool {
        if self.job_owners.get(job_id).is_none_or(|owner| owner.value() != user_id) {
            return false;
        }
        self.cancel_job(job_id)
    }

    pub fn cancel_for_user(&self, user_id: &str) {
        let jobs: Vec<String> = self
            .job_owners
            .iter()
            .filter(|entry| entry.value() == user_id)
            .map(|entry| entry.key().clone())
            .collect();
        for job_id in jobs {
            self.cancel_job(&job_id);
        }
    }

    fn cancel_job(&self, job_id: &str) -> bool {
        let Some(mut job) = self.jobs.get_mut(job_id) else {
            return false;
        };
        if matches!(
            job.status,
            PresentationRenderStatus::Completed | PresentationRenderStatus::Failed
        ) {
            return false;
        }
        if let Some((_, handle)) = self.abort_handles.remove(job_id) {
            handle.abort();
        }
        job.status = PresentationRenderStatus::Cancelled;
        job.error_code = Some("PRESENTATION_RENDER_CANCELLED".into());
        if let Some((_, deck)) = self.job_decks.remove(job_id)
            && self
                .active_by_deck
                .get(&deck)
                .is_some_and(|entry| entry.value() == job_id)
        {
            self.active_by_deck.remove(&deck);
        }
        true
    }

    async fn run_json<const N: usize>(&self, args: [&str; N]) -> Result<Value, OfficeError> {
        let officecli = match &self.officecli_path {
            Some(path) => path.clone(),
            None => resolve_officecli_path()?,
        };
        let mut command = CmdBuilder::clean_cli(&officecli);
        command.args(args);
        let output = tokio::time::timeout(OFFICECLI_TIMEOUT, command.output())
            .await
            .map_err(|_| OfficeError::Presentation("OfficeCLI deck command timed out".into()))?
            .map_err(|error| OfficeError::Presentation(format!("could not start OfficeCLI: {error}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                target: "presentation",
                exit_code = output.status.code(),
                stderr = %stderr.lines().next().unwrap_or_default(),
                "OfficeCLI deck command failed"
            );
            if let Ok(value) = serde_json::from_str::<Value>(&stdout)
                && value.get("valid").is_some()
            {
                return Ok(value);
            }
            return Err(OfficeError::Presentation("OfficeCLI deck command failed".into()));
        }
        serde_json::from_str(&stdout).map_err(OfficeError::Json)
    }

    fn update_status(
        &self,
        job_id: &str,
        status: PresentationRenderStatus,
        output_file: Option<String>,
        error_code: Option<String>,
    ) {
        if let Some(mut job) = self.jobs.get_mut(job_id) {
            if job.status == PresentationRenderStatus::Cancelled {
                return;
            }
            job.status = status;
            job.output_file = output_file;
            job.error_code = error_code;
        }
    }

    fn prune_finished_jobs(&self) {
        if self.jobs.len() < 256 {
            return;
        }
        let finished: Vec<String> = self
            .jobs
            .iter()
            .filter(|entry| {
                matches!(
                    entry.status,
                    PresentationRenderStatus::Completed
                        | PresentationRenderStatus::Failed
                        | PresentationRenderStatus::Cancelled
                )
            })
            .map(|entry| entry.key().clone())
            .collect();
        for job_id in finished {
            self.jobs.remove(&job_id);
            self.job_owners.remove(&job_id);
            self.job_decks.remove(&job_id);
            self.abort_handles.remove(&job_id);
        }
    }
}

fn validate_build_result(value: &Value, output: &Path, expected_revision: u64) -> Result<(), OfficeError> {
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("revision").and_then(Value::as_u64) != Some(expected_revision)
        || value.get("output").and_then(Value::as_str).map(Path::new) != Some(output)
        || !output.is_file()
    {
        return Err(OfficeError::Presentation(
            "OfficeCLI build response or output file did not match the requested revision".into(),
        ));
    }
    Ok(())
}

fn validate_spec_file(path: &Path) -> Result<(), OfficeError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(OfficeError::Presentation("deck spec is not a file".into()));
    }
    if metadata.len() > MAX_SPEC_BYTES {
        return Err(OfficeError::Presentation("deck spec exceeds the 2 MB limit".into()));
    }
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".workmate-deck.json"))
    {
        return Err(OfficeError::Presentation(
            "deck spec must use the .workmate-deck.json suffix".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct RevisionOnly {
    revision: u64,
}

fn read_revision(path: &Path) -> Result<u64, OfficeError> {
    let file = std::fs::File::open(path)?;
    serde_json::from_reader::<_, RevisionOnly>(file)
        .map(|spec| spec.revision)
        .map_err(OfficeError::Json)
        .and_then(|revision| {
            if revision <= MAX_SAFE_REVISION {
                Ok(revision)
            } else {
                Err(OfficeError::Presentation(
                    "deck revision exceeds the cross-runtime safe integer limit".into(),
                ))
            }
        })
}

fn output_path_for(spec_path: &Path) -> PathBuf {
    let file_name = spec_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("presentation.workmate-deck.json");
    let stem = file_name.strip_suffix(".workmate-deck.json").unwrap_or("presentation");
    spec_path.with_file_name(format!("{stem}.pptx"))
}

fn validate_asset_id(asset_id: &str) -> Result<(), OfficeError> {
    if asset_id.is_empty()
        || asset_id.len() > 128
        || !asset_id.as_bytes()[0].is_ascii_alphanumeric()
        || asset_id.contains("..")
        || !asset_id
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, b'-' | b'_' | b'.'))
    {
        return Err(OfficeError::Presentation(
            "asset_id must contain only ASCII letters, numbers, '.', '-', or '_' and be at most 128 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_image_file(path: &Path) -> Result<&'static str, OfficeError> {
    use std::io::Read;

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| OfficeError::Presentation("presentation assets require an image extension".into()))?;
    let mut file = std::fs::File::open(path)?;
    let mut header = [0_u8; 12];
    let length = file.read(&mut header)?;
    let header = &header[..length];
    let normalized = match extension.as_str() {
        "png" if header.starts_with(b"\x89PNG\r\n\x1a\n") => "png",
        "jpg" | "jpeg" if header.starts_with(b"\xff\xd8\xff") => "jpg",
        "gif" if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") => "gif",
        _ => {
            return Err(OfficeError::Presentation(
                "presentation assets must be a PNG, JPEG, or GIF with a matching file signature".into(),
            ));
        }
    };
    Ok(normalized)
}

fn asset_directory_for(spec_path: &Path) -> Result<(PathBuf, String), OfficeError> {
    let file_name = spec_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| OfficeError::Presentation("deck spec has no valid file name".into()))?;
    let stem = file_name
        .strip_suffix(".workmate-deck.json")
        .ok_or_else(|| OfficeError::Presentation("invalid deck spec suffix".into()))?;
    let relative = format!("{stem}.assets");
    Ok((spec_path.with_file_name(&relative), relative))
}

fn ensure_asset_directory_contained(spec_path: &Path, asset_directory: &Path) -> Result<(), OfficeError> {
    let deck_parent = spec_path
        .parent()
        .ok_or_else(|| OfficeError::Presentation("deck spec has no parent directory".into()))?
        .canonicalize()?;
    let canonical_assets = asset_directory.canonicalize()?;
    if canonical_assets.parent() != Some(deck_parent.as_path()) {
        return Err(OfficeError::Presentation(
            "presentation asset directory escapes the deck directory".into(),
        ));
    }
    Ok(())
}

fn asset_directory_size(directory: &Path) -> Result<u64, OfficeError> {
    let mut total = 0_u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn copy_asset_atomic(source: &Path, target: &Path) -> Result<(), OfficeError> {
    use std::io::Write;

    let parent = target
        .parent()
        .ok_or_else(|| OfficeError::Presentation("presentation asset target has no parent".into()))?;
    let file_name = target.file_name().and_then(|value| value.to_str()).unwrap_or("asset");
    let token = Uuid::now_v7();
    let temporary = parent.join(format!(".{file_name}.{token}.tmp"));
    let backup = parent.join(format!(".{file_name}.{token}.bak"));
    let result = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(source)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        drop(output);

        if cfg!(windows) && target.exists() {
            std::fs::rename(target, &backup)?;
            if let Err(error) = std::fs::rename(&temporary, target) {
                let _ = std::fs::rename(&backup, target);
                return Err(error);
            }
            std::fs::remove_file(&backup)?;
        } else {
            std::fs::rename(&temporary, target)?;
        }
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        if backup.exists() && !target.exists() {
            let _ = std::fs::rename(&backup, target);
        }
    }
    result.map_err(OfficeError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.json");
        std::fs::write(&path, br#"{"revision":1}"#).unwrap();
        let error = validate_spec_file(&path).unwrap_err();
        assert!(error.to_string().contains(".workmate-deck.json"));
    }

    #[test]
    fn derives_pptx_next_to_source() {
        let path = Path::new("/workspace/quarterly.workmate-deck.json");
        assert_eq!(output_path_for(path), Path::new("/workspace/quarterly.pptx"));
    }

    #[test]
    fn reads_revision_without_loading_unbounded_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.workmate-deck.json");
        std::fs::write(&path, br#"{"revision":42,"slides":[]}"#).unwrap();
        assert_eq!(read_revision(&path).unwrap(), 42);
    }

    #[test]
    fn rejects_revisions_outside_the_cross_runtime_safe_integer_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.workmate-deck.json");
        std::fs::write(&path, br#"{"revision":9007199254740992}"#).unwrap();
        assert!(read_revision(&path).is_err());
    }

    #[test]
    fn accepts_only_matching_build_results_with_a_real_output_file() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("quarterly.pptx");
        std::fs::write(&output, b"pptx").unwrap();
        let value = serde_json::json!({
            "success": true,
            "output": output,
            "revision": 7
        });
        validate_build_result(&value, &output, 7).unwrap();
    }

    #[test]
    fn rejects_stale_or_missing_build_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("quarterly.pptx");
        let stale = serde_json::json!({
            "success": true,
            "output": output,
            "revision": 6
        });
        assert!(validate_build_result(&stale, &output, 7).is_err());

        let missing = serde_json::json!({
            "success": true,
            "output": output,
            "revision": 7
        });
        assert!(validate_build_result(&missing, &output, 7).is_err());
    }

    #[test]
    fn jobs_are_scoped_to_the_owning_user() {
        let service = PresentationService::new(None);
        let job = PresentationRenderJob {
            job_id: "job-1".into(),
            revision: 1,
            status: PresentationRenderStatus::Queued,
            output_file: None,
            error_code: None,
        };
        service.jobs.insert(job.job_id.clone(), job);
        service.job_owners.insert("job-1".into(), "alice".into());

        assert!(service.get_job("alice", "job-1").is_some());
        assert!(service.get_job("bob", "job-1").is_none());
        assert!(!service.cancel("bob", "job-1"));
    }

    #[test]
    fn cancellation_releases_the_active_deck_slot() {
        let service = PresentationService::new(None);
        let deck = PathBuf::from("deck.workmate-deck.json");
        let job = PresentationRenderJob {
            job_id: "job-1".into(),
            revision: 1,
            status: PresentationRenderStatus::Queued,
            output_file: None,
            error_code: None,
        };
        service.jobs.insert(job.job_id.clone(), job);
        service.job_owners.insert("job-1".into(), "alice".into());
        service.job_decks.insert("job-1".into(), deck.clone());
        service.active_by_deck.insert(deck.clone(), "job-1".into());

        assert!(service.cancel("alice", "job-1"));
        assert!(!service.active_by_deck.contains_key(&deck));
        assert_eq!(
            service.get_job("alice", "job-1").unwrap().status,
            PresentationRenderStatus::Cancelled
        );
    }

    #[test]
    fn imports_a_verified_image_into_the_deck_asset_directory() {
        let directory = tempfile::tempdir().unwrap();
        let spec = directory.path().join("quarterly.workmate-deck.json");
        let source = directory.path().join("source.png");
        std::fs::write(&spec, b"{}").unwrap();
        std::fs::write(&source, b"\x89PNG\r\n\x1a\nimage").unwrap();

        let imported = PresentationService::new(None)
            .import_asset(&spec, &source, "hero-image")
            .unwrap();

        assert!(imported.asset_path.starts_with("quarterly.assets/hero-image-"));
        assert!(imported.asset_path.ends_with(".png"));
        assert!(directory.path().join(&imported.asset_path).is_file());
    }

    #[test]
    fn rejects_disguised_images_without_replacing_the_previous_asset() {
        let directory = tempfile::tempdir().unwrap();
        let spec = directory.path().join("quarterly.workmate-deck.json");
        let source = directory.path().join("source.png");
        std::fs::write(&spec, b"{}").unwrap();
        std::fs::write(&source, b"\x89PNG\r\n\x1a\nold").unwrap();
        let service = PresentationService::new(None);
        let imported = service.import_asset(&spec, &source, "hero").unwrap();
        let target = directory.path().join(imported.asset_path);

        std::fs::write(&source, b"not an image").unwrap();
        assert!(service.import_asset(&spec, &source, "hero").is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"\x89PNG\r\n\x1a\nold");
    }

    #[test]
    fn replacement_imports_use_versioned_paths_and_keep_the_old_file() {
        let directory = tempfile::tempdir().unwrap();
        let spec = directory.path().join("quarterly.workmate-deck.json");
        let source = directory.path().join("source.png");
        std::fs::write(&spec, b"{}").unwrap();
        std::fs::write(&source, b"\x89PNG\r\n\x1a\nold").unwrap();
        let service = PresentationService::new(None);
        let first = service.import_asset(&spec, &source, "hero").unwrap();

        std::fs::write(&source, b"\x89PNG\r\n\x1a\nnew").unwrap();
        let second = service.import_asset(&spec, &source, "hero").unwrap();

        assert_ne!(first.asset_path, second.asset_path);
        assert_eq!(
            std::fs::read(directory.path().join(first.asset_path)).unwrap(),
            b"\x89PNG\r\n\x1a\nold"
        );
        assert_eq!(
            std::fs::read(directory.path().join(second.asset_path)).unwrap(),
            b"\x89PNG\r\n\x1a\nnew"
        );
    }

    #[test]
    fn rejects_asset_ids_that_could_create_hidden_or_ambiguous_paths() {
        assert!(validate_asset_id(".hidden").is_err());
        assert!(validate_asset_id("hero..backup").is_err());
        assert!(validate_asset_id("hero-image_2").is_ok());
    }
}
