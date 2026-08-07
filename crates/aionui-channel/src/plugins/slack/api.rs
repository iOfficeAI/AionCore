//! Slack Web API client (bot token) + Socket Mode open (app token).

use reqwest::Client;
use tracing::debug;

use crate::error::ChannelError;

use super::types::{
    AuthTestResult, ChatPostMessageRequest, ChatPostResult, ChatUpdateRequest, ConnectionsOpenResult, SlackApiResponse,
};

const SLACK_API_BASE: &str = "https://slack.com/api";

pub(crate) struct SlackApi {
    client: Client,
    bot_token: String,
    app_token: String,
}

impl SlackApi {
    pub fn new(client: Client, bot_token: &str, app_token: &str) -> Self {
        Self {
            client,
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
        }
    }

    /// `auth.test` — validates the bot token and returns bot identity.
    pub async fn auth_test(&self) -> Result<AuthTestResult, ChannelError> {
        self.bot_post_empty::<AuthTestResult>("auth.test").await
    }

    /// `apps.connections.open` — Socket Mode WebSocket URL (app-level token).
    pub async fn connections_open(&self) -> Result<String, ChannelError> {
        let url = format!("{SLACK_API_BASE}/apps.connections.open");
        let resp: SlackApiResponse<ConnectionsOpenResult> = self
            .client
            .post(&url)
            .bearer_auth(&self.app_token)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("connections.open request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("connections.open parse failed: {e}")))?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::ConnectionFailed(format!(
                "Slack connections.open failed: {err}"
            )));
        }

        resp.data
            .url
            .filter(|u| !u.is_empty())
            .ok_or_else(|| ChannelError::ConnectionFailed("connections.open returned no url".into()))
    }

    /// `chat.postMessage`.
    pub async fn post_message(&self, req: &ChatPostMessageRequest<'_>) -> Result<String, ChannelError> {
        debug!(channel = req.channel, "Slack chat.postMessage");
        let result: ChatPostResult = self.bot_post_json("chat.postMessage", req).await?;
        result
            .ts
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ChannelError::MessageSendFailed("chat.postMessage returned no ts".into()))
    }

    /// `chat.update` — used for streaming edits.
    pub async fn update_message(&self, req: &ChatUpdateRequest<'_>) -> Result<(), ChannelError> {
        debug!(channel = req.channel, ts = req.ts, "Slack chat.update");
        let _: ChatPostResult = self.bot_post_json("chat.update", req).await?;
        Ok(())
    }

    async fn bot_post_empty<T: serde::de::DeserializeOwned>(&self, method: &str) -> Result<T, ChannelError> {
        let url = format!("{SLACK_API_BASE}/{method}");
        let resp: SlackApiResponse<T> = self
            .client
            .post(&url)
            .bearer_auth(&self.bot_token)
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("{method} request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("{method} parse failed: {e}")))?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::ConnectionFailed(format!("Slack {method} failed: {err}")));
        }
        Ok(resp.data)
    }

    async fn bot_post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &B,
    ) -> Result<T, ChannelError> {
        let url = format!("{SLACK_API_BASE}/{method}");
        let resp: SlackApiResponse<T> = self
            .client
            .post(&url)
            .bearer_auth(&self.bot_token)
            .json(body)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("{method} request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("{method} parse failed: {e}")))?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::MessageSendFailed(format!("Slack {method} failed: {err}")));
        }
        Ok(resp.data)
    }
}
