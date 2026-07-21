use aionui_db::{
    FeedbackDiagnosticsDbContext, FeedbackDiagnosticsProfile, FeedbackDiagnosticsRequest,
    IFeedbackDiagnosticsRepository, SqliteFeedbackDiagnosticsRepository, init_database_memory,
};
use serde_json::json;

const ANCHOR_CREATED_AT: i64 = 9_000_000_000_000;
const ANCHOR_UPDATED_AT: i64 = 9_000_000_010_000;

async fn insert_feedback_fixture(db: &aionui_db::Database) {
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO providers \
            (id, platform, name, base_url, api_key_encrypted, models, enabled, \
             capabilities, context_limit, model_protocols, model_enabled, model_health, \
             bedrock_config, is_full_url, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("prov-secret")
    .bind("openrouter")
    .bind("OpenRouter")
    .bind("https://sk-live-secret@example.invalid/v1/chat/completions?api_key=sk-query")
    .bind("encrypted-sk-live-secret")
    .bind(r#"["sakana/fugu-ultra","anthropic/claude-sonnet-4"]"#)
    .bind(true)
    .bind(r#"[{"type":"text"},{"type":"image"}]"#)
    .bind(128000_i64)
    .bind(r#"{"sakana/fugu-ultra":"openai"}"#)
    .bind(r#"{"sakana/fugu-ultra":true,"anthropic/claude-sonnet-4":false}"#)
    .bind(r#"{"sakana/fugu-ultra":{"ok":false,"code":"auth_failed"}}"#)
    .bind(None::<String>)
    .bind(false)
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO teams \
            (id, user_id, name, workspace, workspace_mode, agents, lead_agent_id, \
             session_mode, agents_version, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("team-1")
    .bind("system_default_user")
    .bind("Diagnostics Team")
    .bind("/tmp/team-workspace")
    .bind("shared")
    .bind(
        json!([
            {"slot_id":"slot-lead","agent_id":"opencode","role":"lead"},
            {"slot_id":"slot-1","agent_id":"opencode","role":"teammate"}
        ])
        .to_string(),
    )
    .bind("opencode")
    .bind("build")
    .bind("1.0.0")
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, model, status, source, channel_chat_id, \
             pinned, pinned_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-auth")
    .bind("system_default_user")
    .bind("Auth failure title should be visible")
    .bind("aionrs")
    .bind(
        json!({
            "agentId": "opencode",
            "assistant_id": "assistant-team",
            "teamId": "team-1",
            "role": "teammate",
            "slot_id": "slot-1",
            "session_mode": "build",
            "apiKey": "sk-extra-secret"
        })
        .to_string(),
    )
    .bind(json!({"providerId":"prov-secret","modelId":"sakana/fugu-ultra"}).to_string())
    .bind("running")
    .bind("aionui")
    .bind(None::<String>)
    .bind(false)
    .bind(None::<i64>)
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, model, status, source, channel_chat_id, \
             pinned, pinned_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-nearby")
    .bind("system_default_user")
    .bind("Nearby conversation shown in screenshot")
    .bind("aionrs")
    .bind(json!({"agentId":"opencode"}).to_string())
    .bind(json!({"providerId":"prov-secret","modelId":"anthropic/claude-sonnet-4"}).to_string())
    .bind("finished")
    .bind("aionui")
    .bind(None::<String>)
    .bind(false)
    .bind(None::<i64>)
    .bind(ANCHOR_CREATED_AT - 2_000)
    .bind(ANCHOR_UPDATED_AT - 1_000)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, model, status, source, channel_chat_id, \
             pinned, pinned_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-old")
    .bind("system_default_user")
    .bind("Old conversation outside feedback window")
    .bind("aionrs")
    .bind(json!({}).to_string())
    .bind(None::<String>)
    .bind("finished")
    .bind("aionui")
    .bind(None::<String>)
    .bind(false)
    .bind(None::<i64>)
    .bind(ANCHOR_CREATED_AT - 90_000_000)
    .bind(ANCHOR_UPDATED_AT - 90_000_000)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, model, status, source, channel_chat_id, \
             pinned, pinned_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-deleted-team")
    .bind("system_default_user")
    .bind("Deleted team conversation should not be returned")
    .bind("aionrs")
    .bind(
        json!({
            "agentId": "opencode",
            "teamId": "team-deleted",
            "role": "lead",
            "slot_id": "slot-deleted"
        })
        .to_string(),
    )
    .bind(json!({"providerId":"prov-secret","modelId":"deleted/model"}).to_string())
    .bind("finished")
    .bind("aionui")
    .bind(None::<String>)
    .bind(false)
    .bind(None::<i64>)
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT + 2_000)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, model, status, source, channel_chat_id, \
             pinned, pinned_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-deleted-assistant")
    .bind("system_default_user")
    .bind("Deleted assistant direct conversation should not be returned")
    .bind("aionrs")
    .bind(json!({"agentId":"opencode"}).to_string())
    .bind(json!({"providerId":"prov-secret","modelId":"deleted/assistant-model"}).to_string())
    .bind("finished")
    .bind("aionui")
    .bind(None::<String>)
    .bind(false)
    .bind(None::<i64>)
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT + 3_000)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-deleted-assistant-error")
    .bind("conv-deleted-assistant")
    .bind("turn-deleted-assistant")
    .bind("tips")
    .bind(
        json!({
            "error": {
                "code": "DeletedAssistantConversationError",
                "message": "deleted assistant conversation error should not be returned"
            }
        })
        .to_string(),
    )
    .bind("center")
    .bind("error")
    .bind(false)
    .bind(ANCHOR_UPDATED_AT + 3_000)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-deleted-team-error")
    .bind("conv-deleted-team")
    .bind("turn-deleted")
    .bind("tips")
    .bind(
        json!({
            "error": {
                "code": "DeletedTeamConversationError",
                "message": "deleted team conversation error should not be returned"
            }
        })
        .to_string(),
    )
    .bind("center")
    .bind("error")
    .bind(false)
    .bind(ANCHOR_UPDATED_AT + 2_000)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-user")
    .bind("conv-auth")
    .bind("turn-1")
    .bind("text")
    .bind(json!({"text":"my prompt contains sk-prompt-secret and private content"}).to_string())
    .bind("right")
    .bind("finish")
    .bind(false)
    .bind(4100_i64)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-error")
    .bind("conv-auth")
    .bind("turn-1")
    .bind("tips")
    .bind(
        json!({
            "error": {
                "code": "UserLlmProviderAuthFailed",
                "message": "Provider auth failed",
                "ownership": "user_llm_provider",
                "retryable": false,
            },
            "resolution": {
                "kind": "provider_settings",
                "targetId": "prov-secret"
            },
            "feedbackRecommended": true
        })
        .to_string(),
    )
    .bind("center")
    .bind("error")
    .bind(false)
    .bind(4200_i64)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-tool-failure")
    .bind("conv-auth")
    .bind("turn-tool-failure")
    .bind("acp_tool_call")
    .bind(
        json!({
            "_meta": {
                "claudeCode": {"toolName": "Bash"},
                "terminal_exit": {"exit_code": 127, "terminal_id": "call-tool-1"},
                "terminal_info": {"cwd": "/tmp/workspace"}
            },
            "update": {
                "content": [{
                    "content": {"text": "Exit code 127\npython.exe: command not found"}
                }]
            }
        })
        .to_string(),
    )
    .bind("center")
    .bind("error")
    .bind(false)
    .bind(ANCHOR_UPDATED_AT + 500)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-nearby-error")
    .bind("conv-nearby")
    .bind("turn-nearby")
    .bind("tips")
    .bind(
        json!({
            "error": {
                "code": "NearbyProviderTimeout",
                "message": "Nearby raw provider timeout should not leak"
            }
        })
        .to_string(),
    )
    .bind("center")
    .bind("error")
    .bind(false)
    .bind(ANCHOR_UPDATED_AT - 500)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO acp_session \
            (conversation_id, agent_source, agent_id, session_id, session_status, \
             session_config, last_active_at, suspended_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-auth")
    .bind("builtin")
    .bind("opencode")
    .bind("sess-1")
    .bind("idle")
    .bind(
        json!({
            "runtime": {
                "current_mode_id": "build",
                "current_model_id": "sakana/fugu-ultra",
                "config_selections": {
                    "mode": "plan",
                    "model": "sakana/fugu-ultra",
                    "effort": "high",
                    "apiKey": "sk-session-secret"
                }
            }
        })
        .to_string(),
    )
    .bind(4300_i64)
    .bind(None::<i64>)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO acp_session \
            (conversation_id, agent_source, agent_id, session_id, session_status, \
             session_config, last_active_at, suspended_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-nearby")
    .bind("builtin")
    .bind("opencode")
    .bind("sess-nearby")
    .bind("idle")
    .bind(
        json!({
            "runtime": {
                "current_mode_id": "inspect",
                "current_model_id": "anthropic/claude-sonnet-4",
                "config_selections": {
                    "mode": "inspect",
                    "model": "anthropic/claude-sonnet-4",
                    "apiKey": "sk-nearby-session-secret"
                }
            }
        })
        .to_string(),
    )
    .bind(ANCHOR_UPDATED_AT - 900)
    .bind(None::<i64>)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agent_metadata \
            (id, name, backend, agent_type, agent_source, enabled, command, args, env, \
             native_skills_dirs, behavior_policy, available_modes, available_models, sort_order, \
             last_check_status, last_check_kind, last_check_error_code, last_check_error_message, \
             created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("opencode")
    .bind("OpenCode")
    .bind("opencode")
    .bind("acp")
    .bind("builtin")
    .bind(true)
    .bind("opencode")
    .bind(json!(["run", "--token", "sk-agent-args-secret"]).to_string())
    .bind(json!({"OPENAI_API_KEY":"sk-agent-env-secret"}).to_string())
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(json!([{"id":"build"},{"id":"plan"}]).to_string())
    .bind(json!([{"id":"sakana/fugu-ultra"},{"id":"anthropic/claude-sonnet-4"}]).to_string())
    .bind(1_i64)
    .bind("ok")
    .bind("session")
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT)
    .execute(pool)
    .await
    .unwrap();

    for (definition_id, assistant_id, deleted_at) in [
        ("assistant-def-team", "assistant-team", None),
        ("assistant-def-direct", "assistant-direct", None),
        (
            "assistant-def-deleted",
            "assistant-deleted",
            Some(ANCHOR_UPDATED_AT + 1_000),
        ),
    ] {
        sqlx::query(
            "INSERT INTO assistant_definitions \
                (id, assistant_id, source, owner_type, source_ref, \
                 name, name_i18n, description, description_i18n, avatar_type, avatar_value, \
                 agent_id, rule_resource_type, rule_resource_ref, \
                 recommended_prompts, recommended_prompts_i18n, default_model_mode, default_model_value, \
                 default_permission_mode, default_permission_value, default_skills_mode, default_skill_ids, \
                 custom_skill_names, default_disabled_builtin_skill_ids, default_mcps_mode, default_mcp_ids, \
                 created_at, updated_at, deleted_at, default_thought_level_mode, default_thought_level_value) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(definition_id)
        .bind(assistant_id)
        .bind("user")
        .bind("user")
        .bind(None::<String>)
        .bind(assistant_id)
        .bind("{}")
        .bind(None::<String>)
        .bind("{}")
        .bind("none")
        .bind(None::<String>)
        .bind("opencode")
        .bind("none")
        .bind(None::<String>)
        .bind("[]")
        .bind("{}")
        .bind("auto")
        .bind(None::<String>)
        .bind("auto")
        .bind(None::<String>)
        .bind("auto")
        .bind("[]")
        .bind("[]")
        .bind("[]")
        .bind("auto")
        .bind("[]")
        .bind(ANCHOR_CREATED_AT)
        .bind(ANCHOR_UPDATED_AT)
        .bind(deleted_at)
        .bind("auto")
        .bind(None::<String>)
        .execute(pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO client_preferences (key, value, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind("appearance.uiScale")
    .bind("1.25")
    .bind(ANCHOR_UPDATED_AT + 100)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO system_settings \
            (id, language, notification_enabled, cron_notification_enabled, command_queue_enabled, save_upload_to_workspace, updated_at) \
         VALUES (1, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            language = excluded.language, \
            notification_enabled = excluded.notification_enabled, \
            cron_notification_enabled = excluded.cron_notification_enabled, \
            command_queue_enabled = excluded.command_queue_enabled, \
            save_upload_to_workspace = excluded.save_upload_to_workspace, \
            updated_at = excluded.updated_at",
    )
    .bind("zh-CN")
    .bind(true)
    .bind(false)
    .bind(true)
    .bind(true)
    .bind(ANCHOR_UPDATED_AT + 101)
    .execute(pool)
    .await
    .unwrap();

    for (conversation_id, definition_id, assistant_id, resolved_model_id, thought_level) in [
        (
            "conv-auth",
            "assistant-def-team",
            "assistant-team",
            "sakana/fugu-ultra",
            Some("high"),
        ),
        (
            "conv-nearby",
            "assistant-def-direct",
            "assistant-direct",
            "anthropic/claude-sonnet-4",
            None,
        ),
        (
            "conv-deleted-assistant",
            "assistant-def-deleted",
            "assistant-deleted",
            "deleted/assistant-model",
            None,
        ),
    ] {
        sqlx::query(
            "INSERT INTO conversation_assistant_snapshots \
                (conversation_id, assistant_definition_id, assistant_id, assistant_source, agent_id, \
                 rules_content, default_model_mode, resolved_model_id, default_permission_mode, \
                 resolved_permission_value, default_skills_mode, resolved_skill_ids, \
                 resolved_disabled_builtin_skill_ids, default_mcps_mode, resolved_mcp_ids, \
                 created_at, updated_at, default_thought_level_mode, resolved_thought_level_value) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(conversation_id)
        .bind(definition_id)
        .bind(assistant_id)
        .bind("user")
        .bind("opencode")
        .bind("rules should not leak")
        .bind("auto")
        .bind(resolved_model_id)
        .bind("auto")
        .bind(None::<String>)
        .bind("auto")
        .bind("[]")
        .bind("[]")
        .bind("auto")
        .bind("[]")
        .bind(ANCHOR_CREATED_AT)
        .bind(ANCHOR_UPDATED_AT)
        .bind("auto")
        .bind(thought_level)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn insert_mcp_feedback_fixture(db: &aionui_db::Database) {
    let original_json = json!({
        "command": "npx @sentry/mcp-server@latest --access-token=raw-token-for-diagnostics --organization-slug=iofficeai",
        "args": ["raw-config-mcp", "--header=Authorization: Bearer raw-bearer-for-diagnostics"],
        "env": {
            "MCP_API_KEY": "raw-api-key-for-diagnostics",
            "MCP_MODEL": "diagnostic-model"
        }
    })
    .to_string();

    sqlx::query(
        "INSERT INTO mcp_servers \
            (id, name, description, enabled, transport_type, transport_config, tools, \
             last_test_status, last_connected, original_json, builtin, deleted_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("mcp-raw-config")
    .bind("raw-config-mcp")
    .bind(None::<String>)
    .bind(true)
    .bind("stdio")
    .bind(json!({"command":"npx","args":["raw-config-mcp"],"env":{}}).to_string())
    .bind(None::<String>)
    .bind("error")
    .bind(None::<i64>)
    .bind(original_json)
    .bind(false)
    .bind(None::<i64>)
    .bind(ANCHOR_CREATED_AT)
    .bind(ANCHOR_UPDATED_AT)
    .execute(db.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn collects_conversation_auth_signals_without_sensitive_payloads() {
    let db = init_database_memory().await.unwrap();
    insert_feedback_fixture(&db).await;
    let repo = SqliteFeedbackDiagnosticsRepository::new(db.pool().clone());

    let result = repo
        .collect_feedback_diagnostics(&FeedbackDiagnosticsRequest {
            user_id: "system_default_user".to_owned(),
            profiles: vec![
                FeedbackDiagnosticsProfile::ConversationSession,
                FeedbackDiagnosticsProfile::ModelAuth,
            ],
            context: FeedbackDiagnosticsDbContext {
                conversation_id: Some("conv-auth".to_owned()),
                ..FeedbackDiagnosticsDbContext::default()
            },
        })
        .await
        .unwrap();

    let conversation = result
        .profiles
        .iter()
        .find(|profile| profile.name == "conversation-session")
        .expect("conversation profile should exist");
    assert_eq!(conversation.mode, "detail");
    assert_eq!(conversation.data["conversation"]["id"], "conv-auth");
    assert_eq!(
        conversation.data["conversation"]["title"],
        "Auth failure title should be visible"
    );
    assert_eq!(conversation.data["conversation"]["model_provider_id"], "prov-secret");
    assert_eq!(conversation.data["messages"]["by_type"]["tips"], 1);
    assert_eq!(
        conversation.data["messages"]["recent_errors"][1]["code"],
        "UserLlmProviderAuthFailed"
    );
    let selected_error_details = conversation.data["messages"]["recent_error_details"]
        .as_array()
        .expect("selected conversation raw error details should be included");
    assert!(selected_error_details.iter().any(|item| {
        item["code"] == "UserLlmProviderAuthFailed" && item["content"]["error"]["message"] == "Provider auth failed"
    }));
    let summaries = conversation.data["messages"]["failure_summaries"]
        .as_array()
        .expect("failure summaries should be included");
    assert!(summaries.iter().any(|item| {
        item["kind"] == "tool"
            && item["tool_name"] == "Bash"
            && item["exit_code"] == 127
            && item["stderr_class"] == "command_not_found"
    }));
    assert_eq!(conversation.data["acp_session"]["current_mode_id"], "build");
    assert_eq!(conversation.data["acp_session"]["config_selections"]["mode"], "plan");
    assert_eq!(
        conversation.data["acp_session"]["config_selections"]["model"],
        "sakana/fugu-ultra"
    );
    assert_eq!(conversation.data["acp_session"]["config_selections"]["effort"], "high");
    assert_eq!(conversation.data["agent_metadata"]["available_model_count"], 2);
    let recent_conversations = conversation.data["recent_conversations"]["items"]
        .as_array()
        .expect("recent conversations should be an array");
    let recent_ids = recent_conversations
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(recent_ids, vec!["conv-auth", "conv-nearby"]);
    assert_eq!(
        recent_conversations[1]["title"],
        "Nearby conversation shown in screenshot"
    );
    assert_eq!(recent_conversations[1]["latest_error_code"], "NearbyProviderTimeout");

    let model_auth = result
        .profiles
        .iter()
        .find(|profile| profile.name == "model-auth")
        .expect("model auth profile should exist");
    assert_eq!(model_auth.data["providers"][0]["id"], "prov-secret");
    assert_eq!(model_auth.data["providers"][0]["base_url_host"], "example.invalid");
    assert_eq!(model_auth.data["providers"][0]["api_key_configured"], true);

    let serialized = serde_json::to_string(&result).unwrap();
    for secret in [
        "sk-live-secret",
        "encrypted-sk-live-secret",
        "sk-query",
        "sk-extra-secret",
        "sk-prompt-secret",
        "private content",
        "sk-session-secret",
        "sk-agent-args-secret",
        "sk-agent-env-secret",
        "Nearby raw provider timeout",
        "Old conversation outside feedback window",
    ] {
        assert!(
            !serialized.contains(secret),
            "diagnostics response leaked sensitive payload: {secret}"
        );
    }
}

#[tokio::test]
async fn mcp_tools_profile_preserves_original_json_shape_with_redacted_credentials() {
    let db = init_database_memory().await.unwrap();
    insert_mcp_feedback_fixture(&db).await;
    let repo = SqliteFeedbackDiagnosticsRepository::new(db.pool().clone());
    let raw_original_json = json!({
        "command": "npx @sentry/mcp-server@latest --access-token=raw-token-for-diagnostics --organization-slug=iofficeai",
        "args": ["raw-config-mcp", "--header=Authorization: Bearer raw-bearer-for-diagnostics"],
        "env": {
            "MCP_API_KEY": "raw-api-key-for-diagnostics",
            "MCP_MODEL": "diagnostic-model"
        }
    })
    .to_string();

    let result = repo
        .collect_feedback_diagnostics(&FeedbackDiagnosticsRequest {
            user_id: "system_default_user".to_owned(),
            profiles: vec![FeedbackDiagnosticsProfile::McpTools],
            context: FeedbackDiagnosticsDbContext::default(),
        })
        .await
        .unwrap();

    let mcp_tools = result
        .profiles
        .iter()
        .find(|profile| profile.name == "mcp-tools")
        .expect("mcp tools profile should exist");
    let server = &mcp_tools.data["servers"][0];

    assert_eq!(server["name"], "raw-config-mcp");
    assert_eq!(server["original_json_bytes"], raw_original_json.len());

    let original_json = server["original_json"]
        .as_str()
        .expect("sanitized original_json should remain a JSON string");
    let parsed: serde_json::Value = serde_json::from_str(original_json).unwrap();
    assert_eq!(
        parsed["command"],
        "npx @sentry/mcp-server@latest --access-token=<redacted> --organization-slug=iofficeai"
    );
    assert_eq!(
        parsed["args"],
        json!(["raw-config-mcp", "--header=Authorization: Bearer <redacted>"])
    );
    assert_eq!(parsed["env"]["MCP_API_KEY"], "<redacted>");
    assert_eq!(parsed["env"]["MCP_MODEL"], "diagnostic-model");

    let serialized = serde_json::to_string(&result).unwrap();
    for secret in [
        "raw-token-for-diagnostics",
        "raw-api-key-for-diagnostics",
        "raw-bearer-for-diagnostics",
    ] {
        assert!(
            !serialized.contains(secret),
            "diagnostics response leaked MCP credential payload: {secret}"
        );
    }
}

#[tokio::test]
async fn conversation_profile_is_scoped_to_current_user() {
    let db = init_database_memory().await.unwrap();
    insert_feedback_fixture(&db).await;
    let repo = SqliteFeedbackDiagnosticsRepository::new(db.pool().clone());

    let result = repo
        .collect_feedback_diagnostics(&FeedbackDiagnosticsRequest {
            user_id: "other-user".to_owned(),
            profiles: vec![FeedbackDiagnosticsProfile::ConversationSession],
            context: FeedbackDiagnosticsDbContext {
                conversation_id: Some("conv-auth".to_owned()),
                ..FeedbackDiagnosticsDbContext::default()
            },
        })
        .await
        .unwrap();

    let conversation = result
        .profiles
        .iter()
        .find(|profile| profile.name == "conversation-session")
        .expect("conversation profile should exist");
    assert_eq!(conversation.mode, "not_found");
    assert!(conversation.data["conversation"].is_null());
}

#[tokio::test]
async fn global_summary_includes_recent_diagnostics_without_sensitive_payloads() {
    let db = init_database_memory().await.unwrap();
    insert_feedback_fixture(&db).await;
    let repo = SqliteFeedbackDiagnosticsRepository::new(db.pool().clone());

    let result = repo
        .collect_feedback_diagnostics(&FeedbackDiagnosticsRequest {
            user_id: "system_default_user".to_owned(),
            profiles: vec![FeedbackDiagnosticsProfile::GlobalSummary],
            context: FeedbackDiagnosticsDbContext::default(),
        })
        .await
        .unwrap();

    let global = result
        .profiles
        .iter()
        .find(|profile| profile.name == "global-summary")
        .expect("global summary profile should exist");
    assert_eq!(global.mode, "summary");
    assert_eq!(global.data["conversation_count"], 3);
    assert_eq!(global.data["message_count"], 4);
    let status_count_total = global.data["conversation_status_counts"]
        .as_array()
        .expect("conversation status counts should be included")
        .iter()
        .map(|item| item["count"].as_i64().unwrap())
        .sum::<i64>();
    assert_eq!(status_count_total, 3);

    let direct_conversations = global.data["recent_conversations"]["direct"]["items"]
        .as_array()
        .expect("direct recent conversations should be included");
    let direct_ids = direct_conversations
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(direct_ids, vec!["conv-nearby", "conv-old"]);
    assert!(!direct_ids.contains(&"conv-deleted-assistant"));
    assert_eq!(
        direct_conversations[0]["title"],
        "Nearby conversation shown in screenshot"
    );
    assert_eq!(direct_conversations[0]["assistant_id"], "assistant-direct");
    assert_eq!(direct_conversations[0]["agent_id"], "opencode");
    assert_eq!(direct_conversations[0]["current_model_id"], "anthropic/claude-sonnet-4");
    assert_eq!(
        direct_conversations[0]["recent_errors"][0]["content"]["error"]["message"],
        "Nearby raw provider timeout should not leak"
    );
    assert_eq!(direct_conversations[0]["message_count"], 1);

    let teams = global.data["recent_conversations"]["team"]["items"]
        .as_array()
        .expect("recent team groups should be included");
    let team_ids = teams
        .iter()
        .map(|item| item["team_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(team_ids, vec!["team-1"]);
    assert!(!team_ids.contains(&"team-deleted"));
    assert_eq!(teams[0]["id"], "team-1");
    assert_eq!(teams[0]["name"], "Diagnostics Team");
    assert_eq!(teams[0]["workspace_mode"], "shared");
    assert_eq!(teams[0]["session_mode"], "build");
    assert_eq!(teams[0]["lead_agent_id"], "opencode");
    assert_eq!(teams[0]["agent_count"], 2);
    assert_eq!(teams[0]["conversation_count"], 1);
    assert_eq!(teams[0]["message_count"], 3);
    assert_eq!(teams[0]["error_message_count"], 2);

    let team_conversations = teams[0]["conversations"]["items"]
        .as_array()
        .expect("team conversations should be nested under their team group");
    assert_eq!(teams[0]["conversations"]["limit"], 20);
    assert_eq!(team_conversations[0]["id"], "conv-auth");
    assert!(team_conversations.iter().all(|item| item["id"] != "conv-deleted-team"));
    assert_eq!(team_conversations[0]["team_id"], "team-1");
    assert_eq!(team_conversations[0]["role"], "teammate");
    assert_eq!(team_conversations[0]["slot_id"], "slot-1");
    assert_eq!(team_conversations[0]["assistant_id"], "assistant-team");
    assert_eq!(team_conversations[0]["latest_error_code"], "UserLlmProviderAuthFailed");
    let team_recent_errors = team_conversations[0]["recent_errors"]
        .as_array()
        .expect("team conversation recent errors should be included");
    assert!(team_recent_errors.iter().any(|item| {
        item["failure_summary"]["kind"] == "tool"
            && item["failure_summary"]["tool_name"] == "Bash"
            && item["failure_summary"]["exit_code"] == 127
    }));
    assert_eq!(
        team_recent_errors
            .iter()
            .find(|item| item["code"] == "UserLlmProviderAuthFailed")
            .expect("provider auth error should be present")["content"]["error"]["message"],
        "Provider auth failed"
    );
    assert_eq!(
        team_conversations[0]["recent_messages"][0]["content"]["text"]["redacted"],
        true
    );
    assert_eq!(team_conversations[0]["message_count"], 3);

    let recent_errors = global.data["recent_errors"]["items"]
        .as_array()
        .expect("recent errors should be included");
    let nearby_error = recent_errors
        .iter()
        .find(|item| item["conversation_id"] == "conv-nearby")
        .expect("nearby provider error should be included");
    assert_eq!(
        nearby_error["conversation_title"],
        "Nearby conversation shown in screenshot"
    );
    assert_eq!(nearby_error["code"], "NearbyProviderTimeout");
    assert_eq!(
        nearby_error["content"]["error"]["message"],
        "Nearby raw provider timeout should not leak"
    );
    let tool_failure = recent_errors
        .iter()
        .find(|item| item["failure_summary"]["tool_name"] == "Bash")
        .expect("tool failure summary should be included");
    assert_eq!(tool_failure["conversation_id"], "conv-auth");
    assert_eq!(tool_failure["failure_summary"]["stderr_class"], "command_not_found");
    let auth_error = recent_errors
        .iter()
        .find(|item| item["code"] == "UserLlmProviderAuthFailed")
        .expect("auth provider error should be included");
    assert_eq!(auth_error["conversation_id"], "conv-auth");
    assert_eq!(auth_error["resolution_target_id"], "prov-secret");
    assert!(
        recent_errors
            .iter()
            .all(|item| item["conversation_id"] != "conv-deleted-team"
                && item["conversation_id"] != "conv-deleted-assistant"),
        "deleted data conversations should be excluded from recent errors"
    );

    let agent_items = global.data["agent_health"]["items"]
        .as_array()
        .expect("agent health items should be included");
    let opencode_agent = agent_items
        .iter()
        .find(|item| item["id"] == "opencode")
        .expect("opencode agent health should be included");
    assert_eq!(opencode_agent["last_check_status"], "ok");

    let provider_items = global.data["provider_health"]["items"]
        .as_array()
        .expect("provider health items should be included");
    let secret_provider = provider_items
        .iter()
        .find(|item| item["id"] == "prov-secret")
        .expect("fixture provider health should be included");
    assert_eq!(secret_provider["base_url_host"], "example.invalid");
    assert_eq!(secret_provider["api_key_configured"], true);
    assert_eq!(secret_provider["unhealthy_model_count"], 1);

    let serialized = serde_json::to_string(&result).unwrap();
    for secret in [
        "sk-live-secret",
        "encrypted-sk-live-secret",
        "sk-query",
        "sk-extra-secret",
        "sk-prompt-secret",
        "private content",
        "sk-session-secret",
        "sk-nearby-session-secret",
        "sk-agent-args-secret",
        "sk-agent-env-secret",
        "rules should not leak",
    ] {
        assert!(
            !serialized.contains(secret),
            "global diagnostics response leaked sensitive payload: {secret}"
        );
    }
}

#[tokio::test]
async fn client_ui_settings_profile_exports_existing_preferences() {
    let db = init_database_memory().await.unwrap();
    insert_feedback_fixture(&db).await;
    let repo = SqliteFeedbackDiagnosticsRepository::new(db.pool().clone());

    let result = repo
        .collect_feedback_diagnostics(&FeedbackDiagnosticsRequest {
            user_id: "system_default_user".to_owned(),
            profiles: vec![FeedbackDiagnosticsProfile::ClientUiSettings],
            context: FeedbackDiagnosticsDbContext::default(),
        })
        .await
        .unwrap();

    let profile = result
        .profiles
        .iter()
        .find(|profile| profile.name == "client-ui-settings")
        .expect("client UI settings profile should exist");

    assert_eq!(profile.mode, "summary");
    assert_eq!(profile.data["preferences"]["items"][0]["key"], "appearance.uiScale");
    assert_eq!(profile.data["system_settings"]["language"], "zh-CN");
    assert_eq!(profile.data["system_settings"]["command_queue_enabled"], true);
}

#[tokio::test]
async fn workspace_summary_uses_existing_conversation_and_team_context() {
    let db = init_database_memory().await.unwrap();
    insert_feedback_fixture(&db).await;
    let repo = SqliteFeedbackDiagnosticsRepository::new(db.pool().clone());

    let result = repo
        .collect_feedback_diagnostics(&FeedbackDiagnosticsRequest {
            user_id: "system_default_user".to_owned(),
            profiles: vec![FeedbackDiagnosticsProfile::WorkspaceSummary],
            context: FeedbackDiagnosticsDbContext {
                conversation_id: Some("conv-auth".to_owned()),
                ..FeedbackDiagnosticsDbContext::default()
            },
        })
        .await
        .unwrap();

    let profile = result
        .profiles
        .iter()
        .find(|profile| profile.name == "workspace-summary")
        .expect("workspace summary profile should exist");

    assert_eq!(profile.mode, "detail");
    assert_eq!(profile.data["conversation"]["id"], "conv-auth");
    assert_eq!(profile.data["team"]["team_id"], "team-1");
    assert_eq!(profile.data["team"]["workspace"], "/tmp/team-workspace");
    assert_eq!(profile.data["runtime_checks"]["path_exists"], serde_json::Value::Null);
}
