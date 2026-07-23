use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::ProjectRow;
use aionui_db::{
    IProjectRepository, SqliteDevelopmentOperationsRepository, SqliteProjectRepository, init_database_memory,
};
use aionui_development::{
    DevelopmentPolicyRules, PolicyDecision, PolicyEngine, PolicyOperation, SecretAccessContext, SecretCreateInput,
    SecretGrantInput, SecretRedactor, SecretReferenceRequest, SecretService,
};

async fn setup() -> (SecretService, aionui_db::Database, tempfile::TempDir) {
    let db = init_database_memory().await.unwrap();
    for (id, name) in [("user-secret", "secret-owner"), ("user-other", "other-owner")] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, '', 1, 1)")
            .bind(id)
            .bind(name)
            .execute(db.pool())
            .await
            .unwrap();
    }
    let workspace = tempfile::tempdir().unwrap();
    let projects = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    projects
        .create(&ProjectRow {
            id: "project-secret".into(),
            user_id: "user-secret".into(),
            name: "Secret Project".into(),
            local_path: workspace.path().to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let operations = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    (
        SecretService::new(operations, projects, Arc::new([7_u8; 32])),
        db,
        workspace,
    )
}

#[tokio::test]
async fn secret_references_are_encrypted_owner_scoped_granted_and_revocable() {
    let (service, db, _workspace) = setup().await;
    let secret = service
        .create(
            "user-secret",
            "project-secret",
            SecretCreateInput {
                name: "GitHub token".into(),
                value: "ghp_plaintext_must_never_escape".into(),
                expires_at: Some(now_ms() + 60_000),
            },
        )
        .await
        .unwrap();
    let public_json = serde_json::to_string(&secret).unwrap();
    assert!(!public_json.contains("plaintext"));
    assert!(!public_json.contains("ciphertext"));

    let encrypted: String = sqlx::query_scalar("SELECT encrypted_value FROM development_secrets WHERE id = ?")
        .bind(&secret.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!encrypted.contains("ghp_plaintext_must_never_escape"));

    service
        .grant(
            "user-secret",
            SecretGrantInput {
                secret_id: secret.id.clone(),
                scope_type: "run".into(),
                scope_id: "run-allowed".into(),
                environment_key: "GITHUB_TOKEN".into(),
                expires_at: Some(now_ms() + 30_000),
            },
        )
        .await
        .unwrap();
    let materialized = service
        .materialize(
            "user-secret",
            &SecretAccessContext {
                project_id: "project-secret".into(),
                run_id: Some("run-allowed".into()),
                agent_id: Some("agent-a".into()),
            },
            &[SecretReferenceRequest {
                secret_id: secret.id.clone(),
                environment_key: "GITHUB_TOKEN".into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        materialized.get("GITHUB_TOKEN"),
        Some("ghp_plaintext_must_never_escape")
    );
    assert!(!format!("{materialized:?}").contains("ghp_plaintext"));

    assert!(
        service
            .materialize(
                "user-other",
                &SecretAccessContext {
                    project_id: "project-secret".into(),
                    run_id: Some("run-allowed".into()),
                    agent_id: None,
                },
                &[SecretReferenceRequest {
                    secret_id: secret.id.clone(),
                    environment_key: "GITHUB_TOKEN".into(),
                }],
            )
            .await
            .is_err()
    );
    assert!(
        service
            .materialize(
                "user-secret",
                &SecretAccessContext {
                    project_id: "project-secret".into(),
                    run_id: Some("run-denied".into()),
                    agent_id: None,
                },
                &[SecretReferenceRequest {
                    secret_id: secret.id.clone(),
                    environment_key: "GITHUB_TOKEN".into(),
                }],
            )
            .await
            .is_err()
    );

    service.revoke("user-secret", &secret.id).await.unwrap();
    assert!(
        service
            .materialize(
                "user-secret",
                &SecretAccessContext {
                    project_id: "project-secret".into(),
                    run_id: Some("run-allowed".into()),
                    agent_id: None,
                },
                &[SecretReferenceRequest {
                    secret_id: secret.id,
                    environment_key: "GITHUB_TOKEN".into(),
                }],
            )
            .await
            .is_err()
    );

    let audit: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, redacted_payload_json FROM development_audit_events \
         WHERE project_id = 'project-secret' ORDER BY created_at, id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    for action in ["secret.create", "secret.grant", "secret.materialize", "secret.revoke"] {
        assert!(
            audit.iter().any(|event| event.0 == action),
            "missing audit action {action}"
        );
    }
    assert!(
        audit
            .iter()
            .all(|event| !event.1.contains("ghp_plaintext_must_never_escape"))
    );
}

#[tokio::test]
async fn expired_secrets_and_agent_grants_fail_closed() {
    let (service, _db, _workspace) = setup().await;
    let expired = service
        .create(
            "user-secret",
            "project-secret",
            SecretCreateInput {
                name: "Expired".into(),
                value: "expired-value".into(),
                expires_at: Some(now_ms() - 1),
            },
        )
        .await
        .unwrap();
    service
        .grant(
            "user-secret",
            SecretGrantInput {
                secret_id: expired.id.clone(),
                scope_type: "agent".into(),
                scope_id: "agent-a".into(),
                environment_key: "AGENT_TOKEN".into(),
                expires_at: None,
            },
        )
        .await
        .unwrap();
    for agent_id in ["agent-a", "agent-b"] {
        assert!(
            service
                .materialize(
                    "user-secret",
                    &SecretAccessContext {
                        project_id: "project-secret".into(),
                        run_id: None,
                        agent_id: Some(agent_id.into()),
                    },
                    &[SecretReferenceRequest {
                        secret_id: expired.id.clone(),
                        environment_key: "AGENT_TOKEN".into(),
                    }],
                )
                .await
                .is_err()
        );
    }
}

#[test]
fn complete_policy_requires_exact_allowlists_and_double_confirmation_for_dangerous_actions() {
    let rules = DevelopmentPolicyRules {
        allowed_commands: vec!["cargo".into(), "bun".into()],
        protected_paths: vec![".github/workflows".into(), ".env".into()],
        allowed_network_hosts: vec!["api.github.com".into()],
        protected_branches: vec!["main".into()],
        dangerous_confirmation_count: 2,
    };
    assert_eq!(
        PolicyEngine::evaluate(
            &rules,
            &PolicyOperation::Command {
                program: "cargo".into()
            },
            0
        ),
        PolicyDecision::Allowed
    );
    assert!(matches!(
        PolicyEngine::evaluate(&rules, &PolicyOperation::Command { program: "bash".into() }, 0),
        PolicyDecision::Denied { .. }
    ));
    assert!(matches!(
        PolicyEngine::evaluate(
            &rules,
            &PolicyOperation::Path {
                path: ".github/workflows/release.yml".into(),
                write: true,
            },
            0,
        ),
        PolicyDecision::Denied { .. }
    ));
    assert_eq!(
        PolicyEngine::evaluate(
            &rules,
            &PolicyOperation::Network {
                host: "api.github.com".into(),
            },
            0,
        ),
        PolicyDecision::Allowed
    );
    for operation in [
        PolicyOperation::Git {
            operation: "push".into(),
            branch: Some("main".into()),
        },
        PolicyOperation::Deploy {
            target: "production".into(),
        },
        PolicyOperation::Delete {
            path: "workspace".into(),
        },
    ] {
        assert_eq!(
            PolicyEngine::evaluate(&rules, &operation, 1),
            PolicyDecision::ConfirmationRequired { remaining: 1 }
        );
        assert_eq!(PolicyEngine::evaluate(&rules, &operation, 2), PolicyDecision::Allowed);
    }
}

#[test]
fn one_redactor_covers_every_persisted_and_user_visible_boundary() {
    let redactor = SecretRedactor::new(["ghp_secret_value".to_owned(), "database-password".to_owned()]);
    for boundary in [
        "message",
        "output",
        "artifact",
        "provider_error",
        "audit",
        "export",
        "backup",
    ] {
        let redacted = redactor.redact_text(&format!(
            "{boundary}: token=ghp_secret_value password=database-password https://user:database-password@example.test"
        ));
        assert!(!redacted.contains("ghp_secret_value"));
        assert!(!redacted.contains("database-password"));
        assert!(redacted.contains("[REDACTED]"));
    }
}
