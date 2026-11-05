//! Local-kb ContextProvider (M9 E15).
//!
//! Brand: CSBU WorkMate. Default OFF. Parallel to KnowHub — never named KnowHub.
//! Does not read skill experience library tables. Callers must not auto-inject
//! this provider into daily chat; only explicit context resolution may select it.

use std::sync::Arc;

use aionui_api_types::{
    ContextAuthorization, ContextAuthorizationRequest, ContextDiscoveryRequest, ContextDocument, ContextFetchRequest,
    ContextHit, ContextProvenance, ContextProviderCapabilities, ContextProviderIdentity, ContextQuery,
    ContextResourceRef, ContextScope, ContextSource,
};
use aionui_db::{ILocalKbRepository, LocalKbDocumentRow};
use async_trait::async_trait;

use super::context_provider::{ContextProvider, ContextProviderError};

pub const LOCAL_KB_PROVIDER_ID: &str = "local-kb";
pub const LOCAL_KB_INSTANCE_ID: &str = "local-kb-default";

#[derive(Clone)]
pub struct LocalKbContextProvider {
    user_id: String,
    repo: Arc<dyn ILocalKbRepository>,
}

impl LocalKbContextProvider {
    pub fn new(user_id: impl Into<String>, repo: Arc<dyn ILocalKbRepository>) -> Self {
        Self {
            user_id: user_id.into(),
            repo,
        }
    }

    async fn require_enabled(&self) -> Result<(), ContextProviderError> {
        let enabled = match self.repo.get_settings(&self.user_id).await {
            Ok(Some(row)) => row.enabled != 0,
            Ok(None) => false,
            Err(err) => return Err(ContextProviderError::Provider(err.to_string())),
        };
        if !enabled {
            return Err(ContextProviderError::Unavailable(
                "local-kb is disabled (default OFF); enable it in Settings before use".into(),
            ));
        }
        Ok(())
    }
}

fn snippet_for(doc: &LocalKbDocumentRow, query: &str, max_chars: usize) -> String {
    let body = doc.body_text.as_str();
    let lower = body.to_lowercase();
    let q = query.trim().to_lowercase();
    let start = lower.find(&q).unwrap_or(0);
    let begin = start.saturating_sub(80);
    let end = (start + q.len() + 160).min(body.len());
    let mut snip = body[begin..end].to_string();
    if begin > 0 {
        snip.insert(0, '…');
    }
    if end < body.len() {
        snip.push('…');
    }
    if snip.chars().count() > max_chars {
        snip = snip.chars().take(max_chars).collect();
        snip.push('…');
    }
    snip
}

fn hit_from_doc(doc: &LocalKbDocumentRow, query: &str) -> ContextHit {
    let retrieved_at = doc.updated_at;
    ContextHit {
        provider: LOCAL_KB_PROVIDER_ID.to_string(),
        source_id: doc.id.clone(),
        title: doc.title.clone(),
        snippet: snippet_for(doc, query, 500),
        score: None,
        permission_scope: format!("user:{}", doc.user_id),
        version_or_updated_at: Some(doc.updated_at.to_string()),
        retrieved_at,
        content_hash: Some(doc.content_hash.clone()),
        provenance: ContextProvenance {
            provider: LOCAL_KB_PROVIDER_ID.to_string(),
            source_id: doc.id.clone(),
            tenant: None,
            space: Some(ContextResourceRef {
                id: doc.source_id.clone(),
                name: Some("local-kb-source".into()),
            }),
            knowledge_base: Some(ContextResourceRef {
                id: LOCAL_KB_INSTANCE_ID.to_string(),
                name: Some("Local knowledge base".into()),
            }),
            document: Some(ContextResourceRef {
                id: doc.id.clone(),
                name: Some(doc.title.clone()),
            }),
            provider_data: Default::default(),
        },
    }
}

#[async_trait]
impl ContextProvider for LocalKbContextProvider {
    fn identity(&self) -> ContextProviderIdentity {
        ContextProviderIdentity {
            provider: LOCAL_KB_PROVIDER_ID.to_string(),
            instance_id: LOCAL_KB_INSTANCE_ID.to_string(),
            display_name: "Local knowledge base".to_string(),
        }
    }

    fn capabilities(&self) -> ContextProviderCapabilities {
        ContextProviderCapabilities {
            single_scope: true,
            multi_scope: false,
            all_accessible: true,
            provenance: true,
            permission_aware: true,
            freshness: true,
            content_fetch: true,
        }
    }

    async fn discover(&self, _request: &ContextDiscoveryRequest) -> Result<Vec<ContextSource>, ContextProviderError> {
        self.require_enabled().await?;
        let sources = self
            .repo
            .list_sources(&self.user_id)
            .await
            .map_err(|e| ContextProviderError::Provider(e.to_string()))?;
        Ok(sources
            .into_iter()
            .map(|s| ContextSource {
                provider: LOCAL_KB_PROVIDER_ID.to_string(),
                source_id: s.id,
                kind: "local_kb_source".into(),
                display_name: s.label,
                parent_source_id: None,
                metadata: Default::default(),
            })
            .collect())
    }

    async fn search(&self, query: &ContextQuery) -> Result<Vec<ContextHit>, ContextProviderError> {
        self.require_enabled().await?;
        super::context_provider::validate_context_query(self.capabilities(), query)?;
        let limit = query.limit.min(100) as i64;
        let rows = self
            .repo
            .search_documents(&self.user_id, &query.query, limit)
            .await
            .map_err(|e| ContextProviderError::Provider(e.to_string()))?;

        // Optional single-scope filter by source id.
        let rows = match &query.scope {
            ContextScope::SingleScope { scope_id } => rows
                .into_iter()
                .filter(|d| d.source_id == *scope_id)
                .collect::<Vec<_>>(),
            ContextScope::AllAccessible => rows,
            ContextScope::MultiScope { .. } => {
                return Err(ContextProviderError::UnsupportedCapability("multi_scope"));
            }
        };

        Ok(rows.iter().map(|d| hit_from_doc(d, &query.query)).collect())
    }

    async fn fetch(&self, request: &ContextFetchRequest) -> Result<ContextDocument, ContextProviderError> {
        self.require_enabled().await?;
        let doc = self
            .repo
            .get_document(&self.user_id, &request.source_id)
            .await
            .map_err(|e| ContextProviderError::Provider(e.to_string()))?
            .ok_or_else(|| ContextProviderError::Unavailable("document not found".into()))?;
        Ok(ContextDocument {
            provider: LOCAL_KB_PROVIDER_ID.to_string(),
            source_id: doc.id.clone(),
            content: doc.body_text,
            content_hash: Some(doc.content_hash),
            version_or_updated_at: Some(doc.updated_at.to_string()),
            retrieved_at: aionui_common::now_ms(),
            provenance: ContextProvenance {
                provider: LOCAL_KB_PROVIDER_ID.to_string(),
                source_id: doc.id.clone(),
                tenant: None,
                space: Some(ContextResourceRef {
                    id: doc.source_id.clone(),
                    name: Some("local-kb-source".into()),
                }),
                knowledge_base: Some(ContextResourceRef {
                    id: LOCAL_KB_INSTANCE_ID.to_string(),
                    name: Some("Local knowledge base".into()),
                }),
                document: Some(ContextResourceRef {
                    id: doc.id.clone(),
                    name: Some(doc.title.clone()),
                }),
                provider_data: Default::default(),
            },
        })
    }

    async fn authorize(
        &self,
        request: &ContextAuthorizationRequest,
    ) -> Result<ContextAuthorization, ContextProviderError> {
        self.require_enabled().await?;
        let allowed = self
            .repo
            .get_document(&self.user_id, &request.source_id)
            .await
            .map_err(|e| ContextProviderError::Provider(e.to_string()))?
            .is_some();
        Ok(ContextAuthorization {
            allowed,
            permission_scope: Some(format!("user:{}", self.user_id)),
            rule_id: "local-kb-owner".into(),
            reason: if allowed {
                "document owned by caller".into()
            } else {
                "document not found for caller".into()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::ContextPurpose;
    use aionui_db::{
        CreateLocalKbDocumentParams, CreateLocalKbSourceParams, SqliteLocalKbRepository, UpsertLocalKbSettingsParams,
        init_database_memory,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn disabled_by_default_and_identity_is_local_kb() {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteLocalKbRepository::new(db.pool().clone()));
        let provider = LocalKbContextProvider::new("system_default_user", repo.clone());
        assert_eq!(provider.identity().provider, LOCAL_KB_PROVIDER_ID);
        let err = provider
            .search(&ContextQuery {
                task_id: "t1".into(),
                project_id: None,
                query: "anything".into(),
                scope: ContextScope::AllAccessible,
                limit: 5,
                filters: Default::default(),
                purpose: ContextPurpose::Planning,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ContextProviderError::Unavailable(_)));
    }

    #[tokio::test]
    async fn enabled_search_returns_hits() {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteLocalKbRepository::new(db.pool().clone()));
        let user = "system_default_user";
        repo.upsert_settings(UpsertLocalKbSettingsParams {
            user_id: user,
            enabled: true,
        })
        .await
        .unwrap();
        repo.create_source(CreateLocalKbSourceParams {
            id: "s1",
            user_id: user,
            kind: "text",
            label: "notes",
            origin_path: None,
        })
        .await
        .unwrap();
        repo.create_document(CreateLocalKbDocumentParams {
            id: "d1",
            source_id: "s1",
            user_id: user,
            title: "Runbook",
            body_text: "CSBU WorkMate local kb sample",
            content_hash: "h1",
            relative_path: None,
        })
        .await
        .unwrap();

        let provider = LocalKbContextProvider::new(user, repo);
        let hits = provider
            .search(&ContextQuery {
                task_id: "t1".into(),
                project_id: None,
                query: "WorkMate".into(),
                scope: ContextScope::AllAccessible,
                limit: 10,
                filters: Default::default(),
                purpose: ContextPurpose::Planning,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, LOCAL_KB_PROVIDER_ID);
        assert!(!hits[0].snippet.to_lowercase().contains("knowhub"));
    }
}
