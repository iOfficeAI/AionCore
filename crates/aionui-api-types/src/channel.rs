use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Plugin management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/plugins/enable`.
///
/// Enables a channel plugin with the given configuration. The `config`
/// field is a JSON object containing platform-specific credentials and
/// connection options (`{ credentials, config }`).
#[derive(Debug, Deserialize)]
pub struct EnablePluginRequest {
    pub plugin_id: String,
    pub config: serde_json::Value,
}

/// Request body for `POST /api/channel/plugins/disable`.
#[derive(Debug, Deserialize)]
pub struct DisablePluginRequest {
    pub plugin_id: String,
}

/// Request body for `POST /api/channel/plugins/test`.
///
/// Tests plugin credentials without persisting. For platforms that need
/// additional config (e.g., Lark requires `appId` + `appSecret`),
/// pass them in `extra_config`.
#[derive(Debug, Deserialize)]
pub struct TestPluginRequest {
    pub plugin_id: String,
    pub token: String,
    #[serde(default)]
    pub extra_config: Option<TestPluginExtraConfig>,
}

/// Extra configuration fields for plugin credential testing.
///
/// Used by platforms that require more than a single token
/// (e.g., Lark needs `app_id` + `app_secret`).
#[derive(Debug, Clone, Deserialize)]
pub struct TestPluginExtraConfig {
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
}

// ---------------------------------------------------------------------------
// B. Pairing management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/pairings/approve`.
#[derive(Debug, Deserialize)]
pub struct ApprovePairingRequest {
    pub code: String,
}

/// Request body for `POST /api/channel/pairings/reject`.
#[derive(Debug, Deserialize)]
pub struct RejectPairingRequest {
    pub code: String,
}

// ---------------------------------------------------------------------------
// C. User management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/users/revoke`.
#[derive(Debug, Deserialize)]
pub struct RevokeUserRequest {
    pub user_id: String,
}

// ---------------------------------------------------------------------------
// D. Settings — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/settings/sync`.
///
/// Invalidates all channel sessions for the given platform so they
/// are recreated with the latest agent/model configuration from
/// `client_preferences` on the next incoming message.
#[derive(Debug, Deserialize)]
pub struct SyncChannelSettingsRequest {
    pub platform: String,
}

/// Assistant binding request for a channel platform.
///
/// New writes must use assistant identity only. Legacy backend / agent fields
/// remain readable in responses for historical settings migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChannelAssistantSettingRequest {
    pub assistant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Assistant binding returned by channel settings endpoints.
///
/// Responses remain backward-compatible while historical channel settings are
/// still being read from assistant/backend mixed records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelAssistantSettingResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Default model reference for a channel platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelDefaultModelSetting {
    pub id: String,
    pub use_model: String,
}

/// Aggregated settings payload for one channel platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelPlatformSettingsResponse {
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant: Option<ChannelAssistantSettingResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<ChannelDefaultModelSetting>,
}

// ---------------------------------------------------------------------------
// E. Plugin management — Response DTOs
// ---------------------------------------------------------------------------

/// Plugin status returned by `GET /api/channel/plugins`.
///
/// Corresponds to `IChannelPluginStatus` in the original TypeScript.
/// Excludes encrypted config data for security.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginStatusResponse {
    pub plugin_id: String,
    #[serde(rename = "type")]
    pub plugin_type: String,
    pub name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub connected: bool,
    pub has_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    pub active_users: i64,
}

/// Result of a plugin credential test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestPluginResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Generic success/error response for channel bridge operations.
///
/// Used by enable/disable plugin, approve/reject pairing, revoke user,
/// and sync settings endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BridgeResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// F. Pairing — Response DTOs
// ---------------------------------------------------------------------------

/// Pending pairing request returned by `GET /api/channel/pairings`.
///
/// Corresponds to `IChannelPairingRequest`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairingRequestResponse {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
}

// ---------------------------------------------------------------------------
// G. User — Response DTOs
// ---------------------------------------------------------------------------

/// Authorized channel user returned by `GET /api/channel/users`.
///
/// Corresponds to `IChannelUser`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelUserResponse {
    pub id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub authorized_at: TimestampMs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active: Option<TimestampMs>,
}

// ---------------------------------------------------------------------------
// H. Session — Response DTOs
// ---------------------------------------------------------------------------

/// Active channel session returned by `GET /api/channel/sessions`.
///
/// Corresponds to `IChannelSession`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelSessionResponse {
    pub id: String,
    pub user_id: String,
    pub agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub created_at: TimestampMs,
    pub last_activity: TimestampMs,
}

// ---------------------------------------------------------------------------
// I. WebSocket event payloads
// ---------------------------------------------------------------------------

/// Payload for `channel.pairing-requested` WebSocket event.
///
/// Pushed when an IM user sends their first message and triggers the
/// pairing authorization flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairingRequestedPayload {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub expires_at: TimestampMs,
}

/// Payload for `channel.plugin-status-changed` WebSocket event.
///
/// Pushed when a plugin starts, stops, or encounters an error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginStatusChangedPayload {
    pub plugin_id: String,
    pub status: PluginStatusResponse,
}

/// Payload for `channel.user-authorized` WebSocket event.
///
/// Pushed after a pairing code is approved and the user record is created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserAuthorizedPayload {
    pub id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- A. Plugin management requests ----------------------------------------

    #[test]
    fn test_enable_plugin_request_deserialize() {
        let raw = json!({
            "plugin_id": "telegram",
            "config": {
                "credentials": { "token": "bot123:ABC" },
                "config": { "mode": "polling" }
            }
        });
        let req: EnablePluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_id, "telegram");
        assert_eq!(req.config["credentials"]["token"], "bot123:ABC");
        assert_eq!(req.config["config"]["mode"], "polling");
    }

    #[test]
    fn test_enable_plugin_request_missing_plugin_id() {
        let raw = json!({ "config": {} });
        let result = serde_json::from_value::<EnablePluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_enable_plugin_request_missing_config() {
        let raw = json!({ "plugin_id": "telegram" });
        let result = serde_json::from_value::<EnablePluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_disable_plugin_request_deserialize() {
        let raw = json!({ "plugin_id": "lark" });
        let req: DisablePluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_id, "lark");
    }

    #[test]
    fn test_disable_plugin_request_missing_plugin_id() {
        let raw = json!({});
        let result = serde_json::from_value::<DisablePluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_test_plugin_request_telegram() {
        let raw = json!({
            "plugin_id": "telegram",
            "token": "bot123:ABC"
        });
        let req: TestPluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_id, "telegram");
        assert_eq!(req.token, "bot123:ABC");
        assert!(req.extra_config.is_none());
    }

    #[test]
    fn test_test_plugin_request_lark_with_extra_config() {
        let raw = json!({
            "plugin_id": "lark",
            "token": "xxx",
            "extra_config": {
                "app_id": "cli_abc",
                "app_secret": "secret123"
            }
        });
        let req: TestPluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_id, "lark");
        let extra = req.extra_config.unwrap();
        assert_eq!(extra.app_id.as_deref(), Some("cli_abc"));
        assert_eq!(extra.app_secret.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_test_plugin_request_missing_token() {
        let raw = json!({ "plugin_id": "telegram" });
        let result = serde_json::from_value::<TestPluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_test_plugin_extra_config_partial() {
        let raw = json!({
            "plugin_id": "lark",
            "token": "xxx",
            "extra_config": { "app_id": "cli_abc" }
        });
        let req: TestPluginRequest = serde_json::from_value(raw).unwrap();
        let extra = req.extra_config.unwrap();
        assert_eq!(extra.app_id.as_deref(), Some("cli_abc"));
        assert!(extra.app_secret.is_none());
    }

    // -- B. Pairing requests --------------------------------------------------

    #[test]
    fn test_approve_pairing_request_deserialize() {
        let raw = json!({ "code": "123456" });
        let req: ApprovePairingRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.code, "123456");
    }

    #[test]
    fn test_approve_pairing_request_missing_code() {
        let raw = json!({});
        let result = serde_json::from_value::<ApprovePairingRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_pairing_request_deserialize() {
        let raw = json!({ "code": "654321" });
        let req: RejectPairingRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.code, "654321");
    }

    // -- C. User management requests ------------------------------------------

    #[test]
    fn test_revoke_user_request_deserialize() {
        let raw = json!({ "user_id": "user-1" });
        let req: RevokeUserRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.user_id, "user-1");
    }

    #[test]
    fn test_revoke_user_request_missing_user_id() {
        let raw = json!({});
        let result = serde_json::from_value::<RevokeUserRequest>(raw);
        assert!(result.is_err());
    }

    // -- D. Settings requests -------------------------------------------------

    #[test]
    fn test_sync_settings_request_deserialize() {
        let raw = json!({ "platform": "telegram" });
        let req: SyncChannelSettingsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.platform, "telegram");
    }

    #[test]
    fn test_sync_settings_request_missing_platform() {
        let raw = json!({});
        let result = serde_json::from_value::<SyncChannelSettingsRequest>(raw);
        assert!(result.is_err());
    }

    // -- E. Plugin status response --------------------------------------------

    #[test]
    fn test_plugin_status_response_serde() {
        let resp = PluginStatusResponse {
            plugin_id: "telegram".into(),
            plugin_type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: true,
            status: Some("running".into()),
            last_connected: Some(1700000000000),
            created_at: 1699000000000,
            updated_at: 1700000000000,
            connected: true,
            has_token: true,
            bot_username: Some("my_bot".into()),
            active_users: 5,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["plugin_id"], "telegram");
        assert_eq!(json["type"], "telegram");
        assert_eq!(json["name"], "Telegram Bot");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["status"], "running");
        assert_eq!(json["last_connected"], 1700000000000_i64);
        assert_eq!(json["created_at"], 1699000000000_i64);
        assert_eq!(json["updated_at"], 1700000000000_i64);
        assert_eq!(json["connected"], true);
        assert_eq!(json["has_token"], true);
        assert_eq!(json["bot_username"], "my_bot");
        assert_eq!(json["active_users"], 5);
    }

    #[test]
    fn test_plugin_status_response_optional_fields_omitted() {
        let resp = PluginStatusResponse {
            plugin_id: "lark".into(),
            plugin_type: "lark".into(),
            name: "Lark Bot".into(),
            enabled: false,
            status: None,
            last_connected: None,
            created_at: 1699000000000,
            updated_at: 1699000000000,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("status").is_none());
        assert!(json.get("last_connected").is_none());
        assert!(json.get("bot_username").is_none());
    }

    // -- E. Test plugin response ----------------------------------------------

    #[test]
    fn test_test_plugin_response_success() {
        let resp = TestPluginResponse {
            success: true,
            bot_username: Some("my_bot".into()),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["bot_username"], "my_bot");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_test_plugin_response_failure() {
        let resp = TestPluginResponse {
            success: false,
            bot_username: None,
            error: Some("Invalid token".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert!(json.get("bot_username").is_none());
        assert_eq!(json["error"], "Invalid token");
    }

    // -- E. Bridge response ---------------------------------------------------

    #[test]
    fn test_bridge_response_success() {
        let resp = BridgeResponse {
            success: true,
            message: Some("Plugin enabled".into()),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "Plugin enabled");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_bridge_response_failure() {
        let resp = BridgeResponse {
            success: false,
            message: None,
            error: Some("Connection refused".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert!(json.get("message").is_none());
        assert_eq!(json["error"], "Connection refused");
    }

    #[test]
    fn test_bridge_response_minimal() {
        let resp = BridgeResponse {
            success: true,
            message: None,
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert!(json.get("message").is_none());
        assert!(json.get("error").is_none());
    }

    // -- F. Pairing response --------------------------------------------------

    #[test]
    fn test_pairing_request_response_serde() {
        let resp = PairingRequestResponse {
            code: "123456".into(),
            platform_user_id: "tg_user_42".into(),
            platform_type: "telegram".into(),
            display_name: Some("Alice".into()),
            requested_at: 1700000000000,
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["code"], "123456");
        assert_eq!(json["platform_user_id"], "tg_user_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["display_name"], "Alice");
        assert_eq!(json["requested_at"], 1700000000000_i64);
        assert_eq!(json["expires_at"], 1700000600000_i64);
    }

    #[test]
    fn test_pairing_request_response_no_display_name() {
        let resp = PairingRequestResponse {
            code: "999999".into(),
            platform_user_id: "user_1".into(),
            platform_type: "lark".into(),
            display_name: None,
            requested_at: 1700000000000,
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("display_name").is_none());
    }

    // -- G. User response -----------------------------------------------------

    #[test]
    fn test_channel_user_response_serde() {
        let resp = ChannelUserResponse {
            id: "usr_1".into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            display_name: Some("Bob".into()),
            authorized_at: 1700000000000,
            last_active: Some(1700001000000),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "usr_1");
        assert_eq!(json["platform_user_id"], "tg_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["display_name"], "Bob");
        assert_eq!(json["authorized_at"], 1700000000000_i64);
        assert_eq!(json["last_active"], 1700001000000_i64);
    }

    #[test]
    fn test_channel_user_response_optional_fields_omitted() {
        let resp = ChannelUserResponse {
            id: "usr_2".into(),
            platform_user_id: "lark_1".into(),
            platform_type: "lark".into(),
            display_name: None,
            authorized_at: 1700000000000,
            last_active: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("display_name").is_none());
        assert!(json.get("last_active").is_none());
    }

    // -- H. Session response --------------------------------------------------

    #[test]
    fn test_channel_session_response_serde() {
        let resp = ChannelSessionResponse {
            id: "sess_1".into(),
            user_id: "usr_1".into(),
            agent_type: "gemini".into(),
            conversation_id: Some("conv_abc".into()),
            workspace: Some("/workspace".into()),
            chat_id: Some("chat_123".into()),
            created_at: 1700000000000,
            last_activity: 1700001000000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "sess_1");
        assert_eq!(json["user_id"], "usr_1");
        assert_eq!(json["agent_type"], "gemini");
        assert_eq!(json["conversation_id"], "conv_abc");
        assert_eq!(json["workspace"], "/workspace");
        assert_eq!(json["chat_id"], "chat_123");
        assert_eq!(json["created_at"], 1700000000000_i64);
        assert_eq!(json["last_activity"], 1700001000000_i64);
    }

    #[test]
    fn test_channel_session_response_optional_fields_omitted() {
        let resp = ChannelSessionResponse {
            id: "sess_2".into(),
            user_id: "usr_2".into(),
            agent_type: "acp".into(),
            conversation_id: None,
            workspace: None,
            chat_id: None,
            created_at: 1700000000000,
            last_activity: 1700000000000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("conversation_id").is_none());
        assert!(json.get("workspace").is_none());
        assert!(json.get("chat_id").is_none());
    }

    // -- I. WebSocket event payloads ------------------------------------------

    #[test]
    fn test_pairing_requested_payload_serde() {
        let payload = PairingRequestedPayload {
            code: "123456".into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            display_name: Some("Alice".into()),
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["code"], "123456");
        assert_eq!(json["platform_user_id"], "tg_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["display_name"], "Alice");
        assert_eq!(json["expires_at"], 1700000600000_i64);
    }

    #[test]
    fn test_pairing_requested_payload_no_display_name() {
        let payload = PairingRequestedPayload {
            code: "000001".into(),
            platform_user_id: "u1".into(),
            platform_type: "dingtalk".into(),
            display_name: None,
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("display_name").is_none());
    }

    #[test]
    fn test_plugin_status_changed_payload_serde() {
        let payload = PluginStatusChangedPayload {
            plugin_id: "telegram".into(),
            status: PluginStatusResponse {
                plugin_id: "telegram".into(),
                plugin_type: "telegram".into(),
                name: "Telegram Bot".into(),
                enabled: true,
                status: Some("running".into()),
                last_connected: Some(1700000000000),
                created_at: 1699000000000,
                updated_at: 1700000000000,
                connected: false,
                has_token: false,
                bot_username: None,
                active_users: 0,
            },
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["plugin_id"], "telegram");
        assert_eq!(json["status"]["type"], "telegram");
        assert_eq!(json["status"]["status"], "running");
        assert_eq!(json["status"]["enabled"], true);
    }

    #[test]
    fn test_user_authorized_payload_serde() {
        let payload = UserAuthorizedPayload {
            id: "usr_1".into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            display_name: Some("Alice".into()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["id"], "usr_1");
        assert_eq!(json["platform_user_id"], "tg_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["display_name"], "Alice");
    }

    #[test]
    fn test_user_authorized_payload_no_display_name() {
        let payload = UserAuthorizedPayload {
            id: "usr_2".into(),
            platform_user_id: "lk_1".into(),
            platform_type: "lark".into(),
            display_name: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("display_name").is_none());
    }

    // -- Roundtrip tests ------------------------------------------------------

    #[test]
    fn test_plugin_status_response_roundtrip() {
        let resp = PluginStatusResponse {
            plugin_id: "dingtalk".into(),
            plugin_type: "dingtalk".into(),
            name: "DingTalk Bot".into(),
            enabled: true,
            status: Some("ready".into()),
            last_connected: None,
            created_at: 1699000000000,
            updated_at: 1699000000000,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PluginStatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn test_bridge_response_roundtrip() {
        let resp = BridgeResponse {
            success: true,
            message: Some("Done".into()),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: BridgeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn test_channel_session_response_roundtrip() {
        let resp = ChannelSessionResponse {
            id: "s1".into(),
            user_id: "u1".into(),
            agent_type: "acp".into(),
            conversation_id: Some("c1".into()),
            workspace: None,
            chat_id: Some("ch1".into()),
            created_at: 1000,
            last_activity: 2000,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ChannelSessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn test_channel_assistant_setting_request_rejects_legacy_backend_fields() {
        let err = serde_json::from_str::<ChannelAssistantSettingRequest>(
            r#"{"assistant_id":"bare-codex","backend":"codex","name":"Codex"}"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown field `backend`"), "{err}");
    }

    #[test]
    fn test_channel_assistant_setting_request_rejects_legacy_custom_agent_fields() {
        let err = serde_json::from_str::<ChannelAssistantSettingRequest>(
            r#"{"assistant_id":"legacy-custom","custom_agent_id":"legacy-custom","name":"Codex"}"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown field `custom_agent_id`"), "{err}");
    }

    #[test]
    fn test_channel_assistant_setting_request_rejects_legacy_agent_type_fields() {
        let err = serde_json::from_str::<ChannelAssistantSettingRequest>(
            r#"{"assistant_id":"bare-codex","agent_type":"acp","name":"Codex"}"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown field `agent_type`"), "{err}");
    }
}
