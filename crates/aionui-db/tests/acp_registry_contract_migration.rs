use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn builtin_acp_launch_contracts_follow_verified_registry_entries() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let cases = [
        ("gemini", "gemini", r#"["--acp"]"#, Some("yolo")),
        ("qwen", "qwen", r#"["--acp","--experimental-skills"]"#, None),
        ("droid", "droid", r#"["acp-daemon"]"#, None),
        ("pi", "npx", r#"["-y","pi-acp"]"#, None),
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

    let cursor = repo.find_builtin_by_backend("cursor").await.unwrap().unwrap();
    assert_eq!(cursor.command.as_deref(), Some("cursor-agent"));
    assert_eq!(cursor.args.as_deref(), Some(r#"["acp"]"#));
    assert_eq!(
        cursor.agent_source_info.as_deref(),
        Some(r#"{"binary_name":"cursor-agent"}"#)
    );
    assert_eq!(cursor.yolo_id, None);

    let codebuddy = repo.find_builtin_by_backend("codebuddy").await.unwrap().unwrap();
    assert_eq!(codebuddy.command.as_deref(), Some("npx"));
    assert_eq!(
        codebuddy.args.as_deref(),
        Some(r#"["-y","--package","@tencent-ai/codebuddy-code","codebuddy","--acp"]"#)
    );
    assert_eq!(
        codebuddy.agent_source_info.as_deref(),
        Some(r#"{"binary_name":"codebuddy","bridge_binary":"npx"}"#)
    );

    for backend in ["goose", "auggie", "kimi", "copilot"] {
        let row = repo.find_builtin_by_backend(backend).await.unwrap().unwrap();
        assert_eq!(
            row.yolo_id, None,
            "{backend} must not advertise an unverified yolo mode"
        );
    }
}
