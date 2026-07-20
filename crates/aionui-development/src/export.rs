use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use aionui_api_types::{
    DevelopmentEvaluation, EvaluationComparison, EvaluationComparisonRequest, EvaluationRecordInput,
    EvaluationRegression, ImportProjectBundleRequest, PlatformInstanceSummary, ProjectExportBundle,
    ProjectExportManifest, ProjectImportReport,
};
use aionui_common::now_ms;
use aionui_db::SqlitePool;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};

use crate::DevelopmentError;

const FORMAT_VERSION: u32 = 1;
const SCHEMA_VERSION: i64 = 34;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct PortabilityService {
    pool: SqlitePool,
    signing_key: SigningKey,
    trusted_signers: BTreeSet<String>,
    instance_fallback: String,
}

impl PortabilityService {
    pub fn new(pool: SqlitePool, signing_secret: &[u8], instance_fallback: impl Into<String>) -> Self {
        let digest = Sha256::digest(signing_secret);
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        let signing_key = SigningKey::from_bytes(&bytes);
        let trusted_signers = BTreeSet::from([hex::encode(signing_key.verifying_key().to_bytes())]);
        Self {
            pool,
            signing_key,
            trusted_signers,
            instance_fallback: instance_fallback.into(),
        }
    }

    pub fn trust_signer(&mut self, public_key: &str) -> Result<(), DevelopmentError> {
        let bytes = hex::decode(public_key)
            .map_err(|_| DevelopmentError::BadRequest("invalid trusted signer public key encoding".into()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| DevelopmentError::BadRequest("invalid trusted signer public key".into()))?;
        VerifyingKey::from_bytes(&bytes)
            .map_err(|_| DevelopmentError::BadRequest("invalid trusted signer public key".into()))?;
        self.trusted_signers.insert(hex::encode(bytes));
        Ok(())
    }

    pub fn signer_public_key(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    pub async fn export_project(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ProjectExportBundle, DevelopmentError> {
        let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id = ? AND user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        if owned != 1 {
            return Err(DevelopmentError::NotFound(format!("project {project_id}")));
        }

        let mut records = BTreeMap::new();
        records.insert(
            "projects".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',id,'user_id',user_id,'name',name,'local_path',local_path,\
                 'repository_url',repository_url,'default_branch',default_branch,'project_type',project_type,\
                 'created_at',created_at,'updated_at',updated_at) FROM projects WHERE id=? AND user_id=?",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "project_command_profiles".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('project_id',project_id,'install_command',install_command,'format_command',format_command,\
                 'lint_command',lint_command,'typecheck_command',typecheck_command,'unit_test_command',unit_test_command,\
                 'integration_test_command',integration_test_command,'e2e_command',e2e_command,'build_command',build_command,\
                 'security_scan_command',security_scan_command,'command_timeout_seconds',command_timeout_seconds,\
                 'updated_at',updated_at) FROM project_command_profiles WHERE project_id=?",
                &[project_id],
            )
            .await?,
        );
        records.insert(
            "project_runtime_profiles".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('project_id',project_id,'environment_kind',environment_kind,'language',language,\
                 'package_manager',package_manager,'runtime_version',runtime_version,'env_keys',env_keys,\
                 'metadata',metadata,'updated_at',updated_at) FROM project_runtime_profiles WHERE project_id=?",
                &[project_id],
            )
            .await?,
        );
        records.insert(
            "project_resource_links".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('project_id',project_id,'user_id',user_id,'resource_type',resource_type,\
                 'resource_id',resource_id,'created_at',created_at) FROM project_resource_links \
                 WHERE project_id=? AND user_id=? AND resource_type IN ('conversation','team')",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "conversations".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',c.id,'user_id',c.user_id,'name',c.name,'type',c.type,'extra',c.extra,\
                 'model',c.model,'status',c.status,'source',c.source,'channel_chat_id',c.channel_chat_id,\
                 'pinned',c.pinned,'pinned_at',c.pinned_at,'created_at',c.created_at,'updated_at',c.updated_at) \
                 FROM conversations c JOIN project_resource_links l ON l.resource_type='conversation' AND l.resource_id=c.id \
                 WHERE l.project_id=? AND l.user_id=? ORDER BY c.created_at,c.id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "messages".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',m.id,'conversation_id',m.conversation_id,'msg_id',m.msg_id,'type',m.type,\
                 'content',m.content,'position',m.position,'status',m.status,'hidden',m.hidden,'created_at',m.created_at) \
                 FROM messages m JOIN project_resource_links l ON l.resource_type='conversation' AND l.resource_id=m.conversation_id \
                 WHERE l.project_id=? AND l.user_id=? ORDER BY m.created_at,m.id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "teams".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',t.id,'user_id',t.user_id,'name',t.name,'workspace',t.workspace,\
                 'workspace_mode',t.workspace_mode,'agents',t.agents,'lead_agent_id',t.lead_agent_id,\
                 'session_mode',t.session_mode,'agents_version',t.agents_version,'created_at',t.created_at,'updated_at',t.updated_at) \
                 FROM teams t JOIN project_resource_links l ON l.resource_type='team' AND l.resource_id=t.id \
                 WHERE l.project_id=? AND l.user_id=? ORDER BY t.created_at,t.id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "assistant_users".into(),
            fetch_json(
                &self.pool,
                "SELECT DISTINCT json_object('id',u.id,'platform_user_id',u.platform_user_id,'platform_type',u.platform_type) \
                 FROM assistant_users u JOIN assistant_sessions s ON s.user_id=u.id \
                 JOIN project_resource_links l ON l.resource_type='conversation' AND l.resource_id=s.conversation_id \
                 WHERE l.project_id=? AND l.user_id=? ORDER BY u.id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "assistant_sessions".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',s.id,'user_id',s.user_id,'agent_type',s.agent_type,\
                 'conversation_id',s.conversation_id,'workspace',s.workspace,'chat_id',s.chat_id,\
                 'message_thread_id',s.message_thread_id,'bound_agent_id',s.bound_agent_id,\
                 'bound_backend',s.bound_backend,'bound_provider_id',s.bound_provider_id,'bound_model',s.bound_model,\
                 'created_at',s.created_at,'last_activity',s.last_activity) \
                 FROM assistant_sessions s JOIN project_resource_links l \
                   ON l.resource_type='conversation' AND l.resource_id=s.conversation_id \
                 WHERE l.project_id=? AND l.user_id=? ORDER BY s.created_at,s.id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "telegram_topic_bindings".into(),
            fetch_json(
                &self.pool,
                "SELECT DISTINCT json_object('chat_id',b.chat_id,'message_thread_id',b.message_thread_id,\
                 'agent_id',b.agent_id,'bound_by_user_id',b.bound_by_user_id,'created_at',b.created_at,'updated_at',b.updated_at) \
                 FROM telegram_topic_bindings b JOIN assistant_sessions s \
                   ON s.chat_id=b.chat_id AND s.message_thread_id=b.message_thread_id \
                 JOIN project_resource_links l ON l.resource_type='conversation' AND l.resource_id=s.conversation_id \
                 WHERE l.project_id=? AND l.user_id=? ORDER BY b.chat_id,b.message_thread_id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "channel_topic_model_overrides".into(),
            fetch_json(
                &self.pool,
                "SELECT DISTINCT json_object('platform',o.platform,'internal_user_id',o.internal_user_id,'chat_id',o.chat_id,\
                 'message_thread_id',o.message_thread_id,'agent_id',o.agent_id,'provider_id',o.provider_id,\
                 'model',o.model,'updated_at',o.updated_at) FROM channel_topic_model_overrides o \
                 JOIN assistant_sessions s ON s.chat_id=o.chat_id AND s.message_thread_id=o.message_thread_id \
                 JOIN project_resource_links l ON l.resource_type='conversation' AND l.resource_id=s.conversation_id \
                 WHERE l.project_id=? AND l.user_id=? AND o.internal_user_id=l.user_id \
                 ORDER BY o.chat_id,o.message_thread_id,o.internal_user_id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "development_policies".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',id,'user_id',user_id,'project_id',project_id,'isolation_mode',isolation_mode,\
                 'container_image',container_image,'devcontainer_config_path',devcontainer_config_path,\
                 'container_cpu_millis',container_cpu_millis,'container_memory_mb',container_memory_mb,\
                 'container_pids_limit',container_pids_limit,'network_mode',network_mode,\
                 'allowed_secret_keys_json',allowed_secret_keys_json,'allowed_commands_json',allowed_commands_json,\
                 'protected_paths_json',protected_paths_json,'allowed_network_hosts_json',allowed_network_hosts_json,\
                 'protected_branches_json',protected_branches_json,'dangerous_confirmation_count',dangerous_confirmation_count,\
                 'max_duration_ms',max_duration_ms,'max_parallel_agents',max_parallel_agents,'max_retries',max_retries,\
                 'max_cost_microunits',max_cost_microunits,'max_total_tokens',max_total_tokens,'fallback_model',fallback_model,\
                 'alert_percent',alert_percent,'over_limit_action',over_limit_action,'created_at',created_at,'updated_at',updated_at) \
                 FROM development_policies WHERE project_id=? AND user_id=?",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "development_retention_policies".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('user_id',user_id,'project_id',project_id,\
                 'conversation_history_days',conversation_history_days,'artifact_days',artifact_days,\
                 'evaluation_days',evaluation_days,'immutable_audit_log',immutable_audit_log,'updated_at',updated_at) \
                 FROM development_retention_policies WHERE project_id=? AND user_id=?",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "development_runs".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',id,'user_id',user_id,'project_id',project_id,'team_id',team_id,\
                 'source_channel',source_channel,'source_user_id',source_user_id,'execution_mode',execution_mode,\
                 'status',status,'request_summary',request_summary,'acceptance_criteria',acceptance_criteria,\
                 'baseline_commit',baseline_commit,'integration_branch',integration_branch,'started_at',started_at,\
                 'finished_at',finished_at,'created_at',created_at,'updated_at',updated_at) \
                 FROM development_runs WHERE project_id=? AND user_id=? ORDER BY created_at,id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "development_deliveries".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',id,'run_id',run_id,'project_id',project_id,'user_id',user_id,'provider',provider,\
                 'repository',repository,'branch',branch,'base_branch',base_branch,'commit_sha',commit_sha,'status',status,\
                 'push_status',push_status,'pr_number',pr_number,'pr_url',pr_url,'pr_status',pr_status,'ci_status',ci_status,\
                 'review_status',review_status,'merge_status',merge_status,'report_json',report_json,'last_error',last_error,\
                 'created_at',created_at,'updated_at',updated_at) FROM development_deliveries \
                 WHERE project_id=? AND user_id=? ORDER BY created_at,id",
                &[project_id, user_id],
            )
            .await?,
        );
        records.insert(
            "development_audit_events".into(),
            fetch_json(
                &self.pool,
                "SELECT json_object('id',id,'user_id',user_id,'actor_type',actor_type,'actor_id',actor_id,'action',action,\
                 'target_type',target_type,'target_id',target_id,'project_id',project_id,'run_id',run_id,'task_id',task_id,\
                 'result',result,'redacted_payload_json',redacted_payload_json,'created_at',created_at) \
                 FROM development_audit_events WHERE project_id=? AND user_id=? ORDER BY created_at,id",
                &[project_id, user_id],
            )
            .await?,
        );

        let instance = self.platform_instance().await?;
        let mut bundle = ProjectExportBundle {
            manifest: ProjectExportManifest {
                format_version: FORMAT_VERSION,
                schema_version: SCHEMA_VERSION,
                app_version: APP_VERSION.into(),
                source_instance_id: instance.instance_id,
                exported_at: now_ms(),
                project_id: project_id.into(),
                record_counts: BTreeMap::new(),
                payload_checksum: String::new(),
                signer_public_key: self.signer_public_key(),
                signature: String::new(),
            },
            records,
        };
        self.seal_bundle(&mut bundle)?;
        Ok(bundle)
    }

    pub fn seal_bundle(&self, bundle: &mut ProjectExportBundle) -> Result<(), DevelopmentError> {
        bundle.manifest.record_counts = bundle
            .records
            .iter()
            .map(|(name, values)| (name.clone(), values.len()))
            .collect();
        let payload = serde_json::to_vec(&bundle.records).map_err(internal)?;
        bundle.manifest.payload_checksum = format!("sha256:{}", hex::encode(Sha256::digest(payload)));
        bundle.manifest.signature = hex::encode(self.signing_key.sign(&signature_input(&bundle.manifest)).to_bytes());
        Ok(())
    }

    pub fn validate_bundle(&self, bundle: &ProjectExportBundle) -> Result<(), DevelopmentError> {
        if bundle.manifest.format_version != FORMAT_VERSION {
            return Err(DevelopmentError::BadRequest(format!(
                "unsupported project bundle format version {}",
                bundle.manifest.format_version
            )));
        }
        if bundle.manifest.schema_version > SCHEMA_VERSION {
            return Err(DevelopmentError::BadRequest(format!(
                "unsupported future schema version {}",
                bundle.manifest.schema_version
            )));
        }
        let allowed = portable_tables();
        if let Some(table) = bundle.records.keys().find(|table| !allowed.contains(table.as_str())) {
            return Err(DevelopmentError::BadRequest(format!(
                "unsupported or sensitive export table {table}"
            )));
        }
        let counts: BTreeMap<_, _> = bundle
            .records
            .iter()
            .map(|(name, values)| (name.clone(), values.len()))
            .collect();
        if counts != bundle.manifest.record_counts {
            return Err(DevelopmentError::BadRequest("record count mismatch".into()));
        }
        let payload = serde_json::to_vec(&bundle.records).map_err(internal)?;
        let checksum = format!("sha256:{}", hex::encode(Sha256::digest(payload)));
        if checksum != bundle.manifest.payload_checksum {
            return Err(DevelopmentError::BadRequest("project bundle checksum mismatch".into()));
        }
        let signature_bytes = hex::decode(&bundle.manifest.signature)
            .map_err(|_| DevelopmentError::BadRequest("invalid project bundle signature encoding".into()))?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|_| DevelopmentError::BadRequest("invalid project bundle signature".into()))?;
        let public_key_bytes = hex::decode(&bundle.manifest.signer_public_key)
            .map_err(|_| DevelopmentError::BadRequest("invalid signer public key encoding".into()))?;
        let public_key_bytes: [u8; 32] = public_key_bytes
            .try_into()
            .map_err(|_| DevelopmentError::BadRequest("invalid signer public key".into()))?;
        let normalized_public_key = hex::encode(public_key_bytes);
        if !self.trusted_signers.contains(&normalized_public_key) {
            return Err(DevelopmentError::BadRequest(
                "project bundle signer is not trusted by this server".into(),
            ));
        }
        VerifyingKey::from_bytes(&public_key_bytes)
            .map_err(|_| DevelopmentError::BadRequest("invalid signer public key".into()))?
            .verify(&signature_input(&bundle.manifest), &signature)
            .map_err(|_| DevelopmentError::BadRequest("project bundle signature verification failed".into()))?;
        Ok(())
    }

    pub async fn import_project(
        &self,
        owner_id: &str,
        request: ImportProjectBundleRequest,
    ) -> Result<ProjectImportReport, DevelopmentError> {
        self.validate_bundle(&request.bundle)?;
        validate_absolute_import_path(&request.local_path)?;
        let user_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id=?")
            .bind(owner_id)
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        if user_exists != 1 {
            return Err(DevelopmentError::NotFound(format!("owner {owner_id}")));
        }
        let project = only_record(&request.bundle, "projects")?;
        let project_id = text(project, "id")?.to_owned();
        if project_id != request.bundle.manifest.project_id {
            return Err(DevelopmentError::BadRequest(
                "manifest project id does not match payload".into(),
            ));
        }
        let source_path = text(project, "local_path")?.to_owned();
        validate_import_relationships(&request.bundle, &source_path, &request.local_path)?;
        for policy in records(&request.bundle, "development_policies") {
            if let Some(path) = optional_text(policy, "devcontainer_config_path")? {
                validate_relative_path(path)?;
            }
        }
        let conflicts =
            collect_import_conflicts(&self.pool, owner_id, &project_id, &request.local_path, &request.bundle).await?;
        if !conflicts.is_empty() {
            return Ok(ProjectImportReport {
                project_id,
                owner_id: owner_id.into(),
                imported: false,
                imported_counts: BTreeMap::new(),
                conflicts,
            });
        }

        let mut transaction = self.pool.begin().await.map_err(internal)?;
        let result = import_records(
            &mut transaction,
            owner_id,
            &request.local_path,
            &source_path,
            &request.bundle,
        )
        .await;
        if let Err(error) = result {
            transaction.rollback().await.map_err(internal)?;
            return Err(DevelopmentError::Internal(format!(
                "import transaction failed: {error}"
            )));
        }
        transaction.commit().await.map_err(internal)?;
        Ok(ProjectImportReport {
            project_id,
            owner_id: owner_id.into(),
            imported: true,
            imported_counts: request.bundle.manifest.record_counts,
            conflicts: Vec::new(),
        })
    }

    pub async fn platform_instance(&self) -> Result<PlatformInstanceSummary, DevelopmentError> {
        let now = now_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO platform_instances \
             (singleton,instance_id,schema_version,app_version,first_started_at,last_started_at) VALUES (1,?,?,?,?,?)",
        )
        .bind(&self.instance_fallback)
        .bind(SCHEMA_VERSION)
        .bind(APP_VERSION)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        let row = sqlx::query(
            "SELECT instance_id,schema_version,app_version,first_started_at,last_started_at FROM platform_instances WHERE singleton=1",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        Ok(PlatformInstanceSummary {
            instance_id: row.try_get("instance_id").map_err(internal)?,
            schema_version: row.try_get("schema_version").map_err(internal)?,
            app_version: row.try_get("app_version").map_err(internal)?,
            first_started_at: row.try_get("first_started_at").map_err(internal)?,
            last_started_at: row.try_get("last_started_at").map_err(internal)?,
            data_size_bytes: page_count.saturating_mul(page_size),
        })
    }

    pub async fn record_startup(&self) -> Result<(), DevelopmentError> {
        let now = now_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO platform_instances \
             (singleton,instance_id,schema_version,app_version,first_started_at,last_started_at) VALUES (1,?,?,?,?,?)",
        )
        .bind(&self.instance_fallback)
        .bind(SCHEMA_VERSION)
        .bind(APP_VERSION)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        sqlx::query("UPDATE platform_instances SET schema_version=?,app_version=?,last_started_at=? WHERE singleton=1")
            .bind(SCHEMA_VERSION)
            .bind(APP_VERSION)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
    }

    pub async fn record_evaluation(
        &self,
        user_id: &str,
        input: EvaluationRecordInput,
    ) -> Result<DevelopmentEvaluation, DevelopmentError> {
        validate_evaluation(&input)?;
        let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id=? AND user_id=?")
            .bind(&input.project_id)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        if owned != 1 {
            return Err(DevelopmentError::NotFound(format!("project {}", input.project_id)));
        }
        if input.accepted_baseline && input.result != "passed" {
            return Err(DevelopmentError::BadRequest(
                "accepted baseline requires a passing evaluation".into(),
            ));
        }
        let row = DevelopmentEvaluation {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: user_id.into(),
            project_id: input.project_id,
            release_id: input.release_id,
            scenario_id: input.scenario_id,
            result: input.result,
            duration_ms: input.duration_ms,
            failure_category: input.failure_category,
            input_tokens: input.input_tokens,
            output_tokens: input.output_tokens,
            cost_microunits: input.cost_microunits,
            cost_source: input.cost_source,
            accepted_baseline: input.accepted_baseline,
            created_at: now_ms(),
        };
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        sqlx::query(
            "INSERT INTO development_evaluations \
             (id,user_id,project_id,release_id,scenario_id,result,duration_ms,failure_category,input_tokens,\
              output_tokens,cost_microunits,cost_source,accepted_baseline,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.release_id)
        .bind(&row.scenario_id)
        .bind(&row.result)
        .bind(row.duration_ms)
        .bind(&row.failure_category)
        .bind(row.input_tokens)
        .bind(row.output_tokens)
        .bind(row.cost_microunits)
        .bind(&row.cost_source)
        .bind(row.accepted_baseline)
        .bind(row.created_at)
        .execute(&mut *transaction)
        .await
        .map_err(|error| DevelopmentError::Conflict(format!("evaluation already exists or is invalid: {error}")))?;
        if row.accepted_baseline {
            sqlx::query(
                "INSERT INTO development_evaluation_baselines (user_id,project_id,release_id,accepted_at) \
                 VALUES (?,?,?,?) ON CONFLICT(user_id,project_id) DO UPDATE SET \
                 release_id=excluded.release_id,accepted_at=excluded.accepted_at",
            )
            .bind(&row.user_id)
            .bind(&row.project_id)
            .bind(&row.release_id)
            .bind(row.created_at)
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        }
        transaction.commit().await.map_err(internal)?;
        Ok(row)
    }

    pub async fn compare_evaluations(
        &self,
        user_id: &str,
        request: EvaluationComparisonRequest,
    ) -> Result<EvaluationComparison, DevelopmentError> {
        if request.required_scenarios.is_empty()
            || !(0..=10_000).contains(&request.max_duration_regression_percent)
            || !(0..=10_000).contains(&request.max_cost_regression_percent)
        {
            return Err(DevelopmentError::BadRequest(
                "invalid evaluation comparison thresholds".into(),
            ));
        }
        let mut regressions = Vec::new();
        let baseline_release: Option<String> = sqlx::query_scalar(
            "SELECT release_id FROM development_evaluation_baselines WHERE user_id=? AND project_id=?",
        )
        .bind(user_id)
        .bind(&request.project_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        let baseline_releases = baseline_release.iter().cloned().collect();
        for scenario in &request.required_scenarios {
            let current = evaluation_row(
                &self.pool,
                "SELECT * FROM development_evaluations WHERE user_id=? AND project_id=? AND release_id=? AND scenario_id=?",
                user_id,
                &request.project_id,
                &request.release_id,
                scenario,
            )
            .await?;
            let Some(current) = current else {
                regressions.push(regression(
                    scenario,
                    "missing",
                    "required scenario has no current result",
                ));
                continue;
            };
            if current.result != "passed" {
                regressions.push(regression(
                    scenario,
                    current.failure_category.as_deref().unwrap_or("result"),
                    "current scenario did not pass",
                ));
            }
            let Some(baseline_release) = baseline_release.as_deref() else {
                regressions.push(regression(
                    scenario,
                    "baseline_missing",
                    "accepted baseline release is missing",
                ));
                continue;
            };
            if baseline_release == request.release_id {
                regressions.push(regression(
                    scenario,
                    "baseline_invalid",
                    "candidate release cannot be its own baseline",
                ));
                continue;
            }
            let baseline = evaluation_row(
                &self.pool,
                "SELECT * FROM development_evaluations WHERE user_id=? AND project_id=? AND release_id=? \
                 AND scenario_id=?",
                user_id,
                &request.project_id,
                baseline_release,
                scenario,
            )
            .await?;
            let Some(baseline) = baseline else {
                regressions.push(regression(
                    scenario,
                    "baseline_missing",
                    "scenario is missing from accepted baseline release",
                ));
                continue;
            };
            if baseline.result != "passed" {
                regressions.push(regression(
                    scenario,
                    "baseline_invalid",
                    "accepted baseline scenario did not pass",
                ));
                continue;
            }
            if exceeds_percent(
                current.duration_ms,
                baseline.duration_ms,
                request.max_duration_regression_percent,
            ) {
                regressions.push(regression(
                    scenario,
                    "duration",
                    "duration exceeded accepted baseline threshold",
                ));
            }
            if exceeds_percent(
                current.cost_microunits,
                baseline.cost_microunits,
                request.max_cost_regression_percent,
            ) {
                regressions.push(regression(
                    scenario,
                    "cost",
                    "cost exceeded accepted baseline threshold",
                ));
            }
        }
        Ok(EvaluationComparison {
            allowed: regressions.is_empty(),
            release_id: request.release_id,
            baseline_release_ids: baseline_releases,
            regressions,
        })
    }
}

fn signature_input(manifest: &ProjectExportManifest) -> Vec<u8> {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{:?}\n{}\n{}",
        manifest.format_version,
        manifest.schema_version,
        manifest.app_version,
        manifest.source_instance_id,
        manifest.exported_at,
        manifest.project_id,
        manifest.record_counts,
        manifest.signer_public_key,
        manifest.payload_checksum
    )
    .into_bytes()
}

fn portable_tables() -> BTreeSet<&'static str> {
    [
        "projects",
        "project_command_profiles",
        "project_runtime_profiles",
        "project_resource_links",
        "conversations",
        "messages",
        "teams",
        "assistant_users",
        "assistant_sessions",
        "telegram_topic_bindings",
        "channel_topic_model_overrides",
        "development_policies",
        "development_retention_policies",
        "development_runs",
        "development_deliveries",
        "development_audit_events",
    ]
    .into_iter()
    .collect()
}

async fn fetch_json(pool: &SqlitePool, sql: &str, binds: &[&str]) -> Result<Vec<Value>, DevelopmentError> {
    let mut query = sqlx::query_scalar::<_, String>(sql);
    for value in binds {
        query = query.bind(*value);
    }
    query
        .fetch_all(pool)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|value| serde_json::from_str(&value).map_err(internal))
        .collect()
}

fn records<'a>(bundle: &'a ProjectExportBundle, table: &str) -> &'a [Value] {
    bundle.records.get(table).map(Vec::as_slice).unwrap_or_default()
}

fn only_record<'a>(bundle: &'a ProjectExportBundle, table: &str) -> Result<&'a Value, DevelopmentError> {
    let values = records(bundle, table);
    if values.len() != 1 {
        return Err(DevelopmentError::BadRequest(format!(
            "{table} must contain exactly one record"
        )));
    }
    Ok(&values[0])
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, DevelopmentError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| DevelopmentError::BadRequest(format!("missing or invalid field {key}")))
}

fn optional_text<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, DevelopmentError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(DevelopmentError::BadRequest(format!("invalid field {key}"))),
    }
}

fn number(value: &Value, key: &str) -> Result<i64, DevelopmentError> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| DevelopmentError::BadRequest(format!("missing or invalid field {key}")))
}

fn optional_number(value: &Value, key: &str) -> Result<Option<i64>, DevelopmentError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| DevelopmentError::BadRequest(format!("invalid field {key}"))),
        _ => Err(DevelopmentError::BadRequest(format!("invalid field {key}"))),
    }
}

fn validate_absolute_import_path(path: &str) -> Result<(), DevelopmentError> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(DevelopmentError::BadRequest(
            "path traversal in import local_path".into(),
        ));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(DevelopmentError::BadRequest(format!(
                    "import path contains a symbolic link: {}",
                    current.display()
                )));
            }
            Ok(metadata) if current == path && !metadata.is_dir() => {
                return Err(DevelopmentError::BadRequest(format!(
                    "import local_path is not a directory: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DevelopmentError::BadRequest(format!(
                    "cannot validate import path {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), DevelopmentError> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(DevelopmentError::BadRequest(
            "path traversal in portable configuration".into(),
        ));
    }
    Ok(())
}

fn record_ids(bundle: &ProjectExportBundle, table: &str, key: &str) -> Result<BTreeSet<String>, DevelopmentError> {
    let mut ids = BTreeSet::new();
    for row in records(bundle, table) {
        let id = text(row, key)?.to_owned();
        if !ids.insert(id.clone()) {
            return Err(DevelopmentError::BadRequest(format!(
                "import validation failed: duplicate {table}:{id}"
            )));
        }
    }
    Ok(ids)
}

fn require_unique_key(seen: &mut BTreeSet<String>, table: &str, key: String) -> Result<(), DevelopmentError> {
    if seen.insert(key.clone()) {
        Ok(())
    } else {
        Err(DevelopmentError::BadRequest(format!(
            "import validation failed: duplicate {table}:{key}"
        )))
    }
}

fn validate_project_references(
    bundle: &ProjectExportBundle,
    table: &str,
    project_id: &str,
) -> Result<(), DevelopmentError> {
    for row in records(bundle, table) {
        if text(row, "project_id")? != project_id {
            return Err(DevelopmentError::BadRequest(format!(
                "import validation failed: {table} references another project"
            )));
        }
    }
    Ok(())
}

fn require_reference(
    table: &str,
    value: &str,
    target_table: &str,
    targets: &BTreeSet<String>,
) -> Result<(), DevelopmentError> {
    if targets.contains(value) {
        Ok(())
    } else {
        Err(DevelopmentError::BadRequest(format!(
            "import validation failed: {table} references missing {target_table}:{value}"
        )))
    }
}

fn remap_workspace(workspace: &str, source_path: &str, local_path: &str) -> Result<String, DevelopmentError> {
    validate_absolute_import_path(source_path)?;
    validate_absolute_import_path(workspace)?;
    let relative = Path::new(workspace).strip_prefix(source_path).map_err(|_| {
        DevelopmentError::BadRequest(format!(
            "import validation failed: team workspace is outside source project: {workspace}"
        ))
    })?;
    let remapped = Path::new(local_path).join(relative).to_string_lossy().into_owned();
    validate_absolute_import_path(&remapped)?;
    Ok(remapped)
}

fn validate_import_relationships(
    bundle: &ProjectExportBundle,
    source_path: &str,
    local_path: &str,
) -> Result<(), DevelopmentError> {
    let project_id = text(only_record(bundle, "projects")?, "id")?;
    let conversations = record_ids(bundle, "conversations", "id")?;
    let messages = record_ids(bundle, "messages", "id")?;
    let teams = record_ids(bundle, "teams", "id")?;
    let assistant_users = record_ids(bundle, "assistant_users", "id")?;
    let assistant_sessions = record_ids(bundle, "assistant_sessions", "id")?;
    let runs = record_ids(bundle, "development_runs", "id")?;
    let deliveries = record_ids(bundle, "development_deliveries", "id")?;
    let audits = record_ids(bundle, "development_audit_events", "id")?;
    let _ = (messages, assistant_sessions, deliveries, audits);

    for table in [
        "project_command_profiles",
        "project_runtime_profiles",
        "development_policies",
        "development_retention_policies",
    ] {
        if records(bundle, table).len() > 1 {
            return Err(DevelopmentError::BadRequest(format!(
                "import validation failed: duplicate {table}:{project_id}"
            )));
        }
    }

    for table in [
        "project_command_profiles",
        "project_runtime_profiles",
        "project_resource_links",
        "development_policies",
        "development_retention_policies",
        "development_runs",
        "development_deliveries",
        "development_audit_events",
    ] {
        validate_project_references(bundle, table, project_id)?;
    }
    for row in records(bundle, "messages") {
        require_reference(
            "messages",
            text(row, "conversation_id")?,
            "conversations",
            &conversations,
        )?;
    }
    for row in records(bundle, "teams") {
        remap_workspace(text(row, "workspace")?, source_path, local_path)?;
    }
    let mut resource_links = BTreeSet::new();
    for row in records(bundle, "project_resource_links") {
        let resource_type = text(row, "resource_type")?;
        let resource_id = text(row, "resource_id")?;
        require_unique_key(
            &mut resource_links,
            "project_resource_links",
            format!("{resource_type}:{resource_id}"),
        )?;
        match resource_type {
            "conversation" => {
                require_reference("project_resource_links", resource_id, "conversations", &conversations)?
            }
            "team" => require_reference("project_resource_links", resource_id, "teams", &teams)?,
            _ => {
                return Err(DevelopmentError::BadRequest(format!(
                    "import validation failed: unsupported resource link type {resource_type}"
                )));
            }
        }
    }
    let mut user_identities = BTreeSet::new();
    for row in records(bundle, "assistant_users") {
        require_unique_key(
            &mut user_identities,
            "assistant_users identity",
            format!("{}:{}", text(row, "platform_type")?, text(row, "platform_user_id")?),
        )?;
    }
    let mut session_topics = BTreeSet::new();
    let mut session_identities = BTreeSet::new();
    for row in records(bundle, "assistant_sessions") {
        require_reference(
            "assistant_sessions",
            text(row, "user_id")?,
            "assistant_users",
            &assistant_users,
        )?;
        require_reference(
            "assistant_sessions",
            text(row, "conversation_id")?,
            "conversations",
            &conversations,
        )?;
        if let Some(workspace) = optional_text(row, "workspace")? {
            remap_workspace(workspace, source_path, local_path)?;
        }
        if let (Some(chat_id), Some(thread_id)) = (
            optional_text(row, "chat_id")?,
            optional_number(row, "message_thread_id")?,
        ) {
            session_topics.insert(format!("{chat_id}:{thread_id}"));
        }
        require_unique_key(
            &mut session_identities,
            "assistant_sessions identity",
            format!(
                "{}:{}:{}",
                text(row, "user_id")?,
                optional_text(row, "chat_id")?.unwrap_or(""),
                optional_number(row, "message_thread_id")?
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            ),
        )?;
    }
    let mut topic_bindings = BTreeSet::new();
    for row in records(bundle, "telegram_topic_bindings") {
        let topic = format!("{}:{}", text(row, "chat_id")?, number(row, "message_thread_id")?);
        require_unique_key(&mut topic_bindings, "telegram_topic_bindings", topic.clone())?;
        require_reference(
            "telegram_topic_bindings",
            &topic,
            "assistant_sessions topic",
            &session_topics,
        )?;
    }
    let mut model_overrides = BTreeSet::new();
    for row in records(bundle, "channel_topic_model_overrides") {
        let topic = format!("{}:{}", text(row, "chat_id")?, number(row, "message_thread_id")?);
        require_unique_key(
            &mut model_overrides,
            "channel_topic_model_overrides",
            format!("{}:{topic}", text(row, "platform")?),
        )?;
        require_reference(
            "channel_topic_model_overrides",
            &topic,
            "assistant_sessions topic",
            &session_topics,
        )?;
    }
    for row in records(bundle, "development_runs") {
        if let Some(team_id) = optional_text(row, "team_id")? {
            require_reference("development_runs", team_id, "teams", &teams)?;
        }
    }
    let mut delivery_runs = BTreeSet::new();
    for row in records(bundle, "development_deliveries") {
        let run_id = text(row, "run_id")?;
        require_unique_key(&mut delivery_runs, "development_deliveries run", run_id.to_owned())?;
        require_reference("development_deliveries", run_id, "development_runs", &runs)?;
    }
    for row in records(bundle, "development_audit_events") {
        if let Some(run_id) = optional_text(row, "run_id")? {
            require_reference("development_audit_events", run_id, "development_runs", &runs)?;
        }
    }
    Ok(())
}

async fn collect_id_conflicts(
    pool: &SqlitePool,
    bundle: &ProjectExportBundle,
    export_table: &str,
    database_table: &str,
) -> Result<Vec<String>, DevelopmentError> {
    let mut conflicts = Vec::new();
    let sql = format!("SELECT COUNT(*) FROM {database_table} WHERE id=?");
    for id in record_ids(bundle, export_table, "id")? {
        let exists: i64 = sqlx::query_scalar(&sql)
            .bind(&id)
            .fetch_one(pool)
            .await
            .map_err(internal)?;
        if exists != 0 {
            conflicts.push(format!("{export_table}:{id}"));
        }
    }
    Ok(conflicts)
}

async fn collect_import_conflicts(
    pool: &SqlitePool,
    owner_id: &str,
    project_id: &str,
    local_path: &str,
    bundle: &ProjectExportBundle,
) -> Result<Vec<String>, DevelopmentError> {
    let mut conflicts = Vec::new();
    let project_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id=? OR (user_id=? AND local_path=?)")
            .bind(project_id)
            .bind(owner_id)
            .bind(local_path)
            .fetch_one(pool)
            .await
            .map_err(internal)?;
    if project_exists != 0 {
        conflicts.push(format!("projects:{project_id}"));
    }
    for (export_table, database_table) in [
        ("conversations", "conversations"),
        ("messages", "messages"),
        ("teams", "teams"),
        ("assistant_sessions", "assistant_sessions"),
        ("development_policies", "development_policies"),
        ("development_runs", "development_runs"),
        ("development_deliveries", "development_deliveries"),
        ("development_audit_events", "development_audit_events"),
    ] {
        conflicts.extend(collect_id_conflicts(pool, bundle, export_table, database_table).await?);
    }
    let mut assistant_user_ids = BTreeMap::new();
    for row in records(bundle, "assistant_users") {
        let source_id = text(row, "id")?;
        let platform_user_id = text(row, "platform_user_id")?;
        let platform_type = text(row, "platform_type")?;
        let matching: Option<String> =
            sqlx::query_scalar("SELECT id FROM assistant_users WHERE platform_user_id=? AND platform_type=?")
                .bind(platform_user_id)
                .bind(platform_type)
                .fetch_optional(pool)
                .await
                .map_err(internal)?;
        match matching {
            Some(target_id) => {
                assistant_user_ids.insert(source_id.to_owned(), target_id);
            }
            None => conflicts.push(format!(
                "assistant_users:{platform_type}:{platform_user_id}:re_pair_required"
            )),
        }
    }
    for row in records(bundle, "assistant_sessions") {
        let source_user_id = text(row, "user_id")?;
        let Some(target_user_id) = assistant_user_ids.get(source_user_id) else {
            continue;
        };
        let chat_id = optional_text(row, "chat_id")?;
        let thread_id = optional_number(row, "message_thread_id")?;
        let matching: Option<String> = sqlx::query_scalar(
            "SELECT id FROM assistant_sessions \
             WHERE user_id=? AND chat_id IS ? AND message_thread_id IS ?",
        )
        .bind(target_user_id)
        .bind(chat_id)
        .bind(thread_id)
        .fetch_optional(pool)
        .await
        .map_err(internal)?;
        if let Some(existing_id) = matching {
            conflicts.push(format!("assistant_sessions:{existing_id}"));
        }
    }
    for row in records(bundle, "telegram_topic_bindings") {
        let chat_id = text(row, "chat_id")?;
        let thread_id = number(row, "message_thread_id")?;
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM telegram_topic_bindings WHERE chat_id=? AND message_thread_id=?")
                .bind(chat_id)
                .bind(thread_id)
                .fetch_one(pool)
                .await
                .map_err(internal)?;
        if exists != 0 {
            conflicts.push(format!("telegram_topic_bindings:{chat_id}:{thread_id}"));
        }
    }
    for row in records(bundle, "channel_topic_model_overrides") {
        let chat_id = text(row, "chat_id")?;
        let thread_id = number(row, "message_thread_id")?;
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM channel_topic_model_overrides \
             WHERE platform=? AND internal_user_id=? AND chat_id=? AND message_thread_id=?",
        )
        .bind(text(row, "platform")?)
        .bind(owner_id)
        .bind(chat_id)
        .bind(thread_id)
        .fetch_one(pool)
        .await
        .map_err(internal)?;
        if exists != 0 {
            conflicts.push(format!("channel_topic_model_overrides:{chat_id}:{thread_id}"));
        }
    }
    conflicts.sort();
    conflicts.dedup();
    Ok(conflicts)
}

async fn import_records(
    transaction: &mut Transaction<'_, Sqlite>,
    owner_id: &str,
    local_path: &str,
    source_path: &str,
    bundle: &ProjectExportBundle,
) -> Result<(), DevelopmentError> {
    let project = only_record(bundle, "projects")?;
    sqlx::query(
        "INSERT INTO projects (id,user_id,name,local_path,repository_url,default_branch,project_type,created_at,updated_at) \
         VALUES (?,?,?,?,?,?,?,?,?)",
    )
    .bind(text(project, "id")?)
    .bind(owner_id)
    .bind(text(project, "name")?)
    .bind(local_path)
    .bind(optional_text(project, "repository_url")?)
    .bind(optional_text(project, "default_branch")?)
    .bind(text(project, "project_type")?)
    .bind(number(project, "created_at")?)
    .bind(number(project, "updated_at")?)
    .execute(&mut **transaction)
    .await
    .map_err(internal)?;

    let mut assistant_user_ids = BTreeMap::new();
    for row in records(bundle, "assistant_users") {
        let source_id = text(row, "id")?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM assistant_users WHERE platform_user_id=? AND platform_type=?")
                .bind(text(row, "platform_user_id")?)
                .bind(text(row, "platform_type")?)
                .fetch_optional(&mut **transaction)
                .await
                .map_err(internal)?;
        let target_id = existing.ok_or_else(|| {
            DevelopmentError::BadRequest(format!(
                "assistant user {source_id} must be paired on the target instance before import"
            ))
        })?;
        assistant_user_ids.insert(source_id.to_owned(), target_id);
    }

    for row in records(bundle, "project_command_profiles") {
        sqlx::query(
            "INSERT INTO project_command_profiles (project_id,install_command,format_command,lint_command,typecheck_command,\
             unit_test_command,integration_test_command,e2e_command,build_command,security_scan_command,command_timeout_seconds,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "project_id")?)
        .bind(optional_text(row, "install_command")?)
        .bind(optional_text(row, "format_command")?)
        .bind(optional_text(row, "lint_command")?)
        .bind(optional_text(row, "typecheck_command")?)
        .bind(optional_text(row, "unit_test_command")?)
        .bind(optional_text(row, "integration_test_command")?)
        .bind(optional_text(row, "e2e_command")?)
        .bind(optional_text(row, "build_command")?)
        .bind(optional_text(row, "security_scan_command")?)
        .bind(number(row, "command_timeout_seconds")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "project_runtime_profiles") {
        sqlx::query(
            "INSERT INTO project_runtime_profiles (project_id,environment_kind,language,package_manager,runtime_version,env_keys,metadata,updated_at) \
             VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "project_id")?)
        .bind(text(row, "environment_kind")?)
        .bind(optional_text(row, "language")?)
        .bind(optional_text(row, "package_manager")?)
        .bind(optional_text(row, "runtime_version")?)
        .bind(text(row, "env_keys")?)
        .bind(text(row, "metadata")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "conversations") {
        sqlx::query(
            "INSERT INTO conversations (id,user_id,name,type,extra,model,status,source,channel_chat_id,pinned,pinned_at,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(owner_id)
        .bind(text(row, "name")?)
        .bind(text(row, "type")?)
        .bind(text(row, "extra")?)
        .bind(optional_text(row, "model")?)
        .bind(text(row, "status")?)
        .bind(optional_text(row, "source")?)
        .bind(optional_text(row, "channel_chat_id")?)
        .bind(number(row, "pinned")?)
        .bind(optional_number(row, "pinned_at")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "messages") {
        sqlx::query(
            "INSERT INTO messages (id,conversation_id,msg_id,type,content,position,status,hidden,created_at) VALUES (?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(text(row, "conversation_id")?)
        .bind(optional_text(row, "msg_id")?)
        .bind(text(row, "type")?)
        .bind(text(row, "content")?)
        .bind(optional_text(row, "position")?)
        .bind(optional_text(row, "status")?)
        .bind(number(row, "hidden")?)
        .bind(number(row, "created_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "teams") {
        let workspace = remap_workspace(text(row, "workspace")?, source_path, local_path)?;
        sqlx::query(
            "INSERT INTO teams (id,user_id,name,workspace,workspace_mode,agents,lead_agent_id,session_mode,agents_version,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(owner_id)
        .bind(text(row, "name")?)
        .bind(&workspace)
        .bind(text(row, "workspace_mode")?)
        .bind(text(row, "agents")?)
        .bind(optional_text(row, "lead_agent_id")?)
        .bind(optional_text(row, "session_mode")?)
        .bind(text(row, "agents_version")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "project_resource_links") {
        sqlx::query(
            "INSERT INTO project_resource_links (project_id,user_id,resource_type,resource_id,created_at) VALUES (?,?,?,?,?)",
        )
        .bind(text(row, "project_id")?)
        .bind(owner_id)
        .bind(text(row, "resource_type")?)
        .bind(text(row, "resource_id")?)
        .bind(number(row, "created_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "telegram_topic_bindings") {
        sqlx::query(
            "INSERT INTO telegram_topic_bindings (chat_id,message_thread_id,agent_id,bound_by_user_id,created_at,updated_at) \
             VALUES (?,?,?,?,?,?)",
        )
        .bind(text(row, "chat_id")?)
        .bind(number(row, "message_thread_id")?)
        .bind(text(row, "agent_id")?)
        .bind(text(row, "bound_by_user_id")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "channel_topic_model_overrides") {
        sqlx::query(
            "INSERT INTO channel_topic_model_overrides (platform,internal_user_id,chat_id,message_thread_id,agent_id,provider_id,model,updated_at) \
             VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "platform")?)
        .bind(owner_id)
        .bind(text(row, "chat_id")?)
        .bind(number(row, "message_thread_id")?)
        .bind(text(row, "agent_id")?)
        .bind(text(row, "provider_id")?)
        .bind(text(row, "model")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "development_policies") {
        sqlx::query(
            "INSERT INTO development_policies (id,user_id,project_id,isolation_mode,container_image,devcontainer_config_path,\
             container_cpu_millis,container_memory_mb,container_pids_limit,network_mode,allowed_secret_keys_json,\
             allowed_commands_json,protected_paths_json,allowed_network_hosts_json,protected_branches_json,\
             dangerous_confirmation_count,max_duration_ms,max_parallel_agents,max_retries,max_cost_microunits,\
             max_total_tokens,fallback_model,alert_percent,over_limit_action,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(owner_id)
        .bind(text(row, "project_id")?)
        .bind(text(row, "isolation_mode")?)
        .bind(optional_text(row, "container_image")?)
        .bind(optional_text(row, "devcontainer_config_path")?)
        .bind(number(row, "container_cpu_millis")?)
        .bind(number(row, "container_memory_mb")?)
        .bind(number(row, "container_pids_limit")?)
        .bind(text(row, "network_mode")?)
        .bind(text(row, "allowed_secret_keys_json")?)
        .bind(text(row, "allowed_commands_json")?)
        .bind(text(row, "protected_paths_json")?)
        .bind(text(row, "allowed_network_hosts_json")?)
        .bind(text(row, "protected_branches_json")?)
        .bind(number(row, "dangerous_confirmation_count")?)
        .bind(number(row, "max_duration_ms")?)
        .bind(number(row, "max_parallel_agents")?)
        .bind(number(row, "max_retries")?)
        .bind(number(row, "max_cost_microunits")?)
        .bind(number(row, "max_total_tokens")?)
        .bind(optional_text(row, "fallback_model")?)
        .bind(number(row, "alert_percent")?)
        .bind(text(row, "over_limit_action")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
            .map_err(internal)?;
    }
    for row in records(bundle, "assistant_sessions") {
        let source_user_id = text(row, "user_id")?;
        let target_user_id = assistant_user_ids.get(source_user_id).ok_or_else(|| {
            DevelopmentError::BadRequest(format!(
                "import validation failed: assistant session user {source_user_id}"
            ))
        })?;
        let workspace = optional_text(row, "workspace")?
            .map(|path| remap_workspace(path, source_path, local_path))
            .transpose()?;
        sqlx::query(
            "INSERT INTO assistant_sessions \
             (id,user_id,agent_type,conversation_id,workspace,chat_id,message_thread_id,bound_agent_id,\
              bound_backend,bound_provider_id,bound_model,created_at,last_activity) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(target_user_id)
        .bind(text(row, "agent_type")?)
        .bind(text(row, "conversation_id")?)
        .bind(workspace)
        .bind(optional_text(row, "chat_id")?)
        .bind(optional_number(row, "message_thread_id")?)
        .bind(optional_text(row, "bound_agent_id")?)
        .bind(optional_text(row, "bound_backend")?)
        .bind(optional_text(row, "bound_provider_id")?)
        .bind(optional_text(row, "bound_model")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "last_activity")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "development_retention_policies") {
        sqlx::query(
            "INSERT INTO development_retention_policies \
             (user_id,project_id,conversation_history_days,artifact_days,evaluation_days,immutable_audit_log,updated_at) \
             VALUES (?,?,?,?,?,?,?)",
        )
        .bind(owner_id)
        .bind(text(row, "project_id")?)
        .bind(number(row, "conversation_history_days")?)
        .bind(number(row, "artifact_days")?)
        .bind(number(row, "evaluation_days")?)
        .bind(number(row, "immutable_audit_log")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "development_runs") {
        sqlx::query(
            "INSERT INTO development_runs (id,user_id,project_id,team_id,source_channel,source_user_id,execution_mode,status,\
             request_summary,acceptance_criteria,baseline_commit,integration_branch,started_at,finished_at,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(owner_id)
        .bind(text(row, "project_id")?)
        .bind(optional_text(row, "team_id")?)
        .bind(optional_text(row, "source_channel")?)
        .bind(optional_text(row, "source_user_id")?)
        .bind(text(row, "execution_mode")?)
        .bind(text(row, "status")?)
        .bind(text(row, "request_summary")?)
        .bind(text(row, "acceptance_criteria")?)
        .bind(optional_text(row, "baseline_commit")?)
        .bind(optional_text(row, "integration_branch")?)
        .bind(optional_number(row, "started_at")?)
        .bind(optional_number(row, "finished_at")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "development_deliveries") {
        sqlx::query(
            "INSERT INTO development_deliveries (id,run_id,project_id,user_id,provider,repository,branch,base_branch,commit_sha,\
             status,push_status,pr_number,pr_url,pr_status,ci_status,review_status,merge_status,report_json,last_error,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(text(row, "run_id")?)
        .bind(text(row, "project_id")?)
        .bind(owner_id)
        .bind(text(row, "provider")?)
        .bind(optional_text(row, "repository")?)
        .bind(text(row, "branch")?)
        .bind(text(row, "base_branch")?)
        .bind(optional_text(row, "commit_sha")?)
        .bind(text(row, "status")?)
        .bind(text(row, "push_status")?)
        .bind(optional_number(row, "pr_number")?)
        .bind(optional_text(row, "pr_url")?)
        .bind(text(row, "pr_status")?)
        .bind(text(row, "ci_status")?)
        .bind(text(row, "review_status")?)
        .bind(text(row, "merge_status")?)
        .bind(text(row, "report_json")?)
        .bind(optional_text(row, "last_error")?)
        .bind(number(row, "created_at")?)
        .bind(number(row, "updated_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    for row in records(bundle, "development_audit_events") {
        sqlx::query(
            "INSERT INTO development_audit_events (id,user_id,actor_type,actor_id,action,target_type,target_id,project_id,\
             run_id,task_id,result,redacted_payload_json,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(text(row, "id")?)
        .bind(owner_id)
        .bind(text(row, "actor_type")?)
        .bind(if text(row, "actor_type")? == "user" {
            owner_id
        } else {
            text(row, "actor_id")?
        })
        .bind(text(row, "action")?)
        .bind(text(row, "target_type")?)
        .bind(text(row, "target_id")?)
        .bind(text(row, "project_id")?)
        .bind(optional_text(row, "run_id")?)
        .bind(Option::<String>::None)
        .bind(text(row, "result")?)
        .bind(text(row, "redacted_payload_json")?)
        .bind(number(row, "created_at")?)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    }
    Ok(())
}

fn validate_evaluation(input: &EvaluationRecordInput) -> Result<(), DevelopmentError> {
    if input.project_id.trim().is_empty()
        || input.release_id.trim().is_empty()
        || input.scenario_id.trim().is_empty()
        || !matches!(input.result.as_str(), "passed" | "failed" | "error" | "skipped")
        || input.duration_ms < 0
        || input.input_tokens < 0
        || input.output_tokens < 0
        || input.cost_microunits < 0
        || input.cost_source.trim().is_empty()
    {
        return Err(DevelopmentError::BadRequest("invalid evaluation record".into()));
    }
    Ok(())
}

async fn evaluation_row(
    pool: &SqlitePool,
    sql: &str,
    user_id: &str,
    project_id: &str,
    release_id: &str,
    scenario_id: &str,
) -> Result<Option<DevelopmentEvaluation>, DevelopmentError> {
    let row = sqlx::query(sql)
        .bind(user_id)
        .bind(project_id)
        .bind(release_id)
        .bind(scenario_id)
        .fetch_optional(pool)
        .await
        .map_err(internal)?;
    row.map(|row| {
        Ok(DevelopmentEvaluation {
            id: row.try_get("id").map_err(internal)?,
            user_id: row.try_get("user_id").map_err(internal)?,
            project_id: row.try_get("project_id").map_err(internal)?,
            release_id: row.try_get("release_id").map_err(internal)?,
            scenario_id: row.try_get("scenario_id").map_err(internal)?,
            result: row.try_get("result").map_err(internal)?,
            duration_ms: row.try_get("duration_ms").map_err(internal)?,
            failure_category: row.try_get("failure_category").map_err(internal)?,
            input_tokens: row.try_get("input_tokens").map_err(internal)?,
            output_tokens: row.try_get("output_tokens").map_err(internal)?,
            cost_microunits: row.try_get("cost_microunits").map_err(internal)?,
            cost_source: row.try_get("cost_source").map_err(internal)?,
            accepted_baseline: row.try_get("accepted_baseline").map_err(internal)?,
            created_at: row.try_get("created_at").map_err(internal)?,
        })
    })
    .transpose()
}

fn exceeds_percent(current: i64, baseline: i64, allowed_percent: i64) -> bool {
    if baseline == 0 {
        return current > 0;
    }
    let allowed = i128::from(baseline) * i128::from(100 + allowed_percent) / 100;
    i128::from(current) > allowed
}

fn regression(scenario: &str, category: &str, message: &str) -> EvaluationRegression {
    EvaluationRegression {
        scenario_id: scenario.into(),
        category: category.into(),
        message: message.into(),
    }
}

fn internal(error: impl std::fmt::Display) -> DevelopmentError {
    DevelopmentError::Internal(error.to_string())
}
