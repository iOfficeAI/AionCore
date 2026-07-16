use reqwest::Client;
use tracing::{debug, warn};

use crate::error::ChannelError;

use super::types::{
    AnswerCallbackQueryRequest, BotCommand, BotCommandScope, EditMessageTextRequest, SendChatActionRequest,
    SendMessageRequest, SetMyCommandsRequest, TgMessage, TgResponse, TgUpdate, TgUser,
};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// HTTP client for the Telegram Bot API.
///
/// Wraps `reqwest::Client` and a bot token. Provides typed methods
/// for `getMe`, `getUpdates`, `sendMessage`, `editMessageText`, and
/// `answerCallbackQuery`.
pub(crate) struct TelegramApi {
    client: Client,
    base_url: String,
}

impl TelegramApi {
    pub async fn get_chat_member_status(&self, chat_id: i64, user_id: i64) -> Result<String, ChannelError> {
        let url = format!("{}/getChatMember", self.base_url);
        let resp: TgResponse<serde_json::Value> = self
            .client
            .get(&url)
            .query(&[("chat_id", chat_id), ("user_id", user_id)])
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("getChatMember request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("getChatMember parse failed: {e}")))?;
        if !resp.ok {
            return Err(ChannelError::PlatformApi(
                resp.description.unwrap_or_else(|| "getChatMember failed".into()),
            ));
        }
        Ok(resp
            .result
            .and_then(|value| {
                value
                    .get("status")
                    .and_then(|status| status.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default())
    }
    /// Create a new API client for the given bot token.
    pub fn new(client: Client, token: &str) -> Self {
        Self {
            client,
            base_url: format!("{TELEGRAM_API_BASE}/bot{token}"),
        }
    }

    /// `getMe` — returns the bot's user identity.
    pub async fn get_me(&self) -> Result<TgUser, ChannelError> {
        let url = format!("{}/getMe", self.base_url);
        let resp: TgResponse<TgUser> = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("getMe request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("getMe parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!("Telegram getMe failed: {desc}")));
        }

        resp.result
            .ok_or_else(|| ChannelError::PlatformApi("getMe returned no result".into()))
    }

    /// `getUpdates` — long-poll for new updates.
    ///
    /// - `offset`: return updates with `update_id >= offset`
    /// - `timeout`: long-polling timeout in seconds (0 = short poll)
    pub async fn get_updates(&self, offset: Option<i64>, timeout: u32) -> Result<Vec<TgUpdate>, ChannelError> {
        let url = format!("{}/getUpdates", self.base_url);

        let mut params = vec![("timeout", timeout.to_string())];
        if let Some(off) = offset {
            params.push(("offset", off.to_string()));
        }

        let resp: TgResponse<Vec<TgUpdate>> = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("getUpdates request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("getUpdates parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            warn!("Telegram getUpdates error: {desc}");
            return Err(ChannelError::PlatformApi(format!("getUpdates failed: {desc}")));
        }

        Ok(resp.result.unwrap_or_default())
    }

    /// `sendMessage` — send a text message. Returns the sent message.
    pub async fn send_message(&self, req: &SendMessageRequest) -> Result<TgMessage, ChannelError> {
        let url = format!("{}/sendMessage", self.base_url);
        debug!(chat_id = req.chat_id, "Sending Telegram message");

        let resp: TgResponse<TgMessage> = self
            .client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("sendMessage request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("sendMessage parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            warn!(
                chat_id = req.chat_id,
                parse_mode = ?req.parse_mode,
                text_len = req.text.chars().count(),
                error = %desc,
                "Telegram sendMessage error"
            );
            return Err(ChannelError::MessageSendFailed(format!("sendMessage failed: {desc}")));
        }

        let sent = resp
            .result
            .ok_or_else(|| ChannelError::MessageSendFailed("sendMessage returned no result".into()))?;
        debug!(
            chat_id = req.chat_id,
            message_id = sent.message_id,
            parse_mode = ?req.parse_mode,
            text_len = req.text.chars().count(),
            "Telegram message sent"
        );
        Ok(sent)
    }

    /// `editMessageText` — edit an existing text message.
    pub async fn edit_message_text(&self, req: &EditMessageTextRequest) -> Result<(), ChannelError> {
        let url = format!("{}/editMessageText", self.base_url);
        debug!(
            chat_id = req.chat_id,
            message_id = req.message_id,
            "Editing Telegram message"
        );

        let resp: TgResponse<TgMessage> = self
            .client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("editMessageText request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("editMessageText parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "editMessageText failed: {desc}"
            )));
        }

        Ok(())
    }

    /// `answerCallbackQuery` — acknowledge a callback query.
    pub async fn answer_callback_query(&self, req: &AnswerCallbackQueryRequest) -> Result<(), ChannelError> {
        let url = format!("{}/answerCallbackQuery", self.base_url);

        let resp: TgResponse<bool> = self
            .client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("answerCallbackQuery request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("answerCallbackQuery parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            warn!("answerCallbackQuery error: {desc}");
        }

        Ok(())
    }

    /// `setMyCommands` — configure the Telegram slash-command menu.
    pub async fn set_my_commands(&self, commands: Vec<BotCommand>, scope: BotCommandScope) -> Result<(), ChannelError> {
        let url = format!("{}/setMyCommands", self.base_url);
        let req = SetMyCommandsRequest { commands, scope };

        let resp: TgResponse<bool> = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("setMyCommands request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("setMyCommands parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!("setMyCommands failed: {desc}")));
        }

        Ok(())
    }

    /// `sendChatAction` — show "typing..." or similar lightweight status.
    pub async fn send_chat_action(
        &self,
        chat_id: i64,
        action: &str,
        message_thread_id: Option<i64>,
    ) -> Result<(), ChannelError> {
        let url = format!("{}/sendChatAction", self.base_url);
        let req = SendChatActionRequest {
            chat_id,
            message_thread_id,
            action: action.to_owned(),
        };

        let resp: TgResponse<bool> = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("sendChatAction request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("sendChatAction parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            warn!("sendChatAction error: {desc}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_constructs_correct_base_url() {
        let client = Client::new();
        let api = TelegramApi::new(client, "123:ABC");
        assert_eq!(api.base_url, "https://api.telegram.org/bot123:ABC");
    }
}
