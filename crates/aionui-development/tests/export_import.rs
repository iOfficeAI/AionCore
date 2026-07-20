use aionui_db::init_database_memory;
use aionui_development::{
    EvaluationComparisonRequest, EvaluationRecordInput, ImportProjectBundleRequest, PortabilityService,
    RetentionCleanupRequest, RetentionPolicyInput, RetentionService,
};

async fn seed_source(pool: &aionui_db::SqlitePool) {
    sqlx::query(
        "INSERT INTO projects (id,user_id,name,local_path,repository_url,default_branch,project_type,created_at,updated_at) \
         VALUES ('project-export','system_default_user','Portable','/source/project','https://example.test/repo.git','main','single',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO project_command_profiles \
         (project_id,unit_test_command,command_timeout_seconds,updated_at) VALUES ('project-export','cargo test',900,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,source,channel_chat_id,created_at,updated_at) \
         VALUES ('conversation-export','system_default_user','History','codex','{}','finished','telegram','chat-1',1,2)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id,conversation_id,type,content,position,status,created_at) \
         VALUES ('message-export','conversation-export','text','{\"text\":\"kept\"}','right','finish',2)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO teams (id,user_id,name,workspace,workspace_mode,agents,created_at,updated_at) \
        VALUES ('team-export','system_default_user','Portable Team','/source/project/packages/api','shared','[]',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    for (kind, id) in [("conversation", "conversation-export"), ("team", "team-export")] {
        sqlx::query(
            "INSERT INTO project_resource_links (project_id,user_id,resource_type,resource_id,created_at) \
             VALUES ('project-export','system_default_user',?,?,1)",
        )
        .bind(kind)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO assistant_users (id,platform_user_id,platform_type,authorized_at) \
         VALUES ('telegram-user','42','telegram',1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_sessions \
         (id,user_id,agent_type,conversation_id,chat_id,message_thread_id,bound_agent_id,bound_backend,\
          bound_provider_id,bound_model,created_at,last_activity) \
         VALUES ('session-export','telegram-user','codex','conversation-export','chat-1',3,'codex-agent',\
                 'codex','provider-main','claude',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_topic_model_overrides \
         (platform,internal_user_id,chat_id,message_thread_id,agent_id,provider_id,model,updated_at) \
         VALUES ('telegram','system_default_user','chat-1',3,'codex-agent','provider-main','claude',1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id,username,password_hash,created_at,updated_at) \
         VALUES ('other-source-user','other-source-user','disabled',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_topic_model_overrides \
         (platform,internal_user_id,chat_id,message_thread_id,agent_id,provider_id,model,updated_at) \
         VALUES ('telegram','other-source-user','chat-1',3,'other-agent','other-provider','private-model',1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO telegram_topic_bindings \
         (chat_id,message_thread_id,agent_id,bound_by_user_id,created_at,updated_at) \
         VALUES ('chat-1',3,'codex','42',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_policies \
         (id,user_id,project_id,isolation_mode,created_at,updated_at) \
         VALUES ('policy-export','system_default_user','project-export','host',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_retention_policies \
         (user_id,project_id,conversation_history_days,artifact_days,evaluation_days,immutable_audit_log,updated_at) \
         VALUES ('system_default_user','project-export',120,60,180,1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_runs \
         (id,user_id,project_id,execution_mode,status,request_summary,acceptance_criteria,created_at,updated_at) \
         VALUES ('run-export','system_default_user','project-export','single','succeeded','Portable run','[]',1,2)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_deliveries \
         (id,run_id,project_id,user_id,branch,base_branch,status,created_at,updated_at) \
         VALUES ('delivery-export','run-export','project-export','system_default_user','feature','main','merged',1,2)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_audit_events \
         (id,user_id,actor_type,actor_id,action,target_type,target_id,project_id,run_id,result,redacted_payload_json,created_at) \
         VALUES ('audit-export','system_default_user','user','system_default_user','portable.test','run','run-export',\
                 'project-export','run-export','success','{}',2)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_secrets \
         (id,user_id,project_id,name,encrypted_value,status,created_at,updated_at) \
         VALUES ('secret-export','system_default_user','project-export','TOKEN','ciphertext-must-not-export','active',1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn signed_project_bundle_round_trips_with_owner_and_path_remapping_without_secrets() {
    let source = init_database_memory().await.unwrap();
    seed_source(source.pool()).await;
    let exporter = PortabilityService::new(source.pool().clone(), b"shared-portability-key", "source-instance");
    let bundle = exporter
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();

    assert_eq!(bundle.manifest.format_version, 1);
    assert!(!bundle.manifest.source_instance_id.is_empty());
    assert!(bundle.records.contains_key("conversations"));
    assert!(bundle.records.contains_key("messages"));
    assert!(bundle.records.contains_key("teams"));
    assert!(bundle.records.contains_key("telegram_topic_bindings"));
    assert_eq!(bundle.records["development_retention_policies"].len(), 1);
    let encoded = serde_json::to_string(&bundle).unwrap();
    assert!(!encoded.contains("development_secrets"));
    assert!(!encoded.contains("ciphertext-must-not-export"));
    assert!(!encoded.contains("private-model"));
    assert_eq!(bundle.records["channel_topic_model_overrides"].len(), 1);
    exporter.validate_bundle(&bundle).unwrap();

    let target = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('restored-owner','restored-owner','disabled',1,1)",
    )
    .execute(target.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_users (id,platform_user_id,platform_type,authorized_at) \
         VALUES ('target-telegram-user','42','telegram',1)",
    )
    .execute(target.pool())
    .await
    .unwrap();
    let mut importer = PortabilityService::new(target.pool().clone(), b"different-target-key", "target-instance");
    assert!(
        importer
            .validate_bundle(&bundle)
            .unwrap_err()
            .to_string()
            .contains("not trusted")
    );
    importer.trust_signer(&exporter.signer_public_key()).unwrap();
    let report = importer
        .import_project(
            "restored-owner",
            ImportProjectBundleRequest {
                bundle: bundle.clone(),
                local_path: "/restored/project".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(report.project_id, "project-export");
    assert_eq!(report.owner_id, "restored-owner");
    assert!(report.imported);
    assert!(report.conflicts.is_empty());

    let restored_owner: String = sqlx::query_scalar("SELECT user_id FROM projects WHERE id='project-export'")
        .fetch_one(target.pool())
        .await
        .unwrap();
    let restored_path: String = sqlx::query_scalar("SELECT local_path FROM projects WHERE id='project-export'")
        .fetch_one(target.pool())
        .await
        .unwrap();
    assert_eq!(restored_owner, "restored-owner");
    assert_eq!(restored_path, "/restored/project");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT workspace FROM teams WHERE id='team-export'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        "/restored/project/packages/api"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE id='message-export'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM telegram_topic_bindings WHERE chat_id='chat-1'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM development_audit_events WHERE id='audit-export'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT conversation_history_days FROM development_retention_policies \
             WHERE user_id='restored-owner' AND project_id='project-export'",
        )
        .fetch_one(target.pool())
        .await
        .unwrap(),
        120
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM assistant_sessions WHERE conversation_id='conversation-export' \
             AND user_id='target-telegram-user' AND chat_id='chat-1' AND message_thread_id=3",
        )
        .fetch_one(target.pool())
        .await
        .unwrap(),
        1
    );
    let restored_binding: (Option<String>, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT bound_agent_id,bound_backend,bound_provider_id,bound_model \
         FROM assistant_sessions WHERE id='session-export'",
    )
    .fetch_one(target.pool())
    .await
    .unwrap();
    assert_eq!(
        restored_binding,
        (
            Some("codex-agent".into()),
            Some("codex".into()),
            Some("provider-main".into()),
            Some("claude".into())
        )
    );
    let reexported = importer
        .export_project("restored-owner", "project-export")
        .await
        .unwrap();
    assert_eq!(reexported.records["assistant_sessions"].len(), 1);
    assert_eq!(reexported.records["telegram_topic_bindings"].len(), 1);
    assert_eq!(
        reexported.records["assistant_sessions"][0]["bound_agent_id"],
        "codex-agent"
    );
    assert_eq!(reexported.records["assistant_sessions"][0]["bound_model"], "claude");

    let conflict = importer
        .import_project(
            "restored-owner",
            ImportProjectBundleRequest {
                bundle,
                local_path: "/restored/project".into(),
            },
        )
        .await
        .unwrap();
    assert!(!conflict.imported);
    assert!(conflict.conflicts.iter().any(|item| item == "projects:project-export"));
}

#[tokio::test]
async fn import_rejects_tampering_future_versions_path_traversal_and_rolls_back() {
    let source = init_database_memory().await.unwrap();
    seed_source(source.pool()).await;
    let exporter = PortabilityService::new(source.pool().clone(), b"shared-portability-key", "source-instance");
    let bundle = exporter
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();
    let target = init_database_memory().await.unwrap();
    let importer = PortabilityService::new(target.pool().clone(), b"shared-portability-key", "target-instance");

    let mut tampered = bundle.clone();
    tampered.records.get_mut("projects").unwrap()[0]["name"] = "tampered".into();
    assert!(
        importer
            .validate_bundle(&tampered)
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );

    let mut tampered_manifest = bundle.clone();
    tampered_manifest.manifest.app_version = "forged-version".into();
    assert!(
        importer
            .validate_bundle(&tampered_manifest)
            .unwrap_err()
            .to_string()
            .contains("signature")
    );

    let mut future = bundle.clone();
    future.manifest.format_version = 99;
    assert!(
        importer
            .validate_bundle(&future)
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );

    let traversal = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle: bundle.clone(),
                local_path: "/restored/../escape".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(traversal.to_string().contains("path traversal"));

    let mut invalid = bundle;
    invalid.records.get_mut("development_deliveries").unwrap()[0]["run_id"] = "missing-run".into();
    exporter.seal_bundle(&mut invalid).unwrap();
    let error = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle: invalid,
                local_path: "/restored/transaction".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("validation"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects WHERE id='project-export'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        0
    );

    let mut outside = exporter
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();
    outside.records.get_mut("teams").unwrap()[0]["workspace"] = "/unrelated/workspace".into();
    exporter.seal_bundle(&mut outside).unwrap();
    let error = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle: outside,
                local_path: "/restored/project".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside source project"));
}

#[tokio::test]
async fn platform_instance_is_stable_and_release_gate_detects_historical_regressions() {
    let database = init_database_memory().await.unwrap();
    seed_source(database.pool()).await;
    let service = PortabilityService::new(database.pool().clone(), b"evaluation-key", "runtime-instance");
    let first = service.platform_instance().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let second = PortabilityService::new(database.pool().clone(), b"evaluation-key", "other-runtime")
        .platform_instance()
        .await
        .unwrap();
    assert_eq!(first.instance_id, second.instance_id);
    assert_eq!(first.last_started_at, second.last_started_at);
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    service.record_startup().await.unwrap();
    let after_startup = service.platform_instance().await.unwrap();
    assert!(after_startup.last_started_at > second.last_started_at);
    assert_eq!(first.schema_version, 34);
    assert!(first.data_size_bytes > 0);

    service
        .record_evaluation(
            "system_default_user",
            EvaluationRecordInput {
                project_id: "project-export".into(),
                release_id: "release-baseline".into(),
                scenario_id: "codex-smoke".into(),
                result: "passed".into(),
                duration_ms: 1_000,
                failure_category: None,
                input_tokens: 100,
                output_tokens: 50,
                cost_microunits: 1_000,
                cost_source: "provider".into(),
                accepted_baseline: true,
            },
        )
        .await
        .unwrap();
    service
        .record_evaluation(
            "system_default_user",
            EvaluationRecordInput {
                project_id: "project-export".into(),
                release_id: "release-current".into(),
                scenario_id: "codex-smoke".into(),
                result: "passed".into(),
                duration_ms: 1_500,
                failure_category: None,
                input_tokens: 100,
                output_tokens: 50,
                cost_microunits: 1_400,
                cost_source: "provider".into(),
                accepted_baseline: false,
            },
        )
        .await
        .unwrap();

    let comparison = service
        .compare_evaluations(
            "system_default_user",
            EvaluationComparisonRequest {
                project_id: "project-export".into(),
                release_id: "release-current".into(),
                required_scenarios: vec!["codex-smoke".into(), "claude-smoke".into()],
                max_duration_regression_percent: 20,
                max_cost_regression_percent: 20,
            },
        )
        .await
        .unwrap();
    assert!(!comparison.allowed);
    assert!(comparison.regressions.iter().any(|item| item.category == "duration"));
    assert!(comparison.regressions.iter().any(|item| item.category == "cost"));
    assert!(comparison.regressions.iter().any(|item| item.category == "missing"));
}

#[tokio::test]
async fn retention_policy_previews_and_requires_confirmation_before_project_scoped_cleanup() {
    let database = init_database_memory().await.unwrap();
    seed_source(database.pool()).await;
    let now = aionui_common::now_ms();
    let old = now - 400 * 24 * 60 * 60 * 1_000;
    sqlx::query("UPDATE messages SET created_at=? WHERE id='message-export'")
        .bind(now)
        .execute(database.pool())
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO messages (id,conversation_id,type,content,position,status,created_at) \
         VALUES ('message-old','conversation-export','text','{\"text\":\"expired\"}','right','finish',?)",
    )
    .bind(old)
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_artifacts \
         (id,run_id,artifact_type,path_or_uri,checksum,created_at) \
         VALUES ('artifact-old','run-export','log','/source/project/.aion/old.log','sha256:old',?)",
    )
    .bind(old)
    .execute(database.pool())
    .await
    .unwrap();
    for (id, baseline) in [("evaluation-old", 0), ("evaluation-baseline", 1)] {
        sqlx::query(
            "INSERT INTO development_evaluations \
             (id,user_id,project_id,release_id,scenario_id,result,duration_ms,input_tokens,output_tokens,\
              cost_microunits,cost_source,accepted_baseline,created_at) \
             VALUES (?,'system_default_user','project-export',?,'retention','passed',1,0,0,0,'test',?,?)",
        )
        .bind(id)
        .bind(id)
        .bind(baseline)
        .bind(old)
        .execute(database.pool())
        .await
        .unwrap();
    }

    let service = RetentionService::new(database.pool().clone());
    let policy = service
        .update_policy(
            "system_default_user",
            "project-export",
            RetentionPolicyInput {
                conversation_history_days: 30,
                artifact_days: 30,
                evaluation_days: 30,
            },
        )
        .await
        .unwrap();
    assert!(policy.immutable_audit_log);

    let preview = service
        .cleanup(
            "system_default_user",
            "project-export",
            RetentionCleanupRequest {
                dry_run: true,
                confirmation_count: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(preview.message_count, 1);
    assert_eq!(preview.artifact_count, 1);
    assert_eq!(preview.evaluation_count, 1);
    assert_eq!(preview.audit_events_retained, 1);

    let rejected = service
        .cleanup(
            "system_default_user",
            "project-export",
            RetentionCleanupRequest {
                dry_run: false,
                confirmation_count: 1,
            },
        )
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("confirmation"));

    let applied = service
        .cleanup(
            "system_default_user",
            "project-export",
            RetentionCleanupRequest {
                dry_run: false,
                confirmation_count: 2,
            },
        )
        .await
        .unwrap();
    assert!(!applied.dry_run);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE id IN ('message-old','message-export')",)
            .fetch_one(database.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM development_evaluations WHERE scenario_id='retention'",)
            .fetch_one(database.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM development_audit_events WHERE project_id='project-export'",
        )
        .fetch_one(database.pool())
        .await
        .unwrap(),
        2
    );

    let bundle = PortabilityService::new(database.pool().clone(), b"retention-key", "source-instance")
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();
    assert_eq!(bundle.records["development_retention_policies"].len(), 1);
}

#[tokio::test]
async fn retention_policy_rejects_invalid_ranges_and_cross_owner_access() {
    let database = init_database_memory().await.unwrap();
    seed_source(database.pool()).await;
    let service = RetentionService::new(database.pool().clone());
    let invalid = service
        .update_policy(
            "system_default_user",
            "project-export",
            RetentionPolicyInput {
                conversation_history_days: 0,
                artifact_days: 30,
                evaluation_days: 30,
            },
        )
        .await
        .unwrap_err();
    assert!(invalid.to_string().contains("between 1 and 3650"));
    assert!(
        service
            .get_policy("other-user", "project-export")
            .await
            .unwrap_err()
            .to_string()
            .contains("project project-export")
    );
}

#[tokio::test]
async fn import_reports_secondary_conflicts_before_writing_any_rows() {
    let source = init_database_memory().await.unwrap();
    seed_source(source.pool()).await;
    let exporter = PortabilityService::new(source.pool().clone(), b"source-key", "source-instance");
    let bundle = exporter
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();
    let target = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at) \
         VALUES ('conversation-export','system_default_user','Conflict','codex','{}','finished',1,1)",
    )
    .execute(target.pool())
    .await
    .unwrap();
    let mut importer = PortabilityService::new(target.pool().clone(), b"target-key", "target-instance");
    importer.trust_signer(&exporter.signer_public_key()).unwrap();
    let report = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle,
                local_path: "/restored/project".into(),
            },
        )
        .await
        .unwrap();
    assert!(!report.imported);
    assert!(
        report
            .conflicts
            .iter()
            .any(|item| item == "conversations:conversation-export")
    );
    assert!(
        report
            .conflicts
            .iter()
            .any(|item| item == "assistant_users:telegram:42:re_pair_required")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects WHERE id='project-export'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn import_preflights_logical_session_conflicts_and_duplicate_portable_keys() {
    let source = init_database_memory().await.unwrap();
    seed_source(source.pool()).await;
    let exporter = PortabilityService::new(source.pool().clone(), b"source-key", "source-instance");
    let bundle = exporter
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();
    let target = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO assistant_users (id,platform_user_id,platform_type,authorized_at) \
         VALUES ('target-user','42','telegram',1)",
    )
    .execute(target.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at) \
         VALUES ('target-conversation','system_default_user','Existing','codex','{}','finished',1,1)",
    )
    .execute(target.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_sessions \
         (id,user_id,agent_type,conversation_id,chat_id,message_thread_id,created_at,last_activity) \
         VALUES ('target-session','target-user','codex','target-conversation','chat-1',3,1,1)",
    )
    .execute(target.pool())
    .await
    .unwrap();
    let mut importer = PortabilityService::new(target.pool().clone(), b"target-key", "target-instance");
    importer.trust_signer(&exporter.signer_public_key()).unwrap();
    let report = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle: bundle.clone(),
                local_path: "/restored/project".into(),
            },
        )
        .await
        .unwrap();
    assert!(!report.imported);
    assert!(
        report
            .conflicts
            .iter()
            .any(|item| item == "assistant_sessions:target-session")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects WHERE id='project-export'")
            .fetch_one(target.pool())
            .await
            .unwrap(),
        0
    );

    let mut duplicate = bundle;
    let duplicate_profile = duplicate.records["project_command_profiles"][0].clone();
    duplicate
        .records
        .get_mut("project_command_profiles")
        .unwrap()
        .push(duplicate_profile);
    exporter.seal_bundle(&mut duplicate).unwrap();
    let error = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle: duplicate,
                local_path: "/restored/duplicate".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("duplicate project_command_profiles"));
}

#[cfg(unix)]
#[tokio::test]
async fn import_rejects_existing_symbolic_links_in_remapped_workspace_path() {
    let source = init_database_memory().await.unwrap();
    seed_source(source.pool()).await;
    let exporter = PortabilityService::new(source.pool().clone(), b"source-key", "source-instance");
    let bundle = exporter
        .export_project("system_default_user", "project-export")
        .await
        .unwrap();
    let target = init_database_memory().await.unwrap();
    let mut importer = PortabilityService::new(target.pool().clone(), b"target-key", "target-instance");
    importer.trust_signer(&exporter.signer_public_key()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target_project = directory.path().join("project");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&target_project).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, target_project.join("packages")).unwrap();

    let error = importer
        .import_project(
            "system_default_user",
            ImportProjectBundleRequest {
                bundle,
                local_path: target_project.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("symbolic link"));
}

#[tokio::test]
async fn evaluation_baseline_is_one_passing_release_and_never_mixes_scenarios() {
    let database = init_database_memory().await.unwrap();
    seed_source(database.pool()).await;
    let service = PortabilityService::new(database.pool().clone(), b"evaluation-key", "runtime-instance");
    let input = |release: &str, scenario: &str, result: &str, baseline: bool| EvaluationRecordInput {
        project_id: "project-export".into(),
        release_id: release.into(),
        scenario_id: scenario.into(),
        result: result.into(),
        duration_ms: 100,
        failure_category: None,
        input_tokens: 1,
        output_tokens: 1,
        cost_microunits: 1,
        cost_source: "test".into(),
        accepted_baseline: baseline,
    };
    service
        .record_evaluation(
            "system_default_user",
            input("baseline-a", "codex-smoke", "passed", true),
        )
        .await
        .unwrap();
    service
        .record_evaluation(
            "system_default_user",
            input("baseline-b", "claude-smoke", "passed", true),
        )
        .await
        .unwrap();
    service
        .record_evaluation(
            "system_default_user",
            input("candidate", "codex-smoke", "passed", false),
        )
        .await
        .unwrap();
    service
        .record_evaluation(
            "system_default_user",
            input("candidate", "claude-smoke", "passed", false),
        )
        .await
        .unwrap();
    let comparison = service
        .compare_evaluations(
            "system_default_user",
            EvaluationComparisonRequest {
                project_id: "project-export".into(),
                release_id: "candidate".into(),
                required_scenarios: vec!["codex-smoke".into(), "claude-smoke".into()],
                max_duration_regression_percent: 20,
                max_cost_regression_percent: 20,
            },
        )
        .await
        .unwrap();
    assert!(!comparison.allowed);
    assert_eq!(comparison.baseline_release_ids, vec!["baseline-b"]);
    assert!(
        comparison
            .regressions
            .iter()
            .any(|item| item.scenario_id == "codex-smoke" && item.category == "baseline_missing")
    );

    let rejected = service
        .record_evaluation(
            "system_default_user",
            input("bad-baseline", "codex-smoke", "failed", true),
        )
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("passing evaluation"));
}
