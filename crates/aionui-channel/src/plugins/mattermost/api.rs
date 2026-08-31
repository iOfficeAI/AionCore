use reqwest::{Client, StatusCode};

use crate::error::ChannelError;

use super::types::{CreatePostRequest, MattermostUser, PatchPostRequest};

#[derive(Clone)]
pub(crate) struct MattermostApi {
    client: Client,
    server_url: String,
    access_token: String,
}

impl MattermostApi {
    pub fn new(client: Client, server_url: String, access_token: String) -> Self {
        Self {
            client,
            server_url,
            access_token,
        }
    }

    pub async fn get_me(&self) -> Result<MattermostUser, ChannelError> {
        let url = format!("{}/api/v4/users/me", self.server_url);
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Mattermost user request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(mattermost_status_error("Mattermost user request failed", status));
        }

        response
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Mattermost user response parse failed: {e}")))
    }

    pub async fn create_post(&self, req: &CreatePostRequest) -> Result<String, ChannelError> {
        let url = format!("{}/api/v4/posts", self.server_url);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.access_token)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost post request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(ChannelError::MessageSendFailed(format!(
                "Mattermost post request failed with status {status}"
            )));
        }

        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost post response parse failed: {e}")))?;
        value["id"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ChannelError::MessageSendFailed("Mattermost post response missing id".into()))
    }

    pub async fn patch_post(&self, post_id: &str, req: &PatchPostRequest) -> Result<(), ChannelError> {
        let url = format!("{}/api/v4/posts/{post_id}/patch", self.server_url);
        let response = self
            .client
            .put(url)
            .bearer_auth(&self.access_token)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost post patch request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(ChannelError::MessageSendFailed(format!(
                "Mattermost post patch request failed with status {status}"
            )));
        }

        Ok(())
    }

    pub fn websocket_url(&self) -> String {
        let ws_base = if let Some(rest) = self.server_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.server_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.server_url.clone()
        };
        format!("{ws_base}/api/v4/websocket")
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}

fn mattermost_status_error(context: &str, status: StatusCode) -> ChannelError {
    ChannelError::ConnectionFailed(format!("{context} with status {status}"))
}
