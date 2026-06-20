use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ChannelError;
use crate::types::{PluginConfig, UnifiedMessageContent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MattermostConfig {
    pub server_url: String,
    pub access_token: String,
    pub allowed_channel_ids: HashSet<String>,
    pub reply_in_thread: bool,
    pub ignore_self_messages: bool,
}

impl MattermostConfig {
    pub fn from_plugin_config(config: &PluginConfig) -> Result<Self, ChannelError> {
        let access_token = first_non_empty([
            config.credentials.extra.get("accessToken"),
            config.credentials.extra.get("access_token"),
        ])
        .or_else(|| non_empty_string(config.credentials.token.as_deref()))
        .or_else(|| non_empty_string(config.credentials.bot_token.as_deref()))
        .ok_or_else(|| ChannelError::InvalidConfig("Missing Mattermost access token".into()))?;

        let config_extra = config.config.as_ref().map(|c| &c.extra);
        let server_url = config_extra
            .and_then(|extra| first_non_empty([extra.get("serverUrl"), extra.get("server_url")]))
            .ok_or_else(|| ChannelError::InvalidConfig("Missing Mattermost server URL".into()))?;

        let allowed_channel_ids = config_extra
            .and_then(|extra| first_non_empty([extra.get("allowedChannelIds"), extra.get("allowed_channel_ids")]))
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        let reply_in_thread = config_extra
            .and_then(|extra| first_bool([extra.get("replyInThread"), extra.get("reply_in_thread")]))
            .unwrap_or(true);
        let ignore_self_messages = config_extra
            .and_then(|extra| first_bool([extra.get("ignoreSelfMessages"), extra.get("ignore_self_messages")]))
            .unwrap_or(true);

        Ok(Self {
            server_url: normalize_server_url(&server_url)?,
            access_token,
            allowed_channel_ids,
            reply_in_thread,
            ignore_self_messages,
        })
    }

    pub fn channel_allowed(&self, channel_id: &str) -> bool {
        self.allowed_channel_ids.is_empty() || self.allowed_channel_ids.contains(channel_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MattermostUser {
    pub id: String,
    pub username: Option<String>,
    pub nickname: Option<String>,
}

impl MattermostUser {
    pub fn display_name(&self) -> String {
        self.nickname
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.username.as_deref().filter(|s| !s.is_empty()))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.id.clone())
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CreatePostRequest {
    pub channel_id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MattermostPost {
    pub id: String,
    pub channel_id: String,
    pub user_id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub create_at: i64,
    #[serde(default)]
    pub root_id: Option<String>,
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default)]
    pub props: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MattermostWsEvent {
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

pub(crate) fn parse_posted_event(value: &Value) -> Option<MattermostPost> {
    let event: MattermostWsEvent = serde_json::from_value(value.clone()).ok()?;
    if event.event != "posted" {
        return None;
    }
    let post_value = event.data.get("post")?;
    match post_value {
        Value::String(s) => serde_json::from_str(s).ok(),
        Value::Object(_) => serde_json::from_value(post_value.clone()).ok(),
        _ => None,
    }
}

pub(crate) fn post_to_content(post: &MattermostPost) -> UnifiedMessageContent {
    UnifiedMessageContent {
        content_type: crate::types::MessageContentType::Text,
        text: post.message.clone(),
        attachments: None,
    }
}

fn normalize_server_url(raw: &str) -> Result<String, ChannelError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ChannelError::InvalidConfig("Missing Mattermost server URL".into()));
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(ChannelError::InvalidConfig(
            "Mattermost server URL must start with http:// or https://".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn first_non_empty<const N: usize>(values: [Option<&Value>; N]) -> Option<String> {
    values.into_iter().find_map(|value| match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        _ => None,
    })
}

fn first_bool<const N: usize>(values: [Option<&Value>; N]) -> Option<bool> {
    values.into_iter().find_map(|value| match value? {
        Value::Bool(v) => Some(*v),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|s| !s.is_empty()).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::types::{PluginConfigOptions, PluginCredentials};

    use super::*;

    fn config(credentials: HashMap<String, Value>, options: HashMap<String, Value>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                token: None,
                app_id: None,
                app_secret: None,
                encrypt_key: None,
                verification_token: None,
                client_id: None,
                client_secret: None,
                account_id: None,
                bot_token: None,
                app_token: None,
                extra: credentials,
            },
            config: Some(PluginConfigOptions {
                mode: None,
                webhook_url: None,
                rate_limit: None,
                require_mention: None,
                extra: options,
            }),
        }
    }

    #[test]
    fn config_accepts_camel_case_fields() {
        let parsed = MattermostConfig::from_plugin_config(&config(
            HashMap::from([("accessToken".into(), Value::String("tok".into()))]),
            HashMap::from([
                ("serverUrl".into(), Value::String("https://mm.example/".into())),
                ("allowedChannelIds".into(), Value::String("c1, c2".into())),
                ("replyInThread".into(), Value::Bool(false)),
            ]),
        ))
        .unwrap();

        assert_eq!(parsed.server_url, "https://mm.example");
        assert_eq!(parsed.access_token, "tok");
        assert!(parsed.channel_allowed("c1"));
        assert!(parsed.channel_allowed("c2"));
        assert!(!parsed.channel_allowed("c3"));
        assert!(!parsed.reply_in_thread);
    }

    #[test]
    fn config_requires_token_and_server_url() {
        let err = MattermostConfig::from_plugin_config(&config(HashMap::new(), HashMap::new())).unwrap_err();
        assert!(err.to_string().contains("access token"));
    }

    #[test]
    fn posted_event_parses_string_post() {
        let raw = serde_json::json!({
            "event": "posted",
            "data": {
                "post": "{\"id\":\"p1\",\"channel_id\":\"c1\",\"user_id\":\"u1\",\"message\":\"hello\",\"create_at\":1000}"
            }
        });
        let post = parse_posted_event(&raw).unwrap();
        assert_eq!(post.id, "p1");
        assert_eq!(post.message, "hello");
    }
}
