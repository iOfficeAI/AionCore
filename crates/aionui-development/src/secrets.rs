use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use aionui_common::{decrypt_string, encrypt_string, now_ms};
use aionui_db::models::{DevelopmentAuditEventRow, DevelopmentSecretGrantRow, DevelopmentSecretRow};
use aionui_db::{IDevelopmentOperationsRepository, IProjectRepository};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::Zeroize;

use crate::DevelopmentError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretCreateInput {
    pub name: String,
    pub value: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMetadata {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub status: String,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretGrantInput {
    pub secret_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub environment_key: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretGrantMetadata {
    pub id: String,
    pub secret_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub environment_key: String,
    pub status: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretAccessContext {
    pub project_id: String,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretReferenceRequest {
    pub secret_id: String,
    pub environment_key: String,
}

pub struct MaterializedSecretEnvironment {
    values: BTreeMap<String, String>,
}

impl MaterializedSecretEnvironment {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub(crate) fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}

impl fmt::Debug for MaterializedSecretEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedSecretEnvironment")
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Drop for MaterializedSecretEnvironment {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
        self.values.clear();
    }
}

#[derive(Clone)]
pub struct SecretService {
    operations: Arc<dyn IDevelopmentOperationsRepository>,
    projects: Arc<dyn IProjectRepository>,
    encryption_key: Arc<[u8; 32]>,
}

impl SecretService {
    pub fn new(
        operations: Arc<dyn IDevelopmentOperationsRepository>,
        projects: Arc<dyn IProjectRepository>,
        encryption_key: Arc<[u8; 32]>,
    ) -> Self {
        Self {
            operations,
            projects,
            encryption_key,
        }
    }

    pub async fn create(
        &self,
        user_id: &str,
        project_id: &str,
        mut input: SecretCreateInput,
    ) -> Result<SecretMetadata, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        input.name = input.name.trim().to_owned();
        if input.name.is_empty() || input.name.len() > 120 || input.value.is_empty() {
            input.value.zeroize();
            return Err(DevelopmentError::BadRequest(
                "Secret name and value are required".into(),
            ));
        }
        let encrypted_value = encrypt_string(&input.value, self.encryption_key.as_ref())
            .map_err(|_| DevelopmentError::Internal("Secret encryption failed".into()))?;
        input.value.zeroize();
        let now = now_ms();
        let row = DevelopmentSecretRow {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: user_id.into(),
            project_id: project_id.into(),
            name: input.name,
            encrypted_value,
            key_version: "application-v1".into(),
            status: "active".into(),
            expires_at: input.expires_at,
            revoked_at: None,
            created_at: now,
            updated_at: now,
        };
        self.operations.insert_secret(&row).await?;
        self.append_audit(
            user_id,
            "secret.create",
            "secret",
            &row.id,
            &row.project_id,
            json!({"name": &row.name, "expires_at": row.expires_at, "key_version": &row.key_version}),
        )
        .await?;
        Ok(metadata(&row))
    }

    pub async fn list(&self, user_id: &str, project_id: &str) -> Result<Vec<SecretMetadata>, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        Ok(self
            .operations
            .list_secrets(user_id, project_id)
            .await?
            .iter()
            .map(metadata)
            .collect())
    }

    pub async fn grant(&self, user_id: &str, input: SecretGrantInput) -> Result<SecretGrantMetadata, DevelopmentError> {
        let secret = self.require_secret(user_id, &input.secret_id).await?;
        if !matches!(input.scope_type.as_str(), "project" | "run" | "agent")
            || input.scope_id.trim().is_empty()
            || !valid_environment_key(&input.environment_key)
        {
            return Err(DevelopmentError::BadRequest("invalid Secret grant".into()));
        }
        if input.scope_type == "project" && input.scope_id != secret.project_id {
            return Err(DevelopmentError::BadRequest(
                "project Secret grant must target its owning project".into(),
            ));
        }
        let now = now_ms();
        let row = DevelopmentSecretGrantRow {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: user_id.into(),
            project_id: secret.project_id,
            secret_id: secret.id,
            scope_type: input.scope_type,
            scope_id: input.scope_id,
            environment_key: input.environment_key,
            status: "active".into(),
            expires_at: input.expires_at,
            revoked_at: None,
            created_at: now,
            updated_at: now,
        };
        self.operations.upsert_secret_grant(&row).await?;
        self.append_audit(
            user_id,
            "secret.grant",
            "secret_grant",
            &row.id,
            &row.project_id,
            json!({
                "secret_id": &row.secret_id,
                "scope_type": &row.scope_type,
                "scope_id": &row.scope_id,
                "environment_key": &row.environment_key,
                "expires_at": row.expires_at,
            }),
        )
        .await?;
        Ok(grant_metadata(&row))
    }

    pub async fn revoke(&self, user_id: &str, secret_id: &str) -> Result<(), DevelopmentError> {
        let secret = self.require_secret(user_id, secret_id).await?;
        if !self.operations.revoke_secret(user_id, secret_id, now_ms()).await? {
            return Err(DevelopmentError::NotFound("Secret".into()));
        }
        self.append_audit(
            user_id,
            "secret.revoke",
            "secret",
            secret_id,
            &secret.project_id,
            json!({"secret_id": secret_id}),
        )
        .await?;
        Ok(())
    }

    pub async fn materialize(
        &self,
        user_id: &str,
        context: &SecretAccessContext,
        requests: &[SecretReferenceRequest],
    ) -> Result<MaterializedSecretEnvironment, DevelopmentError> {
        self.require_project(user_id, &context.project_id).await?;
        let now = now_ms();
        let mut values = BTreeMap::new();
        for request in requests {
            if !valid_environment_key(&request.environment_key) || values.contains_key(&request.environment_key) {
                return Err(DevelopmentError::BadRequest(
                    "invalid or duplicate Secret environment key".into(),
                ));
            }
            let secret = self.require_secret(user_id, &request.secret_id).await?;
            if secret.project_id != context.project_id
                || secret.status != "active"
                || secret.expires_at.is_some_and(|expires| expires <= now)
            {
                return Err(DevelopmentError::NotFound("active Secret grant".into()));
            }
            let grants = self.operations.list_secret_grants(user_id, &secret.id).await?;
            let authorized = grants.iter().any(|grant| {
                grant.status == "active"
                    && grant.environment_key == request.environment_key
                    && grant.expires_at.is_none_or(|expires| expires > now)
                    && match grant.scope_type.as_str() {
                        "project" => grant.scope_id == context.project_id,
                        "run" => context.run_id.as_deref() == Some(grant.scope_id.as_str()),
                        "agent" => context.agent_id.as_deref() == Some(grant.scope_id.as_str()),
                        _ => false,
                    }
            });
            if !authorized {
                return Err(DevelopmentError::NotFound("active Secret grant".into()));
            }
            let plaintext = decrypt_string(&secret.encrypted_value, self.encryption_key.as_ref())
                .map_err(|_| DevelopmentError::Internal("Secret decryption failed".into()))?;
            values.insert(request.environment_key.clone(), plaintext);
        }
        self.append_audit(
            user_id,
            "secret.materialize",
            "secret_access",
            context
                .run_id
                .as_deref()
                .or(context.agent_id.as_deref())
                .unwrap_or(&context.project_id),
            &context.project_id,
            json!({
                "run_id": context.run_id.as_deref(),
                "agent_id": context.agent_id.as_deref(),
                "secret_ids": requests.iter().map(|request| &request.secret_id).collect::<Vec<_>>(),
                "environment_keys": requests.iter().map(|request| &request.environment_key).collect::<Vec<_>>(),
            }),
        )
        .await?;
        Ok(MaterializedSecretEnvironment { values })
    }

    async fn require_project(&self, user_id: &str, project_id: &str) -> Result<(), DevelopmentError> {
        if self.projects.get_for_user(project_id, user_id).await?.is_none() {
            return Err(DevelopmentError::NotFound("Project".into()));
        }
        Ok(())
    }

    async fn require_secret(&self, user_id: &str, secret_id: &str) -> Result<DevelopmentSecretRow, DevelopmentError> {
        self.operations
            .get_secret(user_id, secret_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound("Secret".into()))
    }

    async fn append_audit(
        &self,
        user_id: &str,
        action: &str,
        target_type: &str,
        target_id: &str,
        project_id: &str,
        payload: Value,
    ) -> Result<(), DevelopmentError> {
        let payload = serde_json::to_string(&payload).map_err(|error| DevelopmentError::Internal(error.to_string()))?;
        self.operations
            .append_audit(&DevelopmentAuditEventRow {
                id: uuid::Uuid::now_v7().to_string(),
                user_id: user_id.into(),
                actor_type: "user".into(),
                actor_id: user_id.into(),
                action: action.into(),
                target_type: target_type.into(),
                target_id: target_id.into(),
                project_id: project_id.into(),
                run_id: None,
                task_id: None,
                result: "success".into(),
                redacted_payload_json: redact_text(&payload, &[]),
                created_at: now_ms(),
            })
            .await?;
        Ok(())
    }
}

fn metadata(row: &DevelopmentSecretRow) -> SecretMetadata {
    SecretMetadata {
        id: row.id.clone(),
        project_id: row.project_id.clone(),
        name: row.name.clone(),
        status: row.status.clone(),
        expires_at: row.expires_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn grant_metadata(row: &DevelopmentSecretGrantRow) -> SecretGrantMetadata {
    SecretGrantMetadata {
        id: row.id.clone(),
        secret_id: row.secret_id.clone(),
        scope_type: row.scope_type.clone(),
        scope_id: row.scope_id.clone(),
        environment_key: row.environment_key.clone(),
        status: row.status.clone(),
        expires_at: row.expires_at,
    }
}

fn valid_environment_key(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_uppercase())
        && characters.all(|character| character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit())
}

#[derive(Clone)]
pub struct SecretRedactor {
    values: Vec<String>,
}

impl SecretRedactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values = values.into_iter().filter(|value| !value.is_empty()).collect::<Vec<_>>();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self { values }
    }

    pub fn redact_text(&self, value: &str) -> String {
        redact_text(value, &self.values)
    }
}

pub fn redact_text(value: &str, secret_values: &[String]) -> String {
    let mut redacted = value.to_owned();
    let mut secrets = secret_values
        .iter()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        redacted = redacted.replace(secret, "[REDACTED]");
    }
    let mut authorization_parts = 0_u8;
    redacted
        .split_whitespace()
        .map(|token| {
            let lower = token.to_ascii_lowercase();
            let is_authorization_header =
                lower.trim_matches(|character: char| !character.is_ascii_alphanumeric()) == "authorization";
            if is_authorization_header {
                authorization_parts = 2;
                return token.to_owned();
            }
            if authorization_parts > 0 {
                authorization_parts -= 1;
                return preserve_punctuation(token, "[REDACTED]");
            }
            if lower.contains("ghp_") || lower.contains("github_pat_") {
                return preserve_punctuation(token, "[REDACTED]");
            }
            if let Some((key, _)) = token.split_once('=')
                && [
                    "token",
                    "secret",
                    "password",
                    "passwd",
                    "api_key",
                    "apikey",
                    "authorization",
                ]
                .iter()
                .any(|marker| key.to_ascii_lowercase().contains(marker))
            {
                return format!("{key}=[REDACTED]");
            }
            redact_url_userinfo(token)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn preserve_punctuation(original: &str, replacement: &str) -> String {
    let suffix = original
        .chars()
        .rev()
        .take_while(|character| matches!(character, ',' | ';' | ')' | ']' | '}' | '"'))
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{replacement}{suffix}")
}

fn redact_url_userinfo(token: &str) -> String {
    let Some(scheme) = token.find("://") else {
        return token.into();
    };
    let authority_start = scheme + 3;
    let authority_end = token[authority_start..]
        .find(['/', '?', '#'])
        .map(|offset| authority_start + offset)
        .unwrap_or(token.len());
    let authority = &token[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return token.into();
    };
    format!(
        "{}[REDACTED]@{}{}",
        &token[..authority_start],
        &authority[at + 1..],
        &token[authority_end..]
    )
}
