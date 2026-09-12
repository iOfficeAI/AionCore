use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn remaining_builtin_acp_launch_contracts() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let cases = [
        ("opencode", "opencode", r#"["acp"]"#, Some("build")),
        ("pi", "npx", r#"["-y","pi-acp"]"#, None),
        (
            "deepseek",
            "node",
            r#"["/path/to/dsh-catl-plugins/scripts/run.mjs"]"#,
            None,
        ),
    ];
    for (backend, command, args, yolo_id) in cases {
        let row = repo
            .find_builtin_by_backend(backend)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("missing {backend}"));
        assert_eq!(row.command.as_deref(), Some(command), "{backend} command");
        assert_eq!(row.args.as_deref(), Some(args), "{backend} args");
        assert_eq!(row.yolo_id.as_deref(), yolo_id, "{backend} yolo_id");
    }
}

#[tokio::test]
async fn purged_acp_registry_backends_are_absent() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    for backend in [
        "gemini", "qwen", "droid", "cursor", "codebuddy", "goose", "auggie", "kimi", "copilot",
    ] {
        assert!(
            repo.find_builtin_by_backend(backend).await.unwrap().is_none(),
            "{backend} must not remain a builtin after 045"
        );
    }
}
