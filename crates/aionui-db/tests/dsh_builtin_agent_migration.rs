//! Migration 045 seeds the DeepSeek Harness (dsh CLI) builtin ACP agent row.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn dsh_builtin_acp_metadata_is_seeded() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let dsh = repo
        .get("d5e89a01")
        .await
        .unwrap()
        .expect("seeded DSH ACP row");

    assert_eq!(dsh.name, "DeepSeek Harness");
    assert_eq!(dsh.backend.as_deref(), Some("dsh"));
    assert_eq!(dsh.agent_type, "acp");
    assert_eq!(dsh.agent_source, "builtin");
    assert_eq!(dsh.command.as_deref(), Some("dsh"));
    assert_eq!(dsh.args.as_deref(), Some(r#"["--profile","acp"]"#));
    assert_eq!(
        dsh.icon.as_deref(),
        Some("/api/assets/logos/ai-major/deepseek.svg")
    );
    assert_eq!(
        dsh.agent_source_info.as_deref(),
        Some(r#"{"binary_name":"dsh"}"#)
    );
    assert_eq!(
        dsh.native_skills_dirs.as_deref(),
        Some(r#"[".agents/skills"]"#)
    );
    assert_eq!(dsh.yolo_id, None);
    assert_eq!(dsh.sort_order, 3135);
}

#[tokio::test]
async fn dsh_can_be_queried_by_backend() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend("dsh")
        .await
        .unwrap()
        .expect("dsh is queryable via find_builtin_by_backend");

    assert_eq!(row.id, "d5e89a01");
    assert_eq!(row.command.as_deref(), Some("dsh"));
    assert_eq!(row.args.as_deref(), Some(r#"["--profile","acp"]"#));
}
