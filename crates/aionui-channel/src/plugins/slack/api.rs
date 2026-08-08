use std::time::Duration;

use reqwest::Client;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use crate::error::ChannelError;

use super::types::{
    AuthTestResponse, OpenConnectionResponse, PostMessageRequest, PostMessageResponse, UpdateMessageRequest,
    UpdateMessageResponse,
};

const SLACK_API_BASE: &str = "https://slack.com/api";

/// Cap on how long we honor a Slack rate-limit `Retry-After` before giving up.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Parse Slack's `Retry-After` header (integer seconds) into a capped delay.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.min(MAX_RETRY_AFTER.as_secs())))
}

/// Identity returned by `auth.test`.
#[derive(Debug, Clone)]
pub(super) struct SlackIdentity {
    /// Bot user id (`Uxxxx`) — used to filter the bot's own messages and to
    /// strip its `<@id>` mention from `app_mention` text.
    pub user_id: String,
    /// Human-readable bot name, when Slack returns one.
    pub user: Option<String>,
}

/// HTTP client for the Slack Web API.
///
/// Holds both tokens: the bot token (`xoxb-`) authorizes Web API calls
/// (`auth.test` / `chat.postMessage` / `chat.update`), while the app-level
/// token (`xapp-`) authorizes only `apps.connections.open` for Socket Mode.
pub(super) struct SlackApi {
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

    /// Validate the bot token and fetch the bot's identity (`auth.test`).
    pub async fn auth_test(&self) -> Result<SlackIdentity, ChannelError> {
        let url = format!("{SLACK_API_BASE}/auth.test");
        let resp: AuthTestResponse = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Slack auth.test request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Slack auth.test parse failed: {e}")))?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::InvalidConfig(format!("Slack auth.test error: {err}")));
        }

        Ok(SlackIdentity {
            user_id: resp.user_id.unwrap_or_default(),
            user: resp.user,
        })
    }

    /// Open a Socket Mode connection and return the WSS URL
    /// (`apps.connections.open`, authorized by the app-level token).
    pub async fn open_connection(&self) -> Result<String, ChannelError> {
        let url = format!("{SLACK_API_BASE}/apps.connections.open");
        let resp: OpenConnectionResponse = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.app_token))
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Slack apps.connections.open request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Slack apps.connections.open parse failed: {e}")))?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::ConnectionFailed(format!(
                "Slack apps.connections.open error: {err}"
            )));
        }

        resp.url
            .ok_or_else(|| ChannelError::ConnectionFailed("Slack apps.connections.open returned no url".into()))
    }

    /// Post a message to a channel (`chat.postMessage`). Returns the message
    /// `ts`, used as the message id for later edits.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, ChannelError> {
        let url = format!("{SLACK_API_BASE}/chat.postMessage");
        let body = PostMessageRequest {
            channel: channel.to_string(),
            text: text.to_string(),
            thread_ts: thread_ts.map(ToOwned::to_owned),
        };

        debug!(channel, "Slack chat.postMessage");

        let resp: PostMessageResponse = self.post_json(&url, &body).await?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::MessageSendFailed(format!(
                "Slack chat.postMessage error: {err}"
            )));
        }

        resp.ts
            .ok_or_else(|| ChannelError::MessageSendFailed("Slack chat.postMessage returned no ts".into()))
    }

    /// Edit an existing message (`chat.update`).
    pub async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), ChannelError> {
        let url = format!("{SLACK_API_BASE}/chat.update");
        let body = UpdateMessageRequest {
            channel: channel.to_string(),
            ts: ts.to_string(),
            text: text.to_string(),
        };

        debug!(channel, ts, "Slack chat.update");

        let resp: UpdateMessageResponse = self.post_json(&url, &body).await?;

        if !resp.ok {
            let err = resp.error.unwrap_or_else(|| "unknown".into());
            return Err(ChannelError::MessageSendFailed(format!(
                "Slack chat.update error: {err}"
            )));
        }

        Ok(())
    }

    /// POST a JSON body with the bot token and parse the response, retrying
    /// once on HTTP 429 after honoring the `Retry-After` header.
    async fn post_json<B, R>(&self, url: &str, body: &B) -> Result<R, ChannelError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        let mut resp = self.send_once(url, body).await?;
        if resp.status().as_u16() == 429 {
            let delay = parse_retry_after(resp.headers()).unwrap_or_else(|| Duration::from_secs(1));
            warn!(
                delay_secs = delay.as_secs(),
                "Slack rate limited (429); retrying after Retry-After"
            );
            tokio::time::sleep(delay).await;
            resp = self.send_once(url, body).await?;
        }
        resp.json::<R>()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Slack response parse failed: {e}")))
    }

    /// Send a single authenticated POST with the bot token.
    async fn send_once<B: Serialize>(&self, url: &str, body: &B) -> Result<reqwest::Response, ChannelError> {
        self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(body)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Slack request failed: {e}")))
    }
}

#[cfg(test)]
#[path = "api_test.rs"]
mod api_test;
