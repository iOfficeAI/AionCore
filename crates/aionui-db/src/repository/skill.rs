use crate::error::DbError;
use crate::models::{SkillImportRecordRow, SkillRegistryInstallRow, SkillRow};

/// Skill metadata and import-history data access abstraction.
#[async_trait::async_trait]
pub trait ISkillRepository: Send + Sync {
    /// Returns active skills ordered by most recent update first.
    async fn list(&self) -> Result<Vec<SkillRow>, DbError>;

    /// Returns active global and user-owned skills for `user_id`.
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<SkillRow>, DbError>;

    /// Finds an active skill by name.
    async fn find_by_name(&self, name: &str) -> Result<Option<SkillRow>, DbError>;

    /// Finds an active global or user-owned skill by name.
    async fn find_by_name_for_user(&self, user_id: &str, name: &str) -> Result<Option<SkillRow>, DbError>;

    /// Finds a skill by name, including soft-deleted rows.
    async fn find_by_name_any(&self, name: &str) -> Result<Option<SkillRow>, DbError>;

    /// Finds a global or user-owned skill by name, including soft-deleted rows.
    async fn find_by_name_any_for_user(&self, user_id: &str, name: &str) -> Result<Option<SkillRow>, DbError>;

    /// Creates or updates a user skill by name and clears soft-delete state.
    async fn upsert(&self, params: UpsertSkillParams<'_>) -> Result<SkillRow, DbError>;

    /// Creates or updates a skill owned by `user_id` and clears soft-delete state.
    async fn upsert_for_user(&self, user_id: &str, params: UpsertSkillParams<'_>) -> Result<SkillRow, DbError>;

    /// Creates or updates a global skill row (`user_id IS NULL`).
    async fn upsert_global(&self, params: UpsertSkillParams<'_>) -> Result<SkillRow, DbError> {
        self.upsert(params).await
    }

    /// Soft-deletes an active skill by name.
    async fn delete_by_name(&self, name: &str) -> Result<SkillRow, DbError>;

    /// Soft-deletes an active skill owned by `user_id`.
    async fn delete_by_name_for_user(&self, user_id: &str, name: &str) -> Result<SkillRow, DbError>;

    /// Soft-deletes a user skill and removes its registry provenance atomically when supported.
    async fn delete_by_name_with_registry_origin_for_user(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<SkillRow, DbError> {
        let row = self.delete_by_name_for_user(user_id, name).await?;
        self.delete_registry_install_by_skill_for_user(user_id, &row.id).await?;
        Ok(row)
    }

    /// Appends one import record.
    async fn create_import_record(
        &self,
        params: CreateSkillImportRecordParams<'_>,
    ) -> Result<SkillImportRecordRow, DbError>;

    /// Appends one import record owned by `user_id`.
    async fn create_import_record_for_user(
        &self,
        user_id: &str,
        params: CreateSkillImportRecordParams<'_>,
    ) -> Result<SkillImportRecordRow, DbError>;

    /// Lists recent import records ordered by creation time descending.
    async fn list_import_records(&self, limit: i64) -> Result<Vec<SkillImportRecordRow>, DbError>;

    /// Lists recent import records owned by `user_id`.
    async fn list_import_records_for_user(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<SkillImportRecordRow>, DbError>;

    async fn list_registry_installs_for_user(&self, _user_id: &str) -> Result<Vec<SkillRegistryInstallRow>, DbError> {
        Ok(Vec::new())
    }

    async fn find_registry_install_for_user(
        &self,
        _user_id: &str,
        _registry_key: &str,
        _namespace: &str,
        _slug: &str,
    ) -> Result<Option<SkillRegistryInstallRow>, DbError> {
        Ok(None)
    }

    async fn find_registry_install_by_skill_for_user(
        &self,
        _user_id: &str,
        _skill_id: &str,
    ) -> Result<Option<SkillRegistryInstallRow>, DbError> {
        Ok(None)
    }

    async fn upsert_registry_install_for_user(
        &self,
        _user_id: &str,
        _params: UpsertSkillRegistryInstallParams<'_>,
    ) -> Result<SkillRegistryInstallRow, DbError> {
        Err(DbError::NotFound(
            "skill registry install repository is unavailable".into(),
        ))
    }

    async fn delete_registry_install_by_skill_for_user(&self, _user_id: &str, _skill_id: &str) -> Result<(), DbError> {
        Ok(())
    }
}

/// Parameters for creating or updating a skill row.
#[derive(Debug, Clone)]
pub struct UpsertSkillParams<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub path: &'a str,
    pub source: &'a str,
    pub enabled: bool,
}

/// Parameters for appending a skill import history row.
#[derive(Debug, Clone)]
pub struct CreateSkillImportRecordParams<'a> {
    pub operation_id: &'a str,
    pub source_label: &'a str,
    pub source_path: Option<&'a str>,
    pub source_name: &'a str,
    pub skill_id: Option<&'a str>,
    pub skill_name: Option<&'a str>,
    pub status: &'a str,
    pub error_code: Option<&'a str>,
    pub error_path: Option<&'a str>,
    pub actual_bytes: Option<i64>,
    pub limit_bytes: Option<i64>,
    pub line: Option<i64>,
    pub column: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct UpsertSkillRegistryInstallParams<'a> {
    pub skill_id: &'a str,
    pub registry_key: &'a str,
    pub namespace: &'a str,
    pub slug: &'a str,
    pub remote_skill_id: i64,
    pub remote_version_id: i64,
    pub installed_version: &'a str,
}
