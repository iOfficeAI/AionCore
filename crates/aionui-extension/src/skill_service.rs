use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use aionui_db::{CreateSkillImportRecordParams, ISkillRepository, SkillRow, UpsertSkillParams};

use crate::constants::{
    ASSISTANT_RULES_DIR_NAME, ASSISTANT_SKILLS_DIR_NAME, BUILTIN_AUTO_SKILLS_SUBDIR, BUILTIN_RULES_DIR_NAME,
    COMMON_SKILL_DIRS, CRON_SKILLS_DIR_NAME, SKILL_MANIFEST_FILE, SKILLS_DIR_NAME,
};
use crate::error::ExtensionError;

/// Built-in skill corpus embedded into the binary at compile time.
///
/// Mirrors the strategy used by `aionui-assistant::builtin`: the corpus is
/// authoritative at build time; an optional on-disk override
/// (`AIONUI_BUILTIN_SKILLS_PATH`) is consulted at runtime for rapid
/// iteration and E2E fixtures.
static BUILTIN_SKILLS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../aionui-app/assets/builtin-skills");

/// Name of the environment variable that, when set, overrides the embedded
/// corpus with an on-disk directory. Consumed by
/// [`resolve_skill_paths`] when building [`SkillPaths`].
pub const BUILTIN_SKILLS_ENV_VAR: &str = "AIONUI_BUILTIN_SKILLS_PATH";
const MAX_SKILL_IMPORT_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_SKILL_IMPORT_TOTAL_BYTES: u64 = 200 * 1024 * 1024;
const IMPORT_STAGING_PREFIX: &str = ".import-staging-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillImportLimits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

pub fn skill_import_limits() -> SkillImportLimits {
    SkillImportLimits {
        max_file_bytes: MAX_SKILL_IMPORT_FILE_BYTES,
        max_total_bytes: MAX_SKILL_IMPORT_TOTAL_BYTES,
    }
}

/// Expose the embedded builtin skills corpus for startup
/// materialization. Consumers outside this crate should not depend on
/// `include_dir` directly.
pub fn builtin_skills_corpus() -> &'static Dir<'static> {
    &BUILTIN_SKILLS
}

/// Build the startup materialization marker for a built-in skill corpus.
///
/// The package version alone is not enough: built-in skills can change
/// without a crate version bump during development or prerelease packaging.
/// Include a deterministic content fingerprint so startup refreshes the
/// materialized `{data_dir}/builtin-skills` tree whenever embedded skill
/// files change.
pub fn builtin_skills_materialize_marker(corpus: &Dir<'static>, package_version: &str) -> String {
    format!(
        "{package_version}+builtin-skills.{}",
        builtin_skills_corpus_fingerprint(corpus)
    )
}

fn builtin_skills_corpus_fingerprint(corpus: &Dir<'static>) -> String {
    let mut files = Vec::new();
    collect_corpus_files(corpus, corpus.path(), &mut files);
    files.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut hasher = Sha256::new();
    for (relative_path, contents) in files {
        hasher.update(relative_path.as_bytes());
        hasher.update([0]);
        hasher.update((contents.len() as u64).to_le_bytes());
        hasher.update([0]);
        hasher.update(contents);
        hasher.update([0xff]);
    }

    let digest = hasher.finalize();
    hex::encode(&digest[..12])
}

fn collect_corpus_files(dir: &Dir<'static>, root: &Path, files: &mut Vec<(String, &'static [u8])>) {
    for file in dir.files() {
        let relative_path = file.path().strip_prefix(root).unwrap_or(file.path());
        let normalized_path = relative_path.to_string_lossy().replace('\\', "/");
        files.push((normalized_path, file.contents()));
    }
    for subdir in dir.dirs() {
        collect_corpus_files(subdir, root, files);
    }
}

// ---------------------------------------------------------------------------
// Skill paths resolution
// ---------------------------------------------------------------------------

/// Resolved base directories for skill and rule management.
///
/// `builtin_skills_dir` always points at a real on-disk directory.
/// In production it resolves to `{data_dir}/builtin-skills/`, populated
/// at startup by [`crate::startup_materialize::materialize_if_needed`].
/// In dev/test it can be redirected via [`BUILTIN_SKILLS_ENV_VAR`].
#[derive(Debug, Clone)]
pub struct SkillPaths {
    /// Root data directory (~/.aionui/).
    pub data_dir: PathBuf,
    /// User-created skills directory (~/.aionui/skills/).
    pub user_skills_dir: PathBuf,
    /// Per-job cron skills directory (~/.aionui/cron/skills/).
    pub cron_skills_dir: PathBuf,
    /// Built-in skills directory on disk. Always set.
    /// Points to `{data_dir}/builtin-skills/` in production (populated at
    /// startup by `startup_materialize::materialize_if_needed`) or
    /// wherever [`BUILTIN_SKILLS_ENV_VAR`] points in dev mode.
    pub builtin_skills_dir: PathBuf,
    /// Built-in rules directory (app bundle resource).
    pub builtin_rules_dir: PathBuf,
    /// Assistant-level rules directory (~/.aionui/assistant-rules/).
    pub assistant_rules_dir: PathBuf,
    /// Assistant-level skills directory (~/.aionui/assistant-skills/).
    pub assistant_skills_dir: PathBuf,
}

/// Resolve standard skill paths.
///
/// `app_resource_dir` is the application's bundled resource directory
/// (e.g. the binary's parent or a configured resource path); only
/// `builtin_rules_dir` is still derived from it — built-in skills live
/// under `data_dir` (materialized at startup from the embedded corpus)
/// unless redirected via [`BUILTIN_SKILLS_ENV_VAR`].
///
/// `data_dir` is the user-level data root (e.g. `~/.aionui/`) and
/// determines where user skills, assistant resources, and the built-in
/// skills tree (`{data_dir}/builtin-skills/`) live. Per-conversation
/// agent skills are no longer materialized on disk — see
/// [`materialize_skills_for_agent`] for the symlink contract.
pub fn resolve_skill_paths(app_resource_dir: &Path, data_dir: &Path) -> SkillPaths {
    let builtin_skills_dir = std::env::var(BUILTIN_SKILLS_ENV_VAR)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join(crate::constants::BUILTIN_SKILLS_DIR_NAME));

    SkillPaths {
        data_dir: data_dir.to_path_buf(),
        user_skills_dir: data_dir.join(SKILLS_DIR_NAME),
        cron_skills_dir: data_dir.join(CRON_SKILLS_DIR_NAME),
        builtin_skills_dir,
        builtin_rules_dir: app_resource_dir.join(BUILTIN_RULES_DIR_NAME),
        assistant_rules_dir: data_dir.join(ASSISTANT_RULES_DIR_NAME),
        assistant_skills_dir: data_dir.join(ASSISTANT_SKILLS_DIR_NAME),
    }
}

// ---------------------------------------------------------------------------
// A. Built-in resource reading
// ---------------------------------------------------------------------------

/// Read a built-in rule file by name.
///
/// Returns the file content as a string. Returns an empty string if the
/// file does not exist (graceful degradation per API spec).
pub async fn read_builtin_rule(paths: &SkillPaths, file_name: &str) -> Result<String, ExtensionError> {
    validate_filename(file_name)?;
    let file_path = paths.builtin_rules_dir.join(file_name);
    read_file_or_empty(&file_path).await
}

/// Read a built-in skill file by name.
///
/// `file_name` is a relative path inside the built-in skills corpus
/// (e.g. `"auto-inject/cron/SKILL.md"` or `"mermaid/SKILL.md"`). Returns
/// the file content as a string, or an empty string if the file does not
/// exist (preserves the legacy graceful-degradation contract consumed by
/// the renderer).
///
/// Reads from `paths.builtin_skills_dir`, which is always populated at
/// startup by [`crate::startup_materialize::materialize_if_needed`].
/// Rejects `..`-style traversal.
pub async fn read_builtin_skill(paths: &SkillPaths, file_name: &str) -> Result<String, ExtensionError> {
    validate_builtin_skill_path(file_name)?;
    let file_path = paths.builtin_skills_dir.join(file_name);
    read_file_or_empty(&file_path).await
}

// ---------------------------------------------------------------------------
// B. Assistant-level CRUD
// ---------------------------------------------------------------------------

/// Read an assistant rule with locale fallback.
///
/// Fallback order:
/// 1. `{assistantId}.{locale}.md` (if locale provided)
/// 2. `{assistantId}.md`
/// 3. Empty string
pub async fn read_assistant_rule(
    paths: &SkillPaths,
    assistant_id: &str,
    locale: Option<&str>,
) -> Result<String, ExtensionError> {
    read_assistant_resource(&paths.assistant_rules_dir, assistant_id, locale).await
}

/// Write an assistant rule.
///
/// Creates `{assistantId}.{locale}.md` or `{assistantId}.md` in the
/// assistant rules directory.
pub async fn write_assistant_rule(
    paths: &SkillPaths,
    assistant_id: &str,
    content: &str,
    locale: Option<&str>,
) -> Result<bool, ExtensionError> {
    write_assistant_resource(&paths.assistant_rules_dir, assistant_id, content, locale).await
}

/// Delete all locale versions of an assistant rule.
pub async fn delete_assistant_rule(paths: &SkillPaths, assistant_id: &str) -> Result<bool, ExtensionError> {
    delete_assistant_resource(&paths.assistant_rules_dir, assistant_id).await
}

/// Read an assistant skill with locale fallback.
pub async fn read_assistant_skill(
    paths: &SkillPaths,
    assistant_id: &str,
    locale: Option<&str>,
) -> Result<String, ExtensionError> {
    read_assistant_resource(&paths.assistant_skills_dir, assistant_id, locale).await
}

/// Write an assistant skill.
pub async fn write_assistant_skill(
    paths: &SkillPaths,
    assistant_id: &str,
    content: &str,
    locale: Option<&str>,
) -> Result<bool, ExtensionError> {
    write_assistant_resource(&paths.assistant_skills_dir, assistant_id, content, locale).await
}

/// Delete all locale versions of an assistant skill.
pub async fn delete_assistant_skill(paths: &SkillPaths, assistant_id: &str) -> Result<bool, ExtensionError> {
    delete_assistant_resource(&paths.assistant_skills_dir, assistant_id).await
}

// ---------------------------------------------------------------------------
// C. Skill listing & info
// ---------------------------------------------------------------------------

/// Origin of a listed skill.
///
/// Matches the renderer contract in
/// `src/common/adapter/ipcBridge.ts::listAvailableSkills`, which filters the
/// Skills Hub UI by this value. `Extension` is reserved for
/// extension-contributed skills once `ExtensionRegistry` is wired into the
/// Rust backend; the pilot only emits `Builtin` / `Custom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Builtin,
    Custom,
    Cron,
    Extension,
}

/// A discovered skill item for listing.
///
/// For `source=Builtin`, `location` is the absolute path of the on-disk
/// SKILL.md under `paths.builtin_skills_dir` (populated at startup by
/// [`crate::startup_materialize::materialize_if_needed`]). The
/// `relative_location` carries the relative path suitable for
/// `POST /api/skills/builtin-skill` (e.g. `"auto-inject/cron/SKILL.md"`
/// or `"mermaid/SKILL.md"`). Other sources leave `relative_location`
/// `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillListItem {
    pub name: String,
    pub description: String,
    pub location: String,
    pub relative_location: Option<String>,
    pub is_custom: bool,
    pub source: SkillSource,
}

/// List all available skills (built-in + user custom), deduplicated.
///
/// User custom skills override built-in skills with the same name.
///
/// For built-in entries, the caller sees an absolute `location` pointing
/// at `paths.builtin_skills_dir/.../SKILL.md` — the tree is populated
/// at startup by
/// [`crate::startup_materialize::materialize_if_needed`] so downstream
/// consumers (e.g. the SkillsHubSettings export-symlink flow) can
/// resolve the path on disk. `relative_location` is populated for
/// built-ins only.
pub async fn list_available_skills(paths: &SkillPaths) -> Result<Vec<SkillListItem>, ExtensionError> {
    let mut builtin_skills = std::collections::HashMap::new();

    // 1. Built-in skills (lower priority)
    for item in list_builtin_skills(paths).await {
        builtin_skills.insert(item.name.clone(), item);
    }

    // 2. User custom skills (higher priority, overrides builtin).
    // DB-backed production callers use `list_available_skills_with_repo`.
    // This path-only fallback is retained for low-level tests and scans
    // the user skills directory directly.
    let mut custom_skills = list_user_skills_from_disk(paths).await?;
    for item in &custom_skills {
        builtin_skills.remove(&item.name);
    }

    custom_skills.sort_by(|a, b| {
        skill_modified_time(&b.location)
            .cmp(&skill_modified_time(&a.location))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut builtin_items: Vec<SkillListItem> = builtin_skills.into_values().collect();
    builtin_items.sort_by(|a, b| a.name.cmp(&b.name));

    let mut result = custom_skills;
    result.extend(builtin_items);
    Ok(result)
}

/// List all available skills using the database as the user-skill state source.
pub async fn list_available_skills_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
) -> Result<Vec<SkillListItem>, ExtensionError> {
    list_skills_from_repo(paths, repo).await
}

/// Emit a [`SkillListItem`] for every built-in skill (both auto-inject
/// and opt-in). All paths resolve directly against
/// `paths.builtin_skills_dir`.
async fn list_builtin_skills(paths: &SkillPaths) -> Vec<SkillListItem> {
    list_builtin_skills_from_disk(&paths.builtin_skills_dir).await
}

async fn list_builtin_skills_from_disk(dir: &Path) -> Vec<SkillListItem> {
    let mut items = Vec::new();

    // Top-level opt-in skills (siblings of auto-inject/).
    if let Ok(top) = scan_skill_dirs(dir).await {
        for s in top {
            if s.name == BUILTIN_AUTO_SKILLS_SUBDIR {
                continue;
            }
            // Use the on-disk directory name (basename of scanned path)
            // rather than the frontmatter name, so the path we emit
            // matches the real filesystem layout when the two disagree.
            let dir_name = Path::new(&s.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&s.name)
                .to_string();
            let rel = format!("{dir_name}/{SKILL_MANIFEST_FILE}");
            let location = dir
                .join(&dir_name)
                .join(SKILL_MANIFEST_FILE)
                .to_string_lossy()
                .into_owned();
            items.push(SkillListItem {
                name: s.name,
                description: s.description,
                location,
                relative_location: Some(rel),
                is_custom: false,
                source: SkillSource::Builtin,
            });
        }
    }

    // auto-inject children.
    let auto_dir = dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
    if let Ok(auto) = scan_skill_dirs(&auto_dir).await {
        for s in auto {
            let dir_name = Path::new(&s.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&s.name)
                .to_string();
            let rel = format!("{BUILTIN_AUTO_SKILLS_SUBDIR}/{dir_name}/{SKILL_MANIFEST_FILE}");
            let location = auto_dir
                .join(&dir_name)
                .join(SKILL_MANIFEST_FILE)
                .to_string_lossy()
                .into_owned();
            items.push(SkillListItem {
                name: s.name,
                description: s.description,
                location,
                relative_location: Some(rel),
                is_custom: false,
                source: SkillSource::Builtin,
            });
        }
    }

    items
}

/// A skill discovered during directory scanning.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedSkill {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// An auto-injected built-in skill.
///
/// Auto-injected built-in skill metadata. `location` is the relative path
/// the frontend passes back into `POST /api/skills/builtin-skill`, e.g.
/// `"auto-inject/cron/SKILL.md"`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
struct AutoInjectSkillDiskItem {
    pub name: String,
    pub description: String,
    pub location: String,
}

/// List built-in skills that are auto-injected into every assistant.
///
/// Reads from `{paths.builtin_skills_dir}/auto-inject/`. A missing
/// `auto-inject/` directory yields an empty list, matching the
/// graceful-degradation semantics used elsewhere in this module.
#[cfg(test)]
async fn list_auto_inject_skills_from_disk(paths: &SkillPaths) -> Result<Vec<AutoInjectSkillDiskItem>, ExtensionError> {
    let auto_dir = paths.builtin_skills_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
    let mut items = list_auto_skills_from_disk(&auto_dir).await;
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

#[cfg(test)]
async fn list_auto_skills_from_disk(auto_dir: &Path) -> Vec<AutoInjectSkillDiskItem> {
    let entries = match scan_skill_dirs(auto_dir).await {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .into_iter()
        .map(|s| {
            let name = s.name.clone();
            AutoInjectSkillDiskItem {
                name,
                description: s.description,
                location: format!("{BUILTIN_AUTO_SKILLS_SUBDIR}/{}/{SKILL_MANIFEST_FILE}", s.name),
            }
        })
        .collect()
}

/// Read skill info from a SKILL.md file without importing.
///
/// Returns `(name, description)` extracted from frontmatter.
pub async fn read_skill_info(skill_path: &Path) -> Result<(String, String), ExtensionError> {
    let skill_file = if skill_path.is_dir() {
        skill_path.join(SKILL_MANIFEST_FILE)
    } else {
        skill_path.to_path_buf()
    };

    let content = tokio::fs::read_to_string(&skill_file)
        .await
        .map_err(|_| ExtensionError::SkillNotFound(skill_path.display().to_string()))?;

    let (name, description) = parse_frontmatter_fields(&content)
        .ok_or_else(|| ExtensionError::SkillInvalidFrontmatter(skill_file.display().to_string()))?;

    // Fallback: use directory name if name is empty
    let final_name = if name.is_empty() {
        skill_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        name
    };

    Ok((final_name, description))
}

// ---------------------------------------------------------------------------
// D. Skill import / export / delete
// ---------------------------------------------------------------------------

/// Import a skill by copying its directory to the user skills directory.
///
/// Returns the skill name.
pub async fn import_skill(paths: &SkillPaths, skill_path: &Path) -> Result<String, ExtensionError> {
    let copied = copy_skill_into_user_dir(paths, skill_path).await?;

    debug!(
        skill = %copied.name,
        target = %copied.target_dir.display(),
        copied_bytes = copied.copied_bytes,
        "skill imported (copy)"
    );
    Ok(copied.name)
}

/// Import a skill and persist its management metadata in the database.
pub async fn import_skill_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
    skill_path: &Path,
) -> Result<ImportedSkill, ExtensionError> {
    let copied = copy_skill_into_user_dir(paths, skill_path).await?;
    let overwritten = repo.find_by_name(&copied.name).await?.is_some();
    let row = repo
        .upsert(UpsertSkillParams {
            name: &copied.name,
            description: Some(&copied.description),
            path: copied.target_dir.to_string_lossy().as_ref(),
            source: "user",
            enabled: true,
        })
        .await?;

    debug!(
        skill = %copied.name,
        target = %copied.target_dir.display(),
        copied_bytes = copied.copied_bytes,
        "skill imported (copy)"
    );
    Ok(ImportedSkill {
        name: copied.name,
        skill_id: Some(row.id),
        overwritten,
        copied_bytes: copied.copied_bytes,
    })
}

struct CopiedSkill {
    name: String,
    description: String,
    target_dir: PathBuf,
    copied_bytes: u64,
}

async fn copy_skill_into_user_dir(paths: &SkillPaths, skill_path: &Path) -> Result<CopiedSkill, ExtensionError> {
    let (name, description) = read_skill_info(skill_path).await?;
    validate_filename(&name)?;

    let target_dir = paths.user_skills_dir.join(&name);
    tokio::fs::create_dir_all(&paths.user_skills_dir).await?;

    let staging_dir = import_staging_dir(paths, &name);
    replace_existing_path(&staging_dir).await?;
    let mut budget = SkillImportBudget::default();
    if let Err(err) = copy_skill_dir_for_import(skill_path, &staging_dir, skill_path, &mut budget).await {
        if let Err(cleanup_err) = replace_existing_path(&staging_dir).await {
            warn!(
                staging = %staging_dir.display(),
                error = %cleanup_err,
                "failed to clean skill import staging after copy failure"
            );
        }
        return Err(err);
    }
    replace_existing_path(&target_dir).await?;
    tokio::fs::rename(&staging_dir, &target_dir).await?;

    Ok(CopiedSkill {
        name,
        description,
        target_dir,
        copied_bytes: budget.total_bytes,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillImportBatchFailure {
    pub source_name: String,
    pub code: String,
    pub error_path: Option<String>,
    pub actual_bytes: Option<i64>,
    pub limit_bytes: Option<i64>,
    pub line: Option<i64>,
    pub column: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSkill {
    pub name: String,
    pub skill_id: Option<String>,
    pub overwritten: bool,
    pub copied_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillImportOutcome {
    pub operation_id: String,
    pub imported: Vec<String>,
    pub failed: Vec<SkillImportBatchFailure>,
}

/// Import one skill, a parent directory containing skills, or a zip archive.
///
/// Imports persist stable copies in the user skills directory. Runtime
/// materialization creates agent-specific symlinks later.
pub async fn import_skills(paths: &SkillPaths, source_path: &Path) -> Result<SkillImportOutcome, ExtensionError> {
    let operation_id = aionui_common::generate_prefixed_id("skill_import_op");
    if is_zip_path(source_path) {
        let mut outcome = import_skills_from_zip(paths, source_path).await?;
        outcome.operation_id = operation_id;
        return Ok(outcome);
    }

    let source_path = normalize_import_source_path(source_path)?;

    if source_path.is_dir() {
        if source_path.join(SKILL_MANIFEST_FILE).exists() {
            let name = import_skill(paths, &source_path).await?;
            return Ok(SkillImportOutcome {
                operation_id,
                imported: vec![name],
                failed: Vec::new(),
            });
        }

        let skills = scan_skill_dirs(&source_path).await?;
        if skills.is_empty() {
            return Err(ExtensionError::SkillImportNoSkillFound(
                source_path.display().to_string(),
            ));
        }

        return import_skill_dirs_batch(
            paths,
            skills.into_iter().map(|skill| PathBuf::from(skill.path)).collect(),
            operation_id,
        )
        .await;
    }

    Err(ExtensionError::SkillImportInvalidSource(
        source_path.display().to_string(),
    ))
}

/// Import skills and persist both current skill metadata and import history.
pub async fn import_skills_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
    source_path: &Path,
) -> Result<SkillImportOutcome, ExtensionError> {
    let operation_id = aionui_common::generate_prefixed_id("skill_import_op");
    let source_label = import_source_label(source_path);
    let source_path_text = source_path.to_string_lossy().into_owned();

    if is_zip_path(source_path) {
        let temp_root = paths.user_skills_dir.join(".import-tmp");
        tokio::fs::create_dir_all(&temp_root).await?;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let extract_dir = temp_root.join(format!("skills-{}-{nonce}", std::process::id()));
        tokio::fs::create_dir_all(&extract_dir).await?;

        let archive = source_path.to_path_buf();
        let destination = extract_dir.clone();
        let extraction = tokio::task::spawn_blocking(move || extract_zip_archive(&archive, &destination))
            .await
            .map_err(|e| ExtensionError::SkillImportInvalidZip(format!("Zip extraction task failed: {e}")))?;

        if let Err(err) = extraction {
            let _ = tokio::fs::remove_dir_all(&extract_dir).await;
            let _ = tokio::fs::remove_dir(&temp_root).await;
            return Err(err);
        }

        let result = async {
            let mut skill_dirs = Vec::new();
            collect_skill_dirs_recursive(&extract_dir, &mut skill_dirs).await?;
            if skill_dirs.is_empty() {
                return Err(ExtensionError::SkillImportNoSkillFound(
                    source_path.display().to_string(),
                ));
            }

            import_skill_dirs_batch_with_repo(
                paths,
                repo,
                skill_dirs,
                &operation_id,
                &source_label,
                Some(&source_path_text),
            )
            .await
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&extract_dir).await;
        let _ = tokio::fs::remove_dir(&temp_root).await;
        return result;
    }

    let source_path = normalize_import_source_path(source_path)?;
    let source_label = import_source_label(&source_path);
    let source_path_text = source_path.to_string_lossy().into_owned();

    if source_path.is_dir() {
        if source_path.join(SKILL_MANIFEST_FILE).exists() {
            return import_skill_dirs_batch_with_repo(
                paths,
                repo,
                vec![source_path],
                &operation_id,
                &source_label,
                Some(&source_path_text),
            )
            .await;
        }

        let skills = scan_skill_dirs(&source_path).await?;
        if skills.is_empty() {
            return Err(ExtensionError::SkillImportNoSkillFound(
                source_path.display().to_string(),
            ));
        }

        return import_skill_dirs_batch_with_repo(
            paths,
            repo,
            skills.into_iter().map(|skill| PathBuf::from(skill.path)).collect(),
            &operation_id,
            &source_label,
            Some(&source_path_text),
        )
        .await;
    }

    Err(ExtensionError::SkillImportInvalidSource(
        source_path.display().to_string(),
    ))
}

async fn import_skills_from_zip(paths: &SkillPaths, archive_path: &Path) -> Result<SkillImportOutcome, ExtensionError> {
    let temp_root = paths.user_skills_dir.join(".import-tmp");
    tokio::fs::create_dir_all(&temp_root).await?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let extract_dir = temp_root.join(format!("skills-{}-{nonce}", std::process::id()));
    tokio::fs::create_dir_all(&extract_dir).await?;

    let archive = archive_path.to_path_buf();
    let destination = extract_dir.clone();
    let extraction = tokio::task::spawn_blocking(move || extract_zip_archive(&archive, &destination))
        .await
        .map_err(|e| ExtensionError::SkillImportInvalidZip(format!("Zip extraction task failed: {e}")))?;

    if let Err(err) = extraction {
        let _ = tokio::fs::remove_dir_all(&extract_dir).await;
        let _ = tokio::fs::remove_dir(&temp_root).await;
        return Err(err);
    }

    let result = async {
        let mut skill_dirs = Vec::new();
        collect_skill_dirs_recursive(&extract_dir, &mut skill_dirs).await?;
        if skill_dirs.is_empty() {
            return Err(ExtensionError::SkillImportNoSkillFound(
                archive_path.display().to_string(),
            ));
        }

        import_skill_dirs_batch(
            paths,
            skill_dirs,
            aionui_common::generate_prefixed_id("skill_import_op"),
        )
        .await
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
    let _ = tokio::fs::remove_dir(&temp_root).await;
    result
}

async fn import_skill_dirs_batch(
    paths: &SkillPaths,
    skill_dirs: Vec<PathBuf>,
    operation_id: String,
) -> Result<SkillImportOutcome, ExtensionError> {
    let mut imported = Vec::new();
    let mut failed = Vec::new();

    for skill_dir in skill_dirs {
        match import_skill(paths, &skill_dir).await {
            Ok(name) => imported.push(name),
            Err(err) => failed.push(skill_import_failure_from_error(&import_source_name(&skill_dir), &err)),
        }
    }

    imported.sort();
    imported.dedup();
    failed.sort_by(|left, right| {
        left.source_name
            .cmp(&right.source_name)
            .then(left.code.cmp(&right.code))
    });
    failed.dedup();

    Ok(SkillImportOutcome {
        operation_id,
        imported,
        failed,
    })
}

async fn import_skill_dirs_batch_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
    skill_dirs: Vec<PathBuf>,
    operation_id: &str,
    source_label: &str,
    source_path: Option<&str>,
) -> Result<SkillImportOutcome, ExtensionError> {
    let mut imported = Vec::new();
    let mut failed = Vec::new();

    for skill_dir in skill_dirs {
        let source_name = import_source_name(&skill_dir);
        match import_skill_with_repo(paths, repo, &skill_dir).await {
            Ok(skill) => {
                repo.create_import_record(CreateSkillImportRecordParams {
                    operation_id,
                    source_label,
                    source_path,
                    source_name: &source_name,
                    skill_id: skill.skill_id.as_deref(),
                    skill_name: Some(&skill.name),
                    status: if skill.overwritten { "overwritten" } else { "imported" },
                    error_code: None,
                    error_path: None,
                    actual_bytes: Some(u64_to_i64(skill.copied_bytes)),
                    limit_bytes: None,
                    line: None,
                    column: None,
                })
                .await?;
                imported.push(skill.name);
            }
            Err(err) => {
                let failure = skill_import_failure_from_error(&source_name, &err);
                repo.create_import_record(CreateSkillImportRecordParams {
                    operation_id,
                    source_label,
                    source_path,
                    source_name: &source_name,
                    skill_id: None,
                    skill_name: None,
                    status: "failed",
                    error_code: Some(&failure.code),
                    error_path: failure.error_path.as_deref(),
                    actual_bytes: failure.actual_bytes,
                    limit_bytes: failure.limit_bytes,
                    line: failure.line,
                    column: failure.column,
                })
                .await?;
                failed.push(failure);
            }
        }
    }

    imported.sort();
    imported.dedup();
    failed.sort_by(|left, right| {
        left.source_name
            .cmp(&right.source_name)
            .then(left.code.cmp(&right.code))
    });
    failed.dedup();

    Ok(SkillImportOutcome {
        operation_id: operation_id.to_owned(),
        imported,
        failed,
    })
}

fn import_source_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn import_source_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn skill_import_failure_from_error(source_name: &str, err: &ExtensionError) -> SkillImportBatchFailure {
    let mut failure = SkillImportBatchFailure {
        source_name: source_name.to_owned(),
        code: skill_import_error_code(err).to_owned(),
        error_path: None,
        actual_bytes: None,
        limit_bytes: None,
        line: None,
        column: None,
    };

    match err {
        ExtensionError::SkillImportFileTooLarge {
            file_path,
            file_bytes,
            limit_bytes,
        } => {
            failure.error_path = file_path.clone();
            failure.actual_bytes = Some(u64_to_i64(*file_bytes));
            failure.limit_bytes = Some(u64_to_i64(*limit_bytes));
        }
        ExtensionError::SkillImportTotalTooLarge {
            total_bytes,
            limit_bytes,
        } => {
            failure.actual_bytes = Some(u64_to_i64(*total_bytes));
            failure.limit_bytes = Some(u64_to_i64(*limit_bytes));
        }
        _ => {}
    }

    failure
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn skill_import_error_code(err: &ExtensionError) -> &'static str {
    match err {
        ExtensionError::SkillInvalidFrontmatter(_) => "SKILL_INVALID_FRONTMATTER",
        ExtensionError::SkillImportNoSkillFound(_) => "SKILL_IMPORT_NO_SKILL_FOUND",
        ExtensionError::SkillImportInvalidSource(_) => "SKILL_IMPORT_INVALID_SOURCE",
        ExtensionError::SkillImportSymlinkEntry(_) => "SKILL_IMPORT_SYMLINK_ENTRY",
        ExtensionError::SkillImportFileTooLarge { .. } => "SKILL_IMPORT_FILE_TOO_LARGE",
        ExtensionError::SkillImportTotalTooLarge { .. } => "SKILL_IMPORT_TOTAL_TOO_LARGE",
        ExtensionError::SkillImportInvalidZip(_) => "SKILL_IMPORT_INVALID_ZIP",
        ExtensionError::PathTraversal(_) => "SKILL_IMPORT_INVALID_NAME",
        _ => "SKILL_IMPORT_FAILED",
    }
}

fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn skill_modified_time(path: &str) -> SystemTime {
    std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH)
}

fn normalize_import_source_path(source_path: &Path) -> Result<PathBuf, ExtensionError> {
    if source_path.is_file() {
        let file_name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if file_name == SKILL_MANIFEST_FILE {
            return source_path
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| ExtensionError::SkillImportInvalidSource(source_path.display().to_string()));
        }
    }
    Ok(source_path.to_path_buf())
}

async fn replace_existing_path(path: &Path) -> Result<(), ExtensionError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    if metadata.file_type().is_symlink() || metadata.is_file() {
        tokio::fs::remove_file(path).await?;
    } else {
        tokio::fs::remove_dir_all(path).await?;
    }

    Ok(())
}

fn import_staging_dir(paths: &SkillPaths, skill_name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    paths.user_skills_dir.join(format!(
        "{IMPORT_STAGING_PREFIX}{skill_name}-{}-{nonce}",
        std::process::id()
    ))
}

#[derive(Default)]
struct SkillImportBudget {
    total_bytes: u64,
}

async fn copy_skill_dir_for_import(
    src: &Path,
    dst: &Path,
    root: &Path,
    budget: &mut SkillImportBudget,
) -> Result<(), ExtensionError> {
    tokio::fs::create_dir_all(dst).await?;

    let mut entries = tokio::fs::read_dir(src).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        let metadata = tokio::fs::symlink_metadata(&entry_path).await?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionError::SkillImportSymlinkEntry(
                entry_path.display().to_string(),
            ));
        }

        let dest_path = dst.join(file_name.as_ref());
        if metadata.is_dir() {
            Box::pin(copy_skill_dir_for_import(&entry_path, &dest_path, root, budget)).await?;
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        let relative_path = entry_path
            .strip_prefix(root)
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        enforce_skill_import_budget(metadata.len(), relative_path.as_deref(), budget)?;
        tokio::fs::copy(&entry_path, &dest_path).await?;
    }

    Ok(())
}

fn enforce_skill_import_budget(
    file_bytes: u64,
    file_path: Option<&str>,
    budget: &mut SkillImportBudget,
) -> Result<(), ExtensionError> {
    if file_bytes > MAX_SKILL_IMPORT_FILE_BYTES {
        return Err(ExtensionError::SkillImportFileTooLarge {
            file_path: file_path.map(str::to_owned),
            file_bytes,
            limit_bytes: MAX_SKILL_IMPORT_FILE_BYTES,
        });
    }

    let next_total = budget.total_bytes.saturating_add(file_bytes);
    if next_total > MAX_SKILL_IMPORT_TOTAL_BYTES {
        return Err(ExtensionError::SkillImportTotalTooLarge {
            total_bytes: next_total,
            limit_bytes: MAX_SKILL_IMPORT_TOTAL_BYTES,
        });
    }

    budget.total_bytes = next_total;
    Ok(())
}

/// Export a skill by creating a symlink in the target directory.
pub async fn export_skill_with_symlink(skill_path: &Path, target_dir: &Path) -> Result<(), ExtensionError> {
    let skill_name = skill_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .ok_or_else(|| ExtensionError::InvalidSkillPath(skill_path.display().to_string()))?;

    let target_link = target_dir.join(&skill_name);
    tokio::fs::create_dir_all(target_dir).await?;

    // Remove existing link if present
    if target_link.exists() {
        if target_link.is_symlink() || target_link.is_file() {
            tokio::fs::remove_file(&target_link).await?;
        } else {
            tokio::fs::remove_dir_all(&target_link).await?;
        }
    }

    create_symlink(skill_path, &target_link).await?;

    debug!(
        skill = %skill_name,
        link = %target_link.display(),
        "skill exported (symlink)"
    );
    Ok(())
}

/// Delete a user-custom skill by name.
///
/// Returns an error if the skill is built-in or does not exist.
pub async fn delete_skill(paths: &SkillPaths, skill_name: &str) -> Result<(), ExtensionError> {
    // Safety: reject path traversal
    if skill_name.contains('/') || skill_name.contains('\\') || skill_name.contains("..") {
        return Err(ExtensionError::PathTraversal(skill_name.to_string()));
    }

    let user_path = paths.user_skills_dir.join(skill_name);
    match tokio::fs::symlink_metadata(&user_path).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Check if it exists as a built-in (disk override → filesystem,
            // otherwise embedded corpus).
            if builtin_skill_exists(paths, skill_name) {
                return Err(ExtensionError::BuiltinSkillDeletion(skill_name.to_string()));
            }
            return Err(ExtensionError::SkillNotFound(skill_name.to_string()));
        }
        Err(e) => return Err(e.into()),
    }

    replace_existing_path(&user_path).await?;

    debug!(skill = %skill_name, "skill deleted");
    Ok(())
}

/// Soft-delete a user skill through the database state source.
pub async fn delete_skill_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
    skill_name: &str,
) -> Result<(), ExtensionError> {
    validate_filename(skill_name)?;
    if builtin_skill_exists(paths, skill_name) {
        return Err(ExtensionError::BuiltinSkillDeletion(skill_name.to_string()));
    }

    let Some(row) = repo.find_by_name(skill_name).await? else {
        return Err(ExtensionError::SkillNotFound(skill_name.to_string()));
    };
    if !PathBuf::from(&row.path).is_dir() {
        warn!(
            skill = %skill_name,
            path = %row.path,
            "deleting skill whose database path is missing"
        );
    }

    repo.delete_by_name(skill_name).await?;
    debug!(skill = %skill_name, "skill marked deleted");
    Ok(())
}

/// Check whether a skill name exists in the built-in corpus — either as
/// a top-level opt-in skill or under `auto-inject/`. Consults the
/// on-disk tree at `paths.builtin_skills_dir`.
fn builtin_skill_exists(paths: &SkillPaths, skill_name: &str) -> bool {
    paths.builtin_skills_dir.join(skill_name).is_dir()
        || paths
            .builtin_skills_dir
            .join(BUILTIN_AUTO_SKILLS_SUBDIR)
            .join(skill_name)
            .is_dir()
}

// ---------------------------------------------------------------------------
// D2. Per-agent skill resolution
// ---------------------------------------------------------------------------

/// A resolved skill reference returned by [`materialize_skills_for_agent`].
///
/// `name` is the skill's requested name; `source_path` is the absolute
/// on-disk directory containing its `SKILL.md`. The caller is expected
/// to symlink that directory into the agent CLI's native skills dir
/// rather than copy it — backend no longer owns per-conversation files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentSkill {
    pub name: String,
    pub source_path: PathBuf,
}

/// Resolve each requested skill name to its on-disk source directory.
///
/// Search order per name (first match wins):
/// 1. `{builtin_skills_dir}/{name}/` — top-level opt-in builtin.
/// 2. `{builtin_skills_dir}/auto-inject/{name}/` — auto-inject builtin.
/// 3. `{user_skills_dir}/{name}/` — user-created custom skill.
/// 4. `{cron_skills_dir}/{name}/` — per-job cron skill.
///
/// No files are copied and no per-conversation directory is created —
/// the backend just hands the absolute source paths back to the caller,
/// which is responsible for symlinking them where the CLI expects. This
/// replaces the older "copy into `{data_dir}/agent-skills/{conv_id}/`"
/// behavior once the frontend moved to a symlink-only contract.
///
/// Unknown names are silently skipped (a warning is emitted). Names
/// containing path separators or `..` are rejected with a warn and
/// skipped, matching the legacy behavior. Empty names are ignored.
///
/// The returned list is sorted by `name` for determinism. The
/// `conversation_id` is still validated (rejects path-traversal values)
/// so downstream callers can safely use it in log lines or paths even
/// though this function no longer touches disk per-conversation.
pub async fn materialize_skills_for_agent(
    paths: &SkillPaths,
    conversation_id: &str,
    skills: &[String],
) -> Result<Vec<ResolvedAgentSkill>, ExtensionError> {
    validate_filename(conversation_id)?;

    let mut resolved = Vec::with_capacity(skills.len());
    for name in skills {
        if name.is_empty() {
            continue;
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            warn!(skill = %name, "skipping skill with invalid name");
            continue;
        }
        match resolve_skill_source_path(paths, name).await? {
            Some(source_path) => resolved.push(ResolvedAgentSkill {
                name: name.clone(),
                source_path,
            }),
            None => warn!(skill = %name, "skill not found in any source"),
        }
    }

    resolved.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(resolved)
}

/// Resolve requested skill names using the database for user skill state.
pub async fn materialize_skills_for_agent_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
    conversation_id: &str,
    skills: &[String],
) -> Result<Vec<ResolvedAgentSkill>, ExtensionError> {
    validate_filename(conversation_id)?;
    sync_disk_user_skills_into_repo(paths, repo).await?;

    let mut resolved = Vec::with_capacity(skills.len());
    for name in skills {
        if name.is_empty() {
            continue;
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            warn!(skill = %name, "skipping skill with invalid name");
            continue;
        }
        match resolve_skill_source_path_with_repo(paths, repo, name).await? {
            Some(source_path) => resolved.push(ResolvedAgentSkill {
                name: name.clone(),
                source_path,
            }),
            None => warn!(skill = %name, "skill not found in any source"),
        }
    }

    resolved.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(resolved)
}

/// Create symlinks from a set of resolved skills into the agent CLI's
/// native skills directories inside `workspace`.
///
/// For each relative `skills_rel_dir` (e.g. `.claude/skills`):
/// 1. Resolve the target directory. Existing `{workspace}/{skills_rel_dir}/`
///    wins; if the requested leaf is `skills` and sibling `skill` already
///    exists, reuse that singular directory; otherwise create the requested
///    directory.
/// 2. For each `{ name, source_path }` in `skills`, create a symlink
///    `{target_skills_dir}/{name} -> {source_path}`.
///
/// Existing symlinks/files at the target name are left untouched
/// (first-write-wins, matches the frontend's lstat-then-skip behavior
/// before symlink creation). Individual symlink failures are logged and
/// skipped — skill discovery degrades gracefully, it is not fatal.
///
/// Returns the number of symlinks successfully created across all
/// target dirs.
pub async fn link_workspace_skills(
    workspace: &Path,
    skills_rel_dirs: &[&str],
    skills: &[ResolvedAgentSkill],
) -> Result<usize, ExtensionError> {
    let mut created = 0usize;
    for rel in skills_rel_dirs {
        let target_skills_dir = resolve_workspace_skills_dir(workspace, rel).await;
        tokio::fs::create_dir_all(&target_skills_dir).await?;

        for skill in skills {
            let target = target_skills_dir.join(&skill.name);
            match tokio::fs::symlink_metadata(&target).await {
                // Target already exists — leave it alone.
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(
                        target = %target.display(),
                        error = %e,
                        "skipping skill link: failed to stat target"
                    );
                    continue;
                }
            }
            match link_skill_or_fallback_copy(&skill.source_path, &target).await {
                Ok(()) => {
                    debug!(
                        skill = %skill.name,
                        target = %target.display(),
                        "linked workspace skill"
                    );
                    created += 1;
                }
                Err(e) => {
                    warn!(
                        skill = %skill.name,
                        target = %target.display(),
                        error = %e,
                        "failed to link workspace skill"
                    );
                }
            }
        }
    }
    Ok(created)
}

async fn resolve_workspace_skills_dir(workspace: &Path, skills_rel_dir: &str) -> PathBuf {
    let requested = workspace.join(skills_rel_dir);
    if path_is_dir(&requested).await {
        return requested;
    }

    let rel_path = Path::new(skills_rel_dir);
    if rel_path.file_name() == Some(std::ffi::OsStr::new("skills"))
        && let Some(parent) = rel_path.parent()
    {
        let singular = workspace.join(parent).join("skill");
        if path_is_dir(&singular).await {
            return singular;
        }
    }

    requested
}

async fn path_is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

/// Resolve a skill name to its on-disk source directory using the same
/// search order as [`materialize_skills_for_agent`]. Returns `None` if
/// no matching directory exists in any known source.
async fn resolve_skill_source_path(paths: &SkillPaths, name: &str) -> Result<Option<PathBuf>, ExtensionError> {
    let top = paths.builtin_skills_dir.join(name);
    if top.is_dir() {
        return Ok(Some(top));
    }
    let auto = paths.builtin_skills_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR).join(name);
    if auto.is_dir() {
        return Ok(Some(auto));
    }
    let user = paths.user_skills_dir.join(name);
    if user.is_dir() {
        return Ok(Some(user));
    }
    let cron = paths.cron_skills_dir.join(name);
    if cron.is_dir() {
        return Ok(Some(cron));
    }
    Ok(None)
}

async fn resolve_skill_source_path_with_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
    name: &str,
) -> Result<Option<PathBuf>, ExtensionError> {
    let top = paths.builtin_skills_dir.join(name);
    if top.is_dir() {
        return Ok(Some(top));
    }
    let auto = paths.builtin_skills_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR).join(name);
    if auto.is_dir() {
        return Ok(Some(auto));
    }
    if let Some(row) = repo.find_by_name_any(name).await? {
        let path = PathBuf::from(&row.path);
        if path.is_dir() {
            return Ok(Some(path));
        }
        warn!(
            skill = %name,
            path = %path.display(),
            deleted = row.deleted_at.is_some(),
            "skill row points at a missing directory"
        );
        return Ok(None);
    }
    let cron = paths.cron_skills_dir.join(name);
    if cron.is_dir() {
        return Ok(Some(cron));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// E. Scanning & discovery
// ---------------------------------------------------------------------------

/// Scan a directory for subdirectories containing SKILL.md.
pub async fn scan_for_skills(folder_path: &Path) -> Result<Vec<ScannedSkill>, ExtensionError> {
    scan_skill_dirs(folder_path).await
}

/// Named filesystem path.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedPath {
    pub name: String,
    pub path: String,
}

/// Detect common skill paths relative to the user's home directory.
///
/// Returns paths that actually exist on the filesystem.
pub async fn detect_common_skill_paths() -> Vec<NamedPath> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for (name, rel_path, _slug) in COMMON_SKILL_DIRS {
        let full_path = home.join(rel_path);
        if full_path.exists() {
            result.push(NamedPath {
                name: (*name).to_string(),
                path: full_path.to_string_lossy().into_owned(),
            });
        }
    }

    result
}

/// An external skill source with discovered skills.
///
/// `source` is a stable slug identifying the origin — matches the
/// `ExternalSkillSourceResponse.source` contract consumed by the renderer.
/// Values are drawn from [`COMMON_SKILL_DIRS`] for built-in entries or
/// `format!("custom-{path}")` for user-added paths, so they stay unique
/// across the returned list.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalSkillSource {
    pub name: String,
    pub path: String,
    pub source: String,
    pub skill_count: usize,
    pub skills: Vec<ScannedSkill>,
}

/// Compute the stable `source` slug for a custom external path.
fn custom_source_slug(path: &str) -> String {
    format!("custom-{path}")
}

/// Discover external skills from common paths and custom external paths.
///
/// The returned list preserves deterministic `source` slugs — see
/// [`ExternalSkillSource::source`] for the contract.
pub async fn detect_and_count_external_skills(custom_paths: &[NamedPath]) -> Vec<ExternalSkillSource> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut sources = Vec::new();

    // 1. Common paths (iterate the constant table so we keep the per-entry slug).
    for (name, rel_path, slug) in COMMON_SKILL_DIRS {
        let full_path = home.join(rel_path);
        if !full_path.exists() {
            continue;
        }
        if let Ok(skills) = scan_skill_dirs(&full_path).await {
            sources.push(ExternalSkillSource {
                name: (*name).to_string(),
                path: full_path.to_string_lossy().into_owned(),
                source: (*slug).to_string(),
                skill_count: skills.len(),
                skills,
            });
        }
    }

    // 2. Custom external paths
    for np in custom_paths {
        let path = Path::new(&np.path);
        if let Ok(skills) = scan_skill_dirs(path).await {
            sources.push(ExternalSkillSource {
                name: np.name.clone(),
                path: np.path.clone(),
                source: custom_source_slug(&np.path),
                skill_count: skills.len(),
                skills,
            });
        }
    }

    sources
}

/// Get the user and built-in skill directory paths.
///
/// Both values are real on-disk paths. The built-in path points at the
/// tree populated at startup by
/// [`crate::startup_materialize::materialize_if_needed`], or at the
/// [`BUILTIN_SKILLS_ENV_VAR`] override when set.
pub fn get_skill_paths(paths: &SkillPaths) -> (String, String) {
    (
        paths.user_skills_dir.to_string_lossy().into_owned(),
        paths.builtin_skills_dir.to_string_lossy().into_owned(),
    )
}

async fn list_skills_from_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
) -> Result<Vec<SkillListItem>, ExtensionError> {
    let mut items = Vec::new();
    for row in repo.list().await? {
        let description = row.description.clone().unwrap_or_default();
        items.push(skill_row_to_list_item(paths, row, description));
    }
    Ok(items)
}

/// Synchronize filesystem-backed skill catalogs into the database.
///
/// Listing endpoints read the database only. This synchronization is intended
/// for startup/refresh paths and imports so the database remains the single
/// catalog source for API consumers.
pub async fn sync_skill_catalog_into_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
) -> Result<(), ExtensionError> {
    sync_disk_user_skills_into_repo(paths, repo).await?;
    sync_builtin_skills_into_repo(paths, repo).await?;
    Ok(())
}

async fn sync_builtin_skills_into_repo(paths: &SkillPaths, repo: &dyn ISkillRepository) -> Result<(), ExtensionError> {
    if let Ok(skills) = scan_skill_dirs(&paths.builtin_skills_dir).await {
        for skill in skills {
            if skill.name == BUILTIN_AUTO_SKILLS_SUBDIR {
                continue;
            }
            sync_managed_skill_into_repo(repo, &skill, "builtin").await?;
        }
    }

    let auto_inject_dir = paths.builtin_skills_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
    if let Ok(skills) = scan_skill_dirs(&auto_inject_dir).await {
        for skill in skills {
            sync_managed_skill_into_repo(repo, &skill, "builtin").await?;
        }
    }

    if let Ok(skills) = scan_skill_dirs(&paths.cron_skills_dir).await {
        for skill in skills {
            sync_managed_skill_into_repo(repo, &skill, "cron").await?;
        }
    }

    Ok(())
}

async fn sync_managed_skill_into_repo(
    repo: &dyn ISkillRepository,
    skill: &ScannedSkill,
    source: &str,
) -> Result<(), ExtensionError> {
    if let Some(existing) = repo.find_by_name_any(&skill.name).await?
        && existing.source == "user"
        && existing.deleted_at.is_none()
        && existing.enabled
    {
        return Ok(());
    }

    repo.upsert(UpsertSkillParams {
        name: &skill.name,
        description: Some(&skill.description),
        path: &skill.path,
        source,
        enabled: true,
    })
    .await?;
    Ok(())
}

async fn sync_disk_user_skills_into_repo(
    paths: &SkillPaths,
    repo: &dyn ISkillRepository,
) -> Result<(), ExtensionError> {
    let scanned = scan_skill_dirs(&paths.user_skills_dir).await?;
    let mut backfilled = 0usize;

    for skill in scanned {
        if let Err(err) = validate_filename(&skill.name) {
            warn!(
                skill = %skill.name,
                error = %err,
                "skipping existing user skill with invalid name during database backfill"
            );
            continue;
        }
        if repo.find_by_name_any(&skill.name).await?.is_some() {
            continue;
        }

        repo.upsert(UpsertSkillParams {
            name: &skill.name,
            description: Some(&skill.description),
            path: &skill.path,
            source: "user",
            enabled: true,
        })
        .await?;
        backfilled += 1;
    }

    if backfilled > 0 {
        info!(
            count = backfilled,
            "backfilled existing user skill directories into skill database"
        );
    }

    Ok(())
}

fn skill_row_to_list_item(paths: &SkillPaths, row: SkillRow, description: String) -> SkillListItem {
    let source = skill_source_from_row(&row.source);
    let relative_location = skill_relative_location(paths, &row, source);
    let location = match source {
        SkillSource::Builtin | SkillSource::Cron => PathBuf::from(&row.path)
            .join(SKILL_MANIFEST_FILE)
            .to_string_lossy()
            .into_owned(),
        SkillSource::Custom | SkillSource::Extension => row.path.clone(),
    };

    SkillListItem {
        name: row.name,
        description,
        location,
        relative_location,
        is_custom: source == SkillSource::Custom,
        source,
    }
}

fn skill_source_from_row(source: &str) -> SkillSource {
    match source {
        "builtin" => SkillSource::Builtin,
        "cron" => SkillSource::Cron,
        "extension" => SkillSource::Extension,
        _ => SkillSource::Custom,
    }
}

fn skill_relative_location(paths: &SkillPaths, row: &SkillRow, source: SkillSource) -> Option<String> {
    let skill_dir = Path::new(&row.path);
    match source {
        SkillSource::Builtin => relative_skill_manifest_path(&paths.builtin_skills_dir, skill_dir),
        SkillSource::Cron => None,
        SkillSource::Custom | SkillSource::Extension => None,
    }
}

fn relative_skill_manifest_path(base_dir: &Path, skill_dir: &Path) -> Option<String> {
    let relative_dir = skill_dir.strip_prefix(base_dir).ok()?;
    let mut relative = relative_dir.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        return None;
    }
    relative.push('/');
    relative.push_str(SKILL_MANIFEST_FILE);
    Some(relative)
}

async fn list_user_skills_from_disk(paths: &SkillPaths) -> Result<Vec<SkillListItem>, ExtensionError> {
    let scanned = scan_skill_dirs(&paths.user_skills_dir).await?;
    Ok(scanned
        .into_iter()
        .map(|skill| SkillListItem {
            name: skill.name,
            description: skill.description,
            location: skill.path,
            relative_location: None,
            is_custom: true,
            source: SkillSource::Custom,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read a file and return its content, or an empty string if it does not exist.
async fn read_file_or_empty(path: &Path) -> Result<String, ExtensionError> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ExtensionError::Io(e)),
    }
}

/// Validate a filename to prevent path traversal.
fn validate_filename(name: &str) -> Result<(), ExtensionError> {
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.is_empty() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }
    Ok(())
}

/// Validate a relative path inside the built-in skill corpus. Allows
/// forward slashes (paths like `"auto-inject/cron/SKILL.md"` are
/// normal) but forbids empty segments, backslashes, leading slash,
/// absolute paths, and any `..` component.
fn validate_builtin_skill_path(rel: &str) -> Result<(), ExtensionError> {
    if rel.is_empty() || rel.contains('\\') || rel.contains("..") || rel.starts_with('/') {
        return Err(ExtensionError::PathTraversal(rel.to_string()));
    }
    if rel.split('/').any(|seg| seg.is_empty()) {
        return Err(ExtensionError::PathTraversal(rel.to_string()));
    }
    if Path::new(rel).is_absolute() {
        return Err(ExtensionError::PathTraversal(rel.to_string()));
    }
    Ok(())
}

/// Read an assistant resource (rule or skill) with locale fallback.
async fn read_assistant_resource(
    dir: &Path,
    assistant_id: &str,
    locale: Option<&str>,
) -> Result<String, ExtensionError> {
    validate_filename(assistant_id)?;
    if let Some(loc) = locale {
        validate_filename(loc)?;
    }

    // 1. Try locale-specific file
    if let Some(loc) = locale
        && !loc.is_empty()
    {
        let locale_file = dir.join(format!("{assistant_id}.{loc}.md"));
        if let Ok(content) = tokio::fs::read_to_string(&locale_file).await {
            return Ok(content);
        }
    }

    // 2. Try default file (no locale suffix)
    let default_file = dir.join(format!("{assistant_id}.md"));
    match tokio::fs::read_to_string(&default_file).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ExtensionError::Io(e)),
    }
}

/// Write an assistant resource file.
async fn write_assistant_resource(
    dir: &Path,
    assistant_id: &str,
    content: &str,
    locale: Option<&str>,
) -> Result<bool, ExtensionError> {
    validate_filename(assistant_id)?;
    if let Some(loc) = locale {
        validate_filename(loc)?;
    }

    tokio::fs::create_dir_all(dir).await?;

    let filename = match locale {
        Some(loc) if !loc.is_empty() => format!("{assistant_id}.{loc}.md"),
        _ => format!("{assistant_id}.md"),
    };

    let file_path = dir.join(filename);
    tokio::fs::write(&file_path, content).await?;
    debug!(path = %file_path.display(), "assistant resource written");
    Ok(true)
}

/// Delete all files matching `{assistant_id}*.md` in a directory.
async fn delete_assistant_resource(dir: &Path, assistant_id: &str) -> Result<bool, ExtensionError> {
    validate_filename(assistant_id)?;

    let mut deleted_any = false;

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ExtensionError::Io(e)),
    };

    let prefix = format!("{assistant_id}.");
    let exact = format!("{assistant_id}.md");

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == exact || (name.starts_with(&prefix) && name.ends_with(".md")) {
            tokio::fs::remove_file(entry.path()).await?;
            deleted_any = true;
            debug!(file = %name, "deleted assistant resource");
        }
    }

    Ok(deleted_any)
}

/// Scan a directory for subdirectories containing a SKILL.md file.
async fn scan_skill_dirs(dir: &Path) -> Result<Vec<ScannedSkill>, ExtensionError> {
    let mut result = Vec::new();
    let mut skill_dirs = Vec::new();
    collect_skill_dirs_recursive(dir, &mut skill_dirs).await?;

    for entry_path in skill_dirs {
        let skill_file = entry_path.join(SKILL_MANIFEST_FILE);
        match tokio::fs::read_to_string(&skill_file).await {
            Ok(content) => {
                if let Some((name, description)) = parse_frontmatter_fields(&content) {
                    let final_name = if name.is_empty() {
                        entry_path
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    } else {
                        name
                    };
                    result.push(ScannedSkill {
                        name: final_name,
                        description,
                        path: entry_path.to_string_lossy().into_owned(),
                    });
                }
            }
            Err(e) => {
                warn!(
                    path = %skill_file.display(),
                    error = %e,
                    "failed to read SKILL.md"
                );
            }
        }
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

async fn collect_skill_dirs_recursive(dir: &Path, result: &mut Vec<PathBuf>) -> Result<(), ExtensionError> {
    if dir.join(SKILL_MANIFEST_FILE).exists() {
        result.push(dir.to_path_buf());
        return Ok(());
    }

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ExtensionError::Io(e)),
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            Box::pin(collect_skill_dirs_recursive(&entry_path, result)).await?;
        }
    }

    result.sort();
    Ok(())
}

fn extract_zip_archive(archive_path: &Path, destination: &Path) -> Result<(), ExtensionError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        let entry_name = entry.name().to_string();
        reject_zip_symlink(&entry)?;
        let relative_path = safe_zip_entry_path(&entry_name)?;
        let output_path = destination.join(relative_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&output_path)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&output_path)?;
        io::copy(&mut entry, &mut output)?;
    }

    Ok(())
}

fn safe_zip_entry_path(name: &str) -> Result<PathBuf, ExtensionError> {
    if name.is_empty() || name.contains('\\') {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let path = Path::new(name);
    if path.is_absolute() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let mut safe_path = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe_path.push(part),
            Component::CurDir => {}
            _ => return Err(ExtensionError::PathTraversal(name.to_string())),
        }
    }

    if safe_path.as_os_str().is_empty() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    Ok(safe_path)
}

fn reject_zip_symlink(entry: &zip::read::ZipFile<'_>) -> Result<(), ExtensionError> {
    if let Some(mode) = entry.unix_mode()
        && mode & 0o170000 == 0o120000
    {
        return Err(ExtensionError::SkillImportSymlinkEntry(entry.name().to_string()));
    }
    Ok(())
}

fn zip_error(err: zip::result::ZipError) -> ExtensionError {
    ExtensionError::SkillImportInvalidZip(err.to_string())
}

/// Parse SKILL.md frontmatter to extract name and description.
///
/// Expected format:
/// ```text
/// ---
/// name: skill-name
/// description: One line description
/// ---
/// Body content here...
/// ```
fn parse_frontmatter_fields(content: &str) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct SkillFrontmatter {
        #[serde(default)]
        name: String,
        description: String,
    }

    let frontmatter = extract_frontmatter_text(content)?;
    let parsed = serde_yaml::from_str::<SkillFrontmatter>(frontmatter).ok()?;
    let description = parsed.description.trim().to_string();

    if description.is_empty() {
        return None;
    }

    Some((parsed.name.trim().to_string(), description))
}

fn extract_frontmatter_text(content: &str) -> Option<&str> {
    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;

    let mut pos = 0;
    for line in after_open.lines() {
        let raw = &after_open[pos..];
        let line_len = line.len();
        let line_with_ending_len = if raw[line_len..].starts_with("\r\n") {
            line_len + 2
        } else if raw[line_len..].starts_with('\n') {
            line_len + 1
        } else {
            line_len
        };

        if line == "---" {
            let yaml_text = &after_open[..pos];
            return Some(
                yaml_text
                    .strip_suffix("\r\n")
                    .or_else(|| yaml_text.strip_suffix('\n'))
                    .unwrap_or(yaml_text),
            );
        }

        pos += line_with_ending_len;
    }

    None
}

/// Recursively copy a directory.
async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    tokio::fs::create_dir_all(dst).await?;

    let mut entries = tokio::fs::read_dir(src).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        let dest_path = dst.join(entry.file_name());

        if entry_path.is_dir() {
            Box::pin(copy_dir_recursive(&entry_path, &dest_path)).await?;
        } else {
            tokio::fs::copy(&entry_path, &dest_path).await?;
        }
    }

    Ok(())
}

/// Try to symlink `src` into `dst`; on failure, fall back to a recursive
/// copy of the source directory.
///
/// Motivation: on Windows machines without "Developer Mode" or admin
/// privileges, `CreateSymbolicLinkW` fails with `os error 1314`
/// (`ERROR_PRIVILEGE_NOT_HELD`). Auto-injected builtin skills under each
/// backend's `.<backend>/skills/` directory then become invisible to the
/// CLI agent — silently degrading the product. Falling back to a copy
/// keeps the skills discoverable; the trade-off is that copies do not
/// track upstream changes until the next link pass clears them. The
/// fallback applies on every platform (Linux/macOS shouldn't normally
/// hit this, but we keep behavior uniform so a future EPERM/EROFS sandbox
/// also stays healthy).
///
/// Logs a `warn!` with the OS error kind and `raw_os_error` so we can
/// keep tracking 1314 vs other failure modes in telemetry. No
/// user-identifying data is logged — only the source/target paths
/// (already considered safe to log elsewhere in this module) and the
/// error code.
async fn link_skill_or_fallback_copy(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    match create_symlink_for_link(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Surface the raw OS error so dashboards can keep counting 1314
            // (ERROR_PRIVILEGE_NOT_HELD) separately from other failure modes.
            let raw_os_error = match &e {
                ExtensionError::Io(io_err) => io_err.raw_os_error(),
                _ => None,
            };
            warn!(
                src = %src.display(),
                dst = %dst.display(),
                error = %e,
                raw_os_error = ?raw_os_error,
                "create_symlink failed; falling back to copy_dir_recursive"
            );
            copy_dir_recursive(src, dst).await
        }
    }
}

/// Wrapper around [`create_symlink`] that allows tests to inject a
/// synthetic failure. In non-test builds this is a thin call-through to
/// the platform-specific [`create_symlink`] below.
async fn create_symlink_for_link(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    #[cfg(test)]
    {
        if test_overrides::should_force_symlink_failure() {
            // Use PermissionDenied to mimic the shape Windows returns
            // for ERROR_PRIVILEGE_NOT_HELD. The exact raw_os_error is
            // platform-specific so we only assert on kind in tests.
            return Err(ExtensionError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "forced symlink failure (test)",
            )));
        }
    }
    create_symlink(src, dst).await
}

/// Test-only knob to force the symlink primitive to fail, exercising
/// the [`copy_dir_recursive`] fallback branch on platforms where
/// symlinking would otherwise succeed (Linux/macOS CI).
#[cfg(test)]
mod test_overrides {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FORCE_SYMLINK_FAILURE: AtomicBool = AtomicBool::new(false);

    pub fn should_force_symlink_failure() -> bool {
        FORCE_SYMLINK_FAILURE.load(Ordering::SeqCst)
    }

    /// RAII guard that flips `FORCE_SYMLINK_FAILURE` on creation and
    /// resets it on drop. Tests using this guard must be marked
    /// `#[serial_test::serial]` if any other test in the binary also
    /// flips the flag — at present only one test uses it, so a guard
    /// is enough.
    pub struct ForceFailureGuard;

    impl ForceFailureGuard {
        pub fn new() -> Self {
            FORCE_SYMLINK_FAILURE.store(true, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for ForceFailureGuard {
        fn drop(&mut self) {
            FORCE_SYMLINK_FAILURE.store(false, Ordering::SeqCst);
        }
    }
}

/// Create a symlink (platform-aware).
#[cfg(unix)]
async fn create_symlink(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    tokio::fs::symlink(src, dst).await.map_err(ExtensionError::Io)
}

#[cfg(windows)]
async fn create_symlink(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    // On Windows, directory symlinks require `SeCreateSymbolicLink`
    // (Developer Mode or Admin), which most users don't have — this is
    // the source of the Sentry I1 family of `os error 1314` failures.
    //
    // NTFS junctions are an unprivileged alternative for *directory*
    // targets: the kernel exposes them via `FSCTL_SET_REPARSE_POINT`
    // which does not require the symlink privilege. Use them whenever
    // possible. File targets cannot be junctioned, so they fall back to
    // `tokio::fs::symlink_file`; in the rare cases that fails the
    // outer `link_skill_or_fallback_copy` wrapper still rescues us via
    // `copy_dir_recursive`.
    if src.is_dir() {
        let src = src.to_path_buf();
        let dst = dst.to_path_buf();
        tokio::task::spawn_blocking(move || junction::create(&src, &dst))
            .await
            .map_err(|e| ExtensionError::Io(std::io::Error::other(format!("junction::create join error: {e}"))))?
            .map_err(ExtensionError::Io)
    } else {
        tokio::fs::symlink_file(src, dst).await.map_err(ExtensionError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{ISkillRepository, SqliteSkillRepository, init_database_memory};
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Frontmatter parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frontmatter_valid() {
        let content = "---\nname: my-skill\ndescription: A useful skill\n---\nBody content here.";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "A useful skill");
    }

    #[test]
    fn parse_frontmatter_rejects_invalid_yaml() {
        let content = "---\nname: video-skill\ndescription: Download video: supports batch URLs\n---\nBody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_accepts_quoted_yaml_description() {
        let content = "---\nname: video-skill\ndescription: \"Download video: supports batch URLs\"\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "video-skill");
        assert_eq!(desc, "Download video: supports batch URLs");
    }

    #[test]
    fn parse_frontmatter_accepts_block_scalar_description() {
        let content = "---\nname: douyin-downloader\ndescription: |\n  Download Douyin videos without watermark.\n  Supports batch downloads.\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "douyin-downloader");
        assert_eq!(
            desc,
            "Download Douyin videos without watermark.\nSupports batch downloads."
        );
    }

    #[test]
    fn parse_frontmatter_requires_opening_fence_line() {
        let content = " ---\nname: test\ndescription: desc\n---\nbody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_requires_closing_fence_line() {
        let content = "---\nname: test\ndescription: this has --- inside\nbody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_empty_name() {
        let content = "---\nname: \ndescription: Has description\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert!(name.is_empty());
        assert_eq!(desc, "Has description");
    }

    #[test]
    fn parse_frontmatter_no_opening() {
        let content = "name: test\ndescription: desc\n---\nbody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_no_closing() {
        let content = "---\nname: test\ndescription: desc";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_missing_description() {
        let content = "---\nname: test\n---\nbody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    // -----------------------------------------------------------------------
    // Filename validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_filename_normal() {
        assert!(validate_filename("code-review.md").is_ok());
    }

    #[test]
    fn validate_filename_path_traversal() {
        assert!(validate_filename("../etc/passwd").is_err());
        assert!(validate_filename("foo/bar.md").is_err());
        assert!(validate_filename("foo\\bar.md").is_err());
    }

    #[test]
    fn validate_filename_empty() {
        assert!(validate_filename("").is_err());
    }

    // -----------------------------------------------------------------------
    // Built-in resource reading
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_builtin_rule_existing_file() {
        let tmp = TempDir::new().unwrap();
        let rules_dir = tmp.path().join(BUILTIN_RULES_DIR_NAME);
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("code-review.md"), "# Review rules").unwrap();

        let paths = SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join(SKILLS_DIR_NAME),
            cron_skills_dir: tmp.path().join(CRON_SKILLS_DIR_NAME),
            builtin_skills_dir: tmp.path().join(crate::constants::BUILTIN_SKILLS_DIR_NAME),
            builtin_rules_dir: rules_dir,
            assistant_rules_dir: tmp.path().join(ASSISTANT_RULES_DIR_NAME),
            assistant_skills_dir: tmp.path().join(ASSISTANT_SKILLS_DIR_NAME),
        };

        let content = read_builtin_rule(&paths, "code-review.md").await.unwrap();
        assert_eq!(content, "# Review rules");
    }

    #[tokio::test]
    async fn read_builtin_rule_missing_file() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let content = read_builtin_rule(&paths, "nonexistent.md").await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn read_builtin_rule_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let result = read_builtin_rule(&paths, "../secret.md").await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Assistant CRUD
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn assistant_rule_write_and_read() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        write_assistant_rule(&paths, "abc123", "Be helpful.", None)
            .await
            .unwrap();

        let content = read_assistant_rule(&paths, "abc123", None).await.unwrap();
        assert_eq!(content, "Be helpful.");
    }

    #[tokio::test]
    async fn assistant_rule_locale_fallback() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Write default (no locale)
        write_assistant_rule(&paths, "abc123", "Default content", None)
            .await
            .unwrap();

        // Write zh-CN locale
        write_assistant_rule(&paths, "abc123", "中文内容", Some("zh-CN"))
            .await
            .unwrap();

        // Read with matching locale
        let content = read_assistant_rule(&paths, "abc123", Some("zh-CN")).await.unwrap();
        assert_eq!(content, "中文内容");

        // Read with non-matching locale → falls back to default
        let content = read_assistant_rule(&paths, "abc123", Some("ja-JP")).await.unwrap();
        assert_eq!(content, "Default content");

        // Read without locale → default
        let content = read_assistant_rule(&paths, "abc123", None).await.unwrap();
        assert_eq!(content, "Default content");
    }

    #[tokio::test]
    async fn assistant_rule_read_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let content = read_assistant_rule(&paths, "missing", None).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn assistant_rule_delete_all_locales() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        write_assistant_rule(&paths, "abc123", "Default", None).await.unwrap();
        write_assistant_rule(&paths, "abc123", "Chinese", Some("zh-CN"))
            .await
            .unwrap();
        write_assistant_rule(&paths, "abc123", "English", Some("en-US"))
            .await
            .unwrap();

        let deleted = delete_assistant_rule(&paths, "abc123").await.unwrap();
        assert!(deleted);

        // Verify all files are gone
        let content = read_assistant_rule(&paths, "abc123", None).await.unwrap();
        assert!(content.is_empty());
        let content = read_assistant_rule(&paths, "abc123", Some("zh-CN")).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn assistant_skill_write_and_read() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        write_assistant_skill(&paths, "abc123", "Skill content", None)
            .await
            .unwrap();

        let content = read_assistant_skill(&paths, "abc123", None).await.unwrap();
        assert_eq!(content, "Skill content");
    }

    // -----------------------------------------------------------------------
    // Assistant CRUD — path traversal prevention
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_assistant_rule_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = read_assistant_rule(&paths, "../etc/passwd", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn read_assistant_rule_rejects_traversal_locale() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = read_assistant_rule(&paths, "valid-id", Some("../evil")).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn write_assistant_rule_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = write_assistant_rule(&paths, "../../escape", "content", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn write_assistant_rule_rejects_traversal_locale() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = write_assistant_rule(&paths, "valid-id", "content", Some("../bad")).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn delete_assistant_rule_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = delete_assistant_rule(&paths, "foo/bar").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn read_assistant_skill_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = read_assistant_skill(&paths, "..\\windows", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn write_assistant_skill_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = write_assistant_skill(&paths, "../escape", "content", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn delete_assistant_skill_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = delete_assistant_skill(&paths, "a/b").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    // -----------------------------------------------------------------------
    // Skill listing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_skills_builtin_and_custom() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();

        // Create builtin skills
        create_skill_in_dir(&builtin_dir, "review", "Code review skill");
        create_skill_in_dir(&builtin_dir, "debug", "Debugging skill");

        // Create custom skill (overrides review)
        create_skill_in_dir(&paths.user_skills_dir, "review", "Custom review skill");
        create_skill_in_dir(&paths.user_skills_dir, "my-skill", "My custom skill");

        let skills = list_available_skills(&paths).await.unwrap();

        assert_eq!(skills.len(), 3); // debug + review (custom) + my-skill

        let review = skills.iter().find(|s| s.name == "review").unwrap();
        assert!(review.is_custom);
        assert_eq!(review.description, "Custom review skill");
        assert_eq!(review.source, SkillSource::Custom);

        let debug_skill = skills.iter().find(|s| s.name == "debug").unwrap();
        assert!(!debug_skill.is_custom);
        assert_eq!(debug_skill.source, SkillSource::Builtin);
        assert_eq!(debug_skill.relative_location.as_deref(), Some("debug/SKILL.md"));

        let my_skill = skills.iter().find(|s| s.name == "my-skill").unwrap();
        assert_eq!(my_skill.source, SkillSource::Custom);
        assert!(my_skill.relative_location.is_none());
    }

    #[tokio::test]
    async fn list_skills_empty_dirs() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let skills = list_available_skills(&paths).await.unwrap();
        assert!(skills.is_empty());
    }

    // -----------------------------------------------------------------------
    // Built-in auto skills
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_auto_inject_skills_from_disk_override() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();
        let auto_dir = builtin_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);

        create_skill_in_dir(&auto_dir, "cron", "Schedule recurring tasks");
        create_skill_in_dir(&auto_dir, "skill-creator", "Scaffold a new skill");

        // A top-level built-in skill (NOT under auto-inject/) must be excluded.
        create_skill_in_dir(&builtin_dir, "review", "Top-level builtin");

        let autos = list_auto_inject_skills_from_disk(&paths).await.unwrap();

        assert_eq!(autos.len(), 2);
        let names: std::collections::HashSet<_> = autos.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains("cron"));
        assert!(names.contains("skill-creator"));
        assert!(!names.contains("review"));

        let cron = autos.iter().find(|s| s.name == "cron").unwrap();
        assert_eq!(cron.description, "Schedule recurring tasks");
        assert_eq!(cron.location, "auto-inject/cron/SKILL.md");
    }

    #[tokio::test]
    async fn list_auto_inject_skills_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        // No auto-inject/ directory created under the disk override.

        let autos = list_auto_inject_skills_from_disk(&paths).await.unwrap();
        assert!(autos.is_empty());
    }

    // -----------------------------------------------------------------------
    // Skill info
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_skill_info_valid() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: A test skill\n---\nBody",
        )
        .unwrap();

        let (name, desc) = read_skill_info(&skill_dir).await.unwrap();
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "A test skill");
    }

    #[tokio::test]
    async fn read_skill_info_missing() {
        let tmp = TempDir::new().unwrap();
        let result = read_skill_info(&tmp.path().join("nonexistent")).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Skill import / delete
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn import_skill_copies_directory() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Create source skill
        let source_dir = tmp.path().join("source-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: imported\ndescription: Imported skill\n---\nBody",
        )
        .unwrap();
        std::fs::write(source_dir.join("extra.txt"), "extra data").unwrap();

        let name = import_skill(&paths, &source_dir).await.unwrap();
        assert_eq!(name, "imported");

        // Verify the skill was copied
        let imported_dir = paths.user_skills_dir.join("imported");
        assert!(imported_dir.join(SKILL_MANIFEST_FILE).exists());
        assert!(imported_dir.join("extra.txt").exists());
    }

    #[tokio::test]
    async fn import_skills_imports_selected_skill_manifest_parent() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("single-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: selected-manifest\ndescription: Selected manifest skill\n---\nBody",
        )
        .unwrap();

        let outcome = import_skills(&paths, &source_dir.join(SKILL_MANIFEST_FILE))
            .await
            .unwrap();
        assert_eq!(outcome.imported, vec!["selected-manifest"]);
        assert!(outcome.failed.is_empty());

        let imported_path = paths.user_skills_dir.join("selected-manifest");
        assert!(!imported_path.is_symlink());
        assert!(imported_path.join(SKILL_MANIFEST_FILE).exists());

        std::fs::remove_dir_all(&source_dir).unwrap();
        assert!(imported_path.join(SKILL_MANIFEST_FILE).exists());
    }

    #[tokio::test]
    async fn import_skills_imports_parent_directory_children() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("skill-pack");
        create_skill_in_dir(&source_dir, "alpha", "Alpha skill");
        create_skill_in_dir(&source_dir, "beta", "Beta skill");

        let outcome = import_skills(&paths, &source_dir).await.unwrap();
        assert_eq!(outcome.imported, vec!["alpha", "beta"]);
        assert!(outcome.failed.is_empty());
        assert!(!paths.user_skills_dir.join("alpha").is_symlink());
        assert!(!paths.user_skills_dir.join("beta").is_symlink());
        assert!(paths.user_skills_dir.join("alpha").join(SKILL_MANIFEST_FILE).exists());
        assert!(paths.user_skills_dir.join("beta").join(SKILL_MANIFEST_FILE).exists());
    }

    #[tokio::test]
    async fn import_skills_imports_mixed_folder_without_copying_non_skill_siblings() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("mixed-import-root");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("README.md"), "import notes").unwrap();
        std::fs::write(source_dir.join(".DS_Store"), "metadata").unwrap();
        std::fs::write(source_dir.join("skills.zip"), "not a real zip").unwrap();
        create_skill_in_dir(&source_dir, "alpha", "Alpha skill");
        create_skill_in_dir(&source_dir.join("nested-pack"), "beta", "Beta skill");

        let outcome = import_skills(&paths, &source_dir).await.unwrap();

        assert_eq!(outcome.imported, vec!["alpha", "beta"]);
        assert!(outcome.failed.is_empty());
        assert!(!paths.user_skills_dir.join("mixed-import-root").exists());
        assert!(!paths.user_skills_dir.join("README.md").exists());
        assert!(!paths.user_skills_dir.join(".DS_Store").exists());
        assert!(!paths.user_skills_dir.join("skills.zip").exists());
        assert!(paths.user_skills_dir.join("alpha").join(SKILL_MANIFEST_FILE).exists());
        assert!(paths.user_skills_dir.join("beta").join(SKILL_MANIFEST_FILE).exists());
    }

    #[tokio::test]
    async fn import_skill_copies_small_files_without_name_based_filtering() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("small-files-source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: small-files\ndescription: Small files\n---\nBody",
        )
        .unwrap();
        std::fs::write(source_dir.join(".DS_Store"), "finder metadata").unwrap();
        std::fs::create_dir_all(source_dir.join(".git")).unwrap();
        std::fs::write(source_dir.join(".git/config"), "repo metadata").unwrap();

        let name = import_skill(&paths, &source_dir).await.unwrap();

        assert_eq!(name, "small-files");
        let imported = paths.user_skills_dir.join("small-files");
        assert!(imported.join(SKILL_MANIFEST_FILE).exists());
        assert!(imported.join(".DS_Store").exists());
        assert!(imported.join(".git/config").exists());
    }

    #[tokio::test]
    async fn import_skill_rejects_oversized_files_without_leaving_partial_copy() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("large-source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: too-large\ndescription: Too large\n---\nBody",
        )
        .unwrap();
        let large_file = std::fs::File::create(source_dir.join("movie.mp4")).unwrap();
        large_file.set_len(MAX_SKILL_IMPORT_FILE_BYTES + 1).unwrap();

        let result = import_skill(&paths, &source_dir).await;

        assert!(matches!(result, Err(ExtensionError::SkillImportFileTooLarge { .. })));
        assert!(!paths.user_skills_dir.join("too-large").exists());
        assert!(!has_import_staging_dirs(&paths.user_skills_dir));
    }

    #[tokio::test]
    async fn import_skills_replaces_dangling_link_with_copy() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let stale_source = tmp.path().join("stale-source");
        create_skill_in_dir(&stale_source, "dangling", "Stale source");
        let stale_skill_dir = stale_source.join("dangling");
        let target = paths.user_skills_dir.join("dangling");
        tokio::fs::create_dir_all(&paths.user_skills_dir).await.unwrap();
        create_symlink(&stale_skill_dir, &target).await.unwrap();
        std::fs::remove_dir_all(&stale_source).unwrap();

        let fresh_source = tmp.path().join("fresh-source");
        create_skill_in_dir(&fresh_source, "dangling", "Fresh source");

        let outcome = import_skills(&paths, &fresh_source).await.unwrap();

        assert_eq!(outcome.imported, vec!["dangling"]);
        assert!(outcome.failed.is_empty());
        assert!(!target.is_symlink());
        assert!(target.join(SKILL_MANIFEST_FILE).exists());
        assert!(
            std::fs::read_to_string(target.join(SKILL_MANIFEST_FILE))
                .unwrap()
                .contains("Fresh source")
        );
    }

    #[tokio::test]
    async fn list_available_skills_orders_custom_skills_by_newest_import_first() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let older_dir = tmp.path().join("older-source");
        let newer_dir = tmp.path().join("newer-source");
        std::fs::create_dir_all(&older_dir).unwrap();
        std::fs::create_dir_all(&newer_dir).unwrap();
        std::fs::write(
            older_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: older-skill\ndescription: Older skill\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            newer_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: newer-skill\ndescription: Newer skill\n---\nBody",
        )
        .unwrap();

        import_skill(&paths, &older_dir).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        import_skill(&paths, &newer_dir).await.unwrap();

        let skills = list_available_skills(&paths).await.unwrap();
        let names: Vec<_> = skills.into_iter().map(|skill| skill.name).collect();
        assert_eq!(names[0], "newer-skill");
        assert_eq!(names[1], "older-skill");
    }

    #[tokio::test]
    async fn list_available_skills_with_repo_reads_database_without_filesystem_backfill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let repo = make_test_skill_repo().await;

        create_skill_in_dir(&paths.user_skills_dir, "existing-disk-skill", "Existing disk skill");

        let skills = list_available_skills_with_repo(&paths, &repo).await.unwrap();

        assert!(!skills.iter().any(|skill| skill.name == "existing-disk-skill"));
        assert!(repo.find_by_name("existing-disk-skill").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sync_skill_catalog_into_repo_backfills_existing_user_skill_dirs() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let repo = make_test_skill_repo().await;

        create_skill_in_dir(&paths.user_skills_dir, "existing-disk-skill", "Existing disk skill");

        sync_skill_catalog_into_repo(&paths, &repo).await.unwrap();

        let row = repo.find_by_name("existing-disk-skill").await.unwrap().unwrap();
        assert_eq!(row.source, "user");
        assert_eq!(
            row.path,
            paths.user_skills_dir.join("existing-disk-skill").to_string_lossy()
        );
    }

    #[tokio::test]
    async fn list_available_skills_with_repo_does_not_restore_soft_deleted_disk_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let repo = make_test_skill_repo().await;

        create_skill_in_dir(tmp.path(), "deleted-disk-skill", "Deleted disk skill");
        import_skill_with_repo(&paths, &repo, &tmp.path().join("deleted-disk-skill"))
            .await
            .unwrap();
        delete_skill_with_repo(&paths, &repo, "deleted-disk-skill")
            .await
            .unwrap();

        sync_skill_catalog_into_repo(&paths, &repo).await.unwrap();
        let skills = list_available_skills_with_repo(&paths, &repo).await.unwrap();

        assert!(!skills.iter().any(|skill| skill.name == "deleted-disk-skill"));
        assert!(repo.find_by_name("deleted-disk-skill").await.unwrap().is_none());
        assert!(repo.find_by_name_any("deleted-disk-skill").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_available_skills_with_repo_syncs_builtin_auto_inject_and_cron_sources_into_repo() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let repo = make_test_skill_repo().await;
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();
        let auto_dir = builtin_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);

        create_skill_in_dir(&builtin_dir, "debug", "Debugging skill");
        create_skill_in_dir(&auto_dir, "cron", "Auto injected cron skill");
        create_skill_in_dir(&paths.cron_skills_dir, "scheduled-task", "Scheduled task skill");

        sync_skill_catalog_into_repo(&paths, &repo).await.unwrap();
        let skills = list_available_skills_with_repo(&paths, &repo).await.unwrap();

        let debug = skills.iter().find(|skill| skill.name == "debug").unwrap();
        assert_eq!(debug.source, SkillSource::Builtin);
        assert_eq!(debug.relative_location.as_deref(), Some("debug/SKILL.md"));
        let debug_row = repo.find_by_name("debug").await.unwrap().unwrap();
        assert_eq!(debug_row.source, "builtin");

        let auto_cron = skills.iter().find(|skill| skill.name == "cron").unwrap();
        assert_eq!(auto_cron.source, SkillSource::Builtin);
        assert_eq!(
            auto_cron.relative_location.as_deref(),
            Some("auto-inject/cron/SKILL.md")
        );
        let auto_cron_row = repo.find_by_name("cron").await.unwrap().unwrap();
        assert_eq!(auto_cron_row.source, "builtin");

        let scheduled = skills.iter().find(|skill| skill.name == "scheduled-task").unwrap();
        assert_eq!(scheduled.source, SkillSource::Cron);
        assert_eq!(scheduled.relative_location, None);
        let scheduled_row = repo.find_by_name("scheduled-task").await.unwrap().unwrap();
        assert_eq!(scheduled_row.source, "cron");
    }

    #[tokio::test]
    async fn import_skills_imports_zip_package() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let zip_path = tmp.path().join("skills.zip");

        write_test_zip(
            &zip_path,
            &[
                (
                    "bundle/zip-one/SKILL.md",
                    "---\nname: zip-one\ndescription: First zipped skill\n---\nBody",
                ),
                ("bundle/zip-one/data.txt", "payload"),
                (
                    "bundle/zip-two/SKILL.md",
                    "---\nname: zip-two\ndescription: Second zipped skill\n---\nBody",
                ),
            ],
        );

        let outcome = import_skills(&paths, &zip_path).await.unwrap();
        assert_eq!(outcome.imported, vec!["zip-one", "zip-two"]);
        assert!(outcome.failed.is_empty());
        assert!(paths.user_skills_dir.join("zip-one").join(SKILL_MANIFEST_FILE).exists());
        assert!(paths.user_skills_dir.join("zip-one").join("data.txt").exists());
        assert!(!paths.user_skills_dir.join("zip-one").is_symlink());
        assert!(!paths.user_skills_dir.join(".import-tmp").join("skills.zip").exists());
    }

    #[tokio::test]
    async fn import_skills_rejects_zip_slip_entries() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let zip_path = tmp.path().join("evil.zip");

        write_test_zip(&zip_path, &[("../escape.txt", "outside")]);

        let result = import_skills(&paths, &zip_path).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn import_skill_rejects_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Create a skill whose frontmatter name contains path traversal
        let source_dir = tmp.path().join("evil-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: ../../../etc/evil\ndescription: Malicious skill\n---\nBody",
        )
        .unwrap();

        let result = import_skill(&paths, &source_dir).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn import_skills_rejects_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("evil-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: ../../escape\ndescription: Malicious skill\n---\nBody",
        )
        .unwrap();

        let result = import_skills(&paths, &source_dir).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn import_skills_rejects_invalid_yaml_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("invalid-frontmatter");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: invalid-frontmatter\ndescription: Download video: supports batch URLs\n---\nBody",
        )
        .unwrap();

        let result = import_skills(&paths, &source_dir).await;
        assert!(matches!(result, Err(ExtensionError::SkillInvalidFrontmatter(_))));
        assert!(!paths.user_skills_dir.join("invalid-frontmatter").exists());
    }

    #[tokio::test]
    async fn delete_custom_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        create_skill_in_dir(&paths.user_skills_dir, "to-delete", "Will be deleted");

        delete_skill(&paths, "to-delete").await.unwrap();
        assert!(!paths.user_skills_dir.join("to-delete").exists());
    }

    #[tokio::test]
    async fn delete_custom_skill_with_repo_soft_deletes_without_removing_files() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let repo = make_test_skill_repo().await;

        create_skill_in_dir(tmp.path(), "historical", "Used by an old conversation");
        import_skill_with_repo(&paths, &repo, &tmp.path().join("historical"))
            .await
            .unwrap();

        delete_skill_with_repo(&paths, &repo, "historical").await.unwrap();

        assert!(
            paths
                .user_skills_dir
                .join("historical")
                .join(SKILL_MANIFEST_FILE)
                .exists()
        );

        let listed = list_available_skills_with_repo(&paths, &repo).await.unwrap();
        assert!(!listed.iter().any(|skill| skill.name == "historical"));

        let resolved =
            materialize_skills_for_agent_with_repo(&paths, &repo, "old-conversation", &["historical".to_owned()])
                .await
                .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_path, paths.user_skills_dir.join("historical"));
    }

    #[tokio::test]
    async fn delete_builtin_skill_rejected() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();

        create_skill_in_dir(&builtin_dir, "protected", "Built-in skill");

        let result = delete_skill(&paths, "protected").await;
        assert!(matches!(result, Err(ExtensionError::BuiltinSkillDeletion(_))));
    }

    #[tokio::test]
    async fn delete_nonexistent_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let result = delete_skill(&paths, "ghost").await;
        assert!(matches!(result, Err(ExtensionError::SkillNotFound(_))));
    }

    #[tokio::test]
    async fn delete_skill_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let result = delete_skill(&paths, "../etc").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    // -----------------------------------------------------------------------
    // Scanning
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn scan_for_skills_finds_valid() {
        let tmp = TempDir::new().unwrap();
        create_skill_in_dir(tmp.path(), "skill-a", "First skill");
        create_skill_in_dir(tmp.path(), "skill-b", "Second skill");

        // Create a dir without SKILL.md (should be ignored)
        std::fs::create_dir_all(tmp.path().join("not-a-skill")).unwrap();

        let skills = scan_for_skills(tmp.path()).await.unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "skill-a");
        assert_eq!(skills[1].name, "skill-b");
    }

    #[tokio::test]
    async fn scan_for_skills_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let skills = scan_for_skills(tmp.path()).await.unwrap();
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn scan_for_skills_nonexistent_dir() {
        let skills = scan_for_skills(Path::new("/nonexistent/path")).await.unwrap();
        assert!(skills.is_empty());
    }

    // -----------------------------------------------------------------------
    // Export
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn export_skill_creates_symlink() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: Test\n---\nBody",
        )
        .unwrap();

        let target_dir = tmp.path().join("exports");
        export_skill_with_symlink(&source_dir, &target_dir).await.unwrap();

        let link = target_dir.join("my-skill");
        assert!(link.is_symlink());
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_test_paths(base: &Path) -> SkillPaths {
        // Hand out an empty on-disk builtin-skills dir. Tests that need
        // specific fixtures seed it via `create_skill_in_dir`; tests
        // that want the full real corpus use `make_embedded_paths`.
        SkillPaths {
            data_dir: base.to_path_buf(),
            user_skills_dir: base.join(SKILLS_DIR_NAME),
            cron_skills_dir: base.join(CRON_SKILLS_DIR_NAME),
            builtin_skills_dir: base.join(crate::constants::BUILTIN_SKILLS_DIR_NAME),
            builtin_rules_dir: base.join(BUILTIN_RULES_DIR_NAME),
            assistant_rules_dir: base.join(ASSISTANT_RULES_DIR_NAME),
            assistant_skills_dir: base.join(ASSISTANT_SKILLS_DIR_NAME),
        }
    }

    /// Return `SkillPaths` pre-populated with the real embedded builtin
    /// skills corpus materialized to disk. Use this for tests that
    /// previously relied on the embedded-corpus fallback.
    async fn make_embedded_paths(base: &Path) -> SkillPaths {
        crate::startup_materialize::materialize_embedded_builtin_skills(base, &BUILTIN_SKILLS, "test-version")
            .await
            .expect("failed to materialize embedded corpus for test");
        make_test_paths(base)
    }

    /// Return a `SkillPaths` rooted at `base` with an on-disk
    /// `builtin_skills_dir`, so tests can seed fixtures in that dir.
    fn make_disk_builtin_paths(base: &Path) -> SkillPaths {
        make_test_paths(base)
    }

    fn disk_builtin_dir(paths: &SkillPaths) -> &Path {
        &paths.builtin_skills_dir
    }

    async fn make_test_skill_repo() -> SqliteSkillRepository {
        let db = init_database_memory().await.unwrap();
        SqliteSkillRepository::new(db.pool().clone())
    }

    fn create_skill_in_dir(base: &Path, name: &str, description: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_MANIFEST_FILE),
            format!("---\nname: {name}\ndescription: {description}\n---\nBody content for {name}."),
        )
        .unwrap();
    }

    fn has_import_staging_dirs(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .any(|entry| entry.file_name().to_string_lossy().starts_with(IMPORT_STAGING_PREFIX))
    }

    fn create_resolved_test_skill(source_root: &Path, name: &str) -> ResolvedAgentSkill {
        let source_path = source_root.join(name);
        std::fs::create_dir_all(&source_path).unwrap();
        std::fs::write(
            source_path.join(SKILL_MANIFEST_FILE),
            format!("---\nname: {name}\ndescription: test\n---\nbody"),
        )
        .unwrap();
        ResolvedAgentSkill {
            name: name.to_owned(),
            source_path,
        }
    }

    fn write_test_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        for (name, content) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }

        zip.finish().unwrap();
    }

    // -----------------------------------------------------------------------
    // Embedded corpus
    // -----------------------------------------------------------------------

    #[test]
    fn embedded_builtin_skill_frontmatter_is_valid_yaml() {
        let mut checked = 0;
        let mut failures = Vec::new();

        assert_embedded_skill_frontmatter(&BUILTIN_SKILLS, &mut checked, &mut failures);

        assert!(
            checked >= 20,
            "expected builtin skill corpus to contain many SKILL.md files, got {checked}"
        );
        assert!(
            failures.is_empty(),
            "invalid embedded builtin SKILL.md frontmatter:\n{}",
            failures.join("\n")
        );
    }

    fn assert_embedded_skill_frontmatter(dir: &Dir<'static>, checked: &mut usize, failures: &mut Vec<String>) {
        for file in dir.files() {
            if file.path().file_name().and_then(|name| name.to_str()) != Some(SKILL_MANIFEST_FILE) {
                continue;
            }

            *checked += 1;
            let path = file.path().display();
            let content = match std::str::from_utf8(file.contents()) {
                Ok(content) => content,
                Err(err) => {
                    failures.push(format!("{path}: not UTF-8: {err}"));
                    continue;
                }
            };

            if parse_frontmatter_fields(content).is_none() {
                failures.push(format!("{path}: invalid YAML frontmatter or missing description"));
            }
        }

        for subdir in dir.dirs() {
            assert_embedded_skill_frontmatter(subdir, checked, failures);
        }
    }

    #[tokio::test]
    async fn embedded_lists_auto_inject_from_corpus() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let autos = list_auto_inject_skills_from_disk(&paths).await.unwrap();
        assert!(autos.len() >= 3, "expected ≥3 auto-inject entries, got {}", autos.len());
        for item in &autos {
            assert!(
                item.location.starts_with("auto-inject/"),
                "location must start with auto-inject/, got {}",
                item.location
            );
            assert!(item.location.ends_with("/SKILL.md"));
            assert!(!item.description.is_empty());
        }
    }

    #[tokio::test]
    async fn embedded_reads_builtin_skill_content() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let content = read_builtin_skill(&paths, "auto-inject/cron/SKILL.md").await.unwrap();
        assert!(!content.is_empty(), "embedded cron SKILL.md is empty");
        assert!(
            content.trim_start().starts_with("---"),
            "expected frontmatter, got: {}",
            content.chars().take(80).collect::<String>()
        );
    }

    #[tokio::test]
    async fn embedded_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let result = read_builtin_skill(&paths, "../etc/passwd").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));

        let result = read_builtin_skill(&paths, "auto-inject/../../secret").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn embedded_handles_missing_file() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let content = read_builtin_skill(&paths, "nonexistent/SKILL.md").await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn disk_override_reads_from_disk_not_embedded() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();
        let auto_dir = builtin_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
        create_skill_in_dir(&auto_dir, "fixture-only", "Fixture-only skill");

        let autos = list_auto_inject_skills_from_disk(&paths).await.unwrap();
        let names: Vec<&str> = autos.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"fixture-only"),
            "disk override should reflect seeded skill; got {names:?}"
        );
        // Embedded skills (e.g. `cron`) must NOT leak into the disk view.
        assert!(
            !names.contains(&"cron"),
            "disk override must not include embedded skills"
        );
    }

    #[tokio::test]
    async fn list_skills_builtin_has_relative_location_from_embedded() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let skills = list_available_skills(&paths).await.unwrap();
        let builtins: Vec<_> = skills.iter().filter(|s| s.source == SkillSource::Builtin).collect();
        assert!(!builtins.is_empty(), "no builtin skills listed");
        for s in &builtins {
            let rel = s
                .relative_location
                .as_deref()
                .expect("builtin must have relative_location");
            assert!(
                rel.ends_with("/SKILL.md"),
                "relative_location must end in /SKILL.md, got {rel}"
            );
            assert!(
                s.location.contains(crate::constants::BUILTIN_SKILLS_DIR_NAME),
                "builtin location must live under the view dir, got {}",
                s.location
            );
            // Lazy materialization wrote SKILL.md to disk.
            assert!(
                std::path::Path::new(&s.location).exists(),
                "materialized view missing: {}",
                s.location
            );
        }
    }

    // -----------------------------------------------------------------------
    // Materialize (symlink contract)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn materialize_empty_list_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let list = materialize_skills_for_agent(&paths, "conv-empty", &[]).await.unwrap();
        assert!(list.is_empty());
        // No per-conversation dir should be created.
        assert!(!paths.data_dir.join("agent-skills").exists());
        assert!(!paths.data_dir.join("conversations").exists());
    }

    #[tokio::test]
    async fn materialize_resolves_auto_inject_skill_by_name() {
        // Auto-inject skills are resolved only when the caller names
        // them explicitly (see `ConversationService::create` snapshot).
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(&paths, "conv-named", &["cron".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "cron");
        // source_path points at the real on-disk auto-inject directory.
        let expected = paths.builtin_skills_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR).join("cron");
        assert_eq!(resolved[0].source_path, expected);
        assert!(resolved[0].source_path.is_dir());
        assert!(resolved[0].source_path.join(SKILL_MANIFEST_FILE).exists());
    }

    #[tokio::test]
    async fn materialize_resolves_opt_in_top_level_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(&paths, "conv-opt", &["mermaid".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "mermaid");
        let expected = paths.builtin_skills_dir.join("mermaid");
        assert_eq!(resolved[0].source_path, expected);
    }

    #[tokio::test]
    async fn materialize_resolves_user_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;
        create_skill_in_dir(&paths.user_skills_dir, "my-custom", "A user skill");

        let resolved = materialize_skills_for_agent(&paths, "conv-user", &["my-custom".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_path, paths.user_skills_dir.join("my-custom"));
    }

    #[tokio::test]
    async fn materialize_silently_skips_unknown_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(&paths, "conv-missing", &["no-such-skill".to_owned()])
            .await
            .unwrap();
        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn materialize_skips_invalid_names_but_keeps_valid_ones() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(
            &paths,
            "conv-mixed",
            &[
                "".to_owned(),
                "../evil".to_owned(),
                "foo/bar".to_owned(),
                "cron".to_owned(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "cron");
    }

    #[tokio::test]
    async fn materialize_returns_sorted_list_with_source_paths() {
        // Deterministic ordering — callers rely on it for stable symlink
        // layouts and for easier debugging / snapshot tests.
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(&paths, "conv-sorted", &["mermaid".to_owned(), "cron".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "cron");
        assert_eq!(resolved[1].name, "mermaid");
        for entry in &resolved {
            assert!(entry.source_path.is_absolute());
            assert!(entry.source_path.is_dir());
        }
    }

    #[tokio::test]
    async fn materialize_rejects_bad_conversation_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let err = materialize_skills_for_agent(&paths, "../evil", &[]).await.unwrap_err();
        assert!(matches!(err, ExtensionError::PathTraversal(_)));
    }

    #[tokio::test]
    async fn materialize_does_not_touch_disk_beyond_reads() {
        // Guardrail: the symlink contract forbids any per-conversation
        // directory on disk. Verify the function only reads the sources
        // and never writes.
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let _ = materialize_skills_for_agent(&paths, "conv-pure", &["cron".to_owned()])
            .await
            .unwrap();
        assert!(!paths.data_dir.join("agent-skills").exists());
        assert!(!paths.data_dir.join("conversations").exists());
    }

    // -----------------------------------------------------------------------
    // Windows symlink → copy_dir_recursive fallback
    // -----------------------------------------------------------------------

    /// When the platform symlink primitive fails (mirrors Windows
    /// `os error 1314 ERROR_PRIVILEGE_NOT_HELD`), `link_workspace_skills`
    /// must materialize the skill via `copy_dir_recursive` instead so the
    /// CLI agent can still discover it. Forced via `ForceFailureGuard`
    /// on Linux/macOS CI where symlinking would otherwise succeed.
    #[tokio::test]
    async fn link_workspace_skills_falls_back_to_copy_when_symlink_fails() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");

        // Seed a fake skill source directory with a SKILL.md and a
        // nested file so we can verify the copy is recursive.
        let skill_source = source_root.join("my-skill");
        std::fs::create_dir_all(skill_source.join("nested")).unwrap();
        std::fs::write(
            skill_source.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: test\n---\nbody",
        )
        .unwrap();
        std::fs::write(skill_source.join("nested").join("data.txt"), "payload").unwrap();

        let resolved = vec![ResolvedAgentSkill {
            name: "my-skill".to_owned(),
            source_path: skill_source.clone(),
        }];

        // Force the symlink primitive to fail for the duration of this
        // test, exercising the copy fallback branch.
        let _guard = test_overrides::ForceFailureGuard::new();

        let created = link_workspace_skills(&workspace, &[".claude/skills"], &resolved)
            .await
            .expect("link_workspace_skills should succeed via copy fallback");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        let target = workspace.join(".claude/skills").join("my-skill");
        assert!(target.exists(), "target directory must exist");
        // It must NOT be a symlink — fallback path uses copy_dir_recursive.
        let meta = tokio::fs::symlink_metadata(&target).await.unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "fallback must produce a real directory, not a symlink"
        );
        assert!(target.is_dir(), "target must be a directory");

        // Verify the contents were copied recursively.
        let manifest = std::fs::read_to_string(target.join(SKILL_MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("name: my-skill"));
        let nested = std::fs::read_to_string(target.join("nested").join("data.txt")).unwrap();
        assert_eq!(nested, "payload");
    }

    #[tokio::test]
    async fn link_workspace_skills_uses_existing_singular_skill_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");
        let existing_skill_dir = workspace.join(".claude").join("skill");
        std::fs::create_dir_all(&existing_skill_dir).unwrap();

        let resolved = vec![create_resolved_test_skill(&source_root, "my-skill")];

        let created = link_workspace_skills(&workspace, &[".claude/skills"], &resolved)
            .await
            .expect("link_workspace_skills should use existing singular skill dir");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        assert!(
            existing_skill_dir.join("my-skill").exists(),
            "existing singular skill dir should receive the skill"
        );
        assert!(
            !workspace.join(".claude").join("skills").exists(),
            "plural skills dir should not be created when singular skill dir already exists"
        );
    }

    #[tokio::test]
    async fn link_workspace_skills_creates_requested_dir_inside_existing_agent_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");
        std::fs::create_dir_all(workspace.join(".codex")).unwrap();

        let resolved = vec![create_resolved_test_skill(&source_root, "my-skill")];

        let created = link_workspace_skills(&workspace, &[".codex/skills"], &resolved)
            .await
            .expect("link_workspace_skills should create missing skills dir");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        assert!(
            workspace.join(".codex/skills/my-skill").is_dir(),
            "missing skills dir should be created under the existing agent dir"
        );
    }

    #[tokio::test]
    async fn link_workspace_skills_prefers_existing_plural_dir_over_singular_sibling() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");
        let plural_dir = workspace.join(".gemini").join("skills");
        let singular_dir = workspace.join(".gemini").join("skill");
        std::fs::create_dir_all(&plural_dir).unwrap();
        std::fs::create_dir_all(&singular_dir).unwrap();

        let resolved = vec![create_resolved_test_skill(&source_root, "my-skill")];

        let created = link_workspace_skills(&workspace, &[".gemini/skills"], &resolved)
            .await
            .expect("link_workspace_skills should prefer the requested existing dir");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        assert!(
            plural_dir.join("my-skill").is_dir(),
            "existing plural skills dir should receive the skill"
        );
        assert!(
            !singular_dir.join("my-skill").exists(),
            "singular sibling should remain untouched when requested dir exists"
        );
    }

    /// Windows-only: directory linking must go through an NTFS junction
    /// (created by the `junction` crate) rather than `symlink_dir`, so
    /// the link works for users without Developer Mode. We assert the
    /// resulting path is a reparse point (junction is reported as a
    /// symlink by `symlink_metadata().file_type().is_symlink()`) and
    /// that the source contents are reachable through the link.
    ///
    /// The test is skipped on non-Windows platforms.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn link_workspace_skills_uses_junction_on_windows() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");

        let skill_source = source_root.join("my-skill");
        std::fs::create_dir_all(skill_source.join("nested")).unwrap();
        std::fs::write(
            skill_source.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: test\n---\nbody",
        )
        .unwrap();
        std::fs::write(skill_source.join("nested").join("data.txt"), "payload").unwrap();

        let resolved = vec![ResolvedAgentSkill {
            name: "my-skill".to_owned(),
            source_path: skill_source.clone(),
        }];

        let created = link_workspace_skills(&workspace, &[".claude/skills"], &resolved)
            .await
            .expect("link_workspace_skills should succeed via junction");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        let target = workspace.join(".claude/skills").join("my-skill");
        assert!(target.exists(), "target path must exist");

        // Junctions are reparse points; `symlink_metadata` reports them
        // as symlinks on Windows. The directory copy fallback would
        // produce a real directory (is_symlink() == false).
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "Windows directory link must be a junction (reparse point), \
             not a copied directory"
        );

        // Reading through the link must surface the source contents.
        let manifest = std::fs::read_to_string(target.join(SKILL_MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("name: my-skill"));
        let nested = std::fs::read_to_string(target.join("nested").join("data.txt")).unwrap();
        assert_eq!(nested, "payload");
    }
}
