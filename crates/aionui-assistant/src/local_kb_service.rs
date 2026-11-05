//! Local-kb service (M9 E15). Default OFF. Not experience library / KnowHub.

use std::path::Path;
use std::sync::Arc;

use crate::error::AssistantError;
use aionui_api_types::{
    CreateLocalKbSourceRequest, LocalKbSearchHit, LocalKbSearchRequest, LocalKbSettingsResponse, LocalKbSourceResponse,
    UpdateLocalKbSettingsRequest,
};
use aionui_db::{
    CreateLocalKbDocumentParams, CreateLocalKbSourceParams, ILocalKbRepository, UpsertLocalKbSettingsParams,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const LOCAL_KB_PROVIDER_ID: &str = "local-kb";

const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIR_FILES: usize = 200;
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "json", "yaml", "yml", "toml", "csv", "log", "rs", "ts", "tsx", "js", "py",
];

pub struct LocalKbService {
    repo: Arc<dyn ILocalKbRepository>,
}

impl LocalKbService {
    pub fn new(repo: Arc<dyn ILocalKbRepository>) -> Self {
        Self { repo }
    }

    pub async fn get_settings(&self, user_id: &str) -> Result<LocalKbSettingsResponse, AssistantError> {
        match self.repo.get_settings(user_id).await.map_err(db_err)? {
            Some(row) => Ok(LocalKbSettingsResponse {
                enabled: row.enabled != 0,
                updated_at: row.updated_at,
            }),
            None => Ok(LocalKbSettingsResponse {
                enabled: false,
                updated_at: 0,
            }),
        }
    }

    pub async fn update_settings(
        &self,
        user_id: &str,
        req: UpdateLocalKbSettingsRequest,
    ) -> Result<LocalKbSettingsResponse, AssistantError> {
        let row = self
            .repo
            .upsert_settings(UpsertLocalKbSettingsParams {
                user_id,
                enabled: req.enabled,
            })
            .await
            .map_err(db_err)?;
        Ok(LocalKbSettingsResponse {
            enabled: row.enabled != 0,
            updated_at: row.updated_at,
        })
    }

    pub async fn list_sources(&self, user_id: &str) -> Result<Vec<LocalKbSourceResponse>, AssistantError> {
        let sources = self.repo.list_sources(user_id).await.map_err(db_err)?;
        let mut out = Vec::with_capacity(sources.len());
        for s in sources {
            let docs = self
                .repo
                .list_documents_for_source(user_id, &s.id)
                .await
                .map_err(db_err)?;
            out.push(LocalKbSourceResponse {
                id: s.id,
                kind: s.kind,
                label: s.label,
                origin_path: s.origin_path,
                document_count: docs.len() as i64,
                created_at: s.created_at,
                updated_at: s.updated_at,
            });
        }
        Ok(out)
    }

    pub async fn create_source(
        &self,
        user_id: &str,
        req: CreateLocalKbSourceRequest,
    ) -> Result<LocalKbSourceResponse, AssistantError> {
        let kind = req.kind.trim().to_ascii_lowercase();
        if !matches!(kind.as_str(), "text" | "file" | "directory") {
            return Err(AssistantError::BadRequest(
                "kind must be text, file, or directory".into(),
            ));
        }
        let label = req.label.trim();
        if label.is_empty() {
            return Err(AssistantError::BadRequest("label is required".into()));
        }

        let source_id = Uuid::now_v7().to_string();
        let origin = req.origin_path.as_deref().map(str::trim).filter(|s| !s.is_empty());

        match kind.as_str() {
            "text" => {
                let body = req.text.as_deref().unwrap_or("").trim();
                if body.is_empty() {
                    return Err(AssistantError::BadRequest("text is required for kind=text".into()));
                }
                if body.len() > MAX_TEXT_BYTES {
                    return Err(AssistantError::BadRequest("text exceeds 2 MiB limit".into()));
                }
                let source = self
                    .repo
                    .create_source(CreateLocalKbSourceParams {
                        id: &source_id,
                        user_id,
                        kind: "text",
                        label,
                        origin_path: None,
                    })
                    .await
                    .map_err(db_err)?;
                let title = req
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(label);
                self.insert_doc(user_id, &source.id, title, body, None).await?;
                Ok(LocalKbSourceResponse {
                    id: source.id,
                    kind: source.kind,
                    label: source.label,
                    origin_path: source.origin_path,
                    document_count: 1,
                    created_at: source.created_at,
                    updated_at: source.updated_at,
                })
            }
            "file" => {
                let path = origin.ok_or_else(|| AssistantError::BadRequest("origin_path required for file".into()))?;
                let content = read_text_file(Path::new(path))?;
                let source = self
                    .repo
                    .create_source(CreateLocalKbSourceParams {
                        id: &source_id,
                        user_id,
                        kind: "file",
                        label,
                        origin_path: Some(path),
                    })
                    .await
                    .map_err(db_err)?;
                let title = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or(label);
                self.insert_doc(user_id, &source.id, title, &content, Some(path))
                    .await?;
                Ok(LocalKbSourceResponse {
                    id: source.id,
                    kind: source.kind,
                    label: source.label,
                    origin_path: source.origin_path,
                    document_count: 1,
                    created_at: source.created_at,
                    updated_at: source.updated_at,
                })
            }
            "directory" => {
                let path =
                    origin.ok_or_else(|| AssistantError::BadRequest("origin_path required for directory".into()))?;
                let files = collect_dir_texts(Path::new(path))?;
                if files.is_empty() {
                    return Err(AssistantError::BadRequest(
                        "directory has no supported text files".into(),
                    ));
                }
                let source = self
                    .repo
                    .create_source(CreateLocalKbSourceParams {
                        id: &source_id,
                        user_id,
                        kind: "directory",
                        label,
                        origin_path: Some(path),
                    })
                    .await
                    .map_err(db_err)?;
                for (rel, body) in &files {
                    self.insert_doc(user_id, &source.id, rel, body, Some(rel)).await?;
                }
                Ok(LocalKbSourceResponse {
                    id: source.id,
                    kind: source.kind,
                    label: source.label,
                    origin_path: source.origin_path,
                    document_count: files.len() as i64,
                    created_at: source.created_at,
                    updated_at: source.updated_at,
                })
            }
            _ => unreachable!(),
        }
    }

    pub async fn delete_source(&self, user_id: &str, source_id: &str) -> Result<(), AssistantError> {
        let deleted = self.repo.delete_source(user_id, source_id).await.map_err(db_err)?;
        if !deleted {
            return Err(AssistantError::NotFound("local-kb source not found".into()));
        }
        Ok(())
    }

    pub async fn search(
        &self,
        user_id: &str,
        req: LocalKbSearchRequest,
    ) -> Result<Vec<LocalKbSearchHit>, AssistantError> {
        let settings = self.get_settings(user_id).await?;
        if !settings.enabled {
            return Err(AssistantError::BadRequest(
                "local-kb is disabled (default OFF); enable it before searching".into(),
            ));
        }
        let q = req.query.trim();
        if q.is_empty() {
            return Err(AssistantError::BadRequest("query is required".into()));
        }
        let limit = req.limit.clamp(1, 100) as i64;
        let rows = self.repo.search_documents(user_id, q, limit).await.map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|d| {
                let snippet = make_snippet(&d.body_text, q);
                LocalKbSearchHit {
                    document_id: d.id,
                    source_id: d.source_id,
                    title: d.title,
                    snippet,
                    content_hash: d.content_hash,
                    provider: LOCAL_KB_PROVIDER_ID.to_string(),
                }
            })
            .collect())
    }

    async fn insert_doc(
        &self,
        user_id: &str,
        source_id: &str,
        title: &str,
        body: &str,
        relative_path: Option<&str>,
    ) -> Result<(), AssistantError> {
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
        let id = Uuid::now_v7().to_string();
        self.repo
            .create_document(CreateLocalKbDocumentParams {
                id: &id,
                source_id,
                user_id,
                title,
                body_text: body,
                content_hash: &hash,
                relative_path,
            })
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

fn db_err(err: aionui_db::DbError) -> AssistantError {
    AssistantError::from(err)
}

fn read_text_file(path: &Path) -> Result<String, AssistantError> {
    if !path.is_file() {
        return Err(AssistantError::BadRequest(format!("not a file: {}", path.display())));
    }
    let meta = std::fs::metadata(path).map_err(|e| AssistantError::BadRequest(e.to_string()))?;
    if meta.len() as usize > MAX_TEXT_BYTES {
        return Err(AssistantError::BadRequest("file exceeds 2 MiB limit".into()));
    }
    let bytes = std::fs::read(path).map_err(|e| AssistantError::BadRequest(e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| AssistantError::BadRequest("file is not valid UTF-8 text".into()))
}

fn collect_dir_texts(root: &Path) -> Result<Vec<(String, String)>, AssistantError> {
    if !root.is_dir() {
        return Err(AssistantError::BadRequest(format!(
            "not a directory: {}",
            root.display()
        )));
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| AssistantError::BadRequest(e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| AssistantError::BadRequest(e.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !TEXT_EXTENSIONS.iter().any(|e| *e == ext) {
                continue;
            }
            match read_text_file(&path) {
                Ok(body) => {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, body));
                }
                Err(_) => continue,
            }
            if out.len() >= MAX_DIR_FILES {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

fn make_snippet(body: &str, query: &str) -> String {
    let lower = body.to_lowercase();
    let needle = query.to_lowercase();
    let byte_start = lower.find(&needle).unwrap_or(0);
    let char_start = body[..byte_start].chars().count();
    let begin = char_start.saturating_sub(60);
    let take = needle.chars().count() + 140;
    let mut chars = body.chars().skip(begin).take(take);
    let mut s: String = chars.by_ref().collect();
    if begin > 0 {
        s.insert(0, '…');
    }
    if chars.next().is_some() || begin + take < body.chars().count() {
        // if truncated from end
        if body.chars().count() > begin + take {
            s.push('…');
        }
    }
    s
}
