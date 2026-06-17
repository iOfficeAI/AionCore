use std::sync::Arc;

use aionui_channel::action::{ActionExecutor, MessageResult};
use aionui_channel::channel_settings::ChannelSettingsService;
use aionui_channel::pairing::PairingService;
use aionui_channel::session::SessionManager;
use aionui_channel::types::{
    MessageContentType, PluginType, UnifiedIncomingMessage, UnifiedMessageContent, UnifiedUser,
};

fn make_text_message(user_id: &str, chat_id: &str, text: &str) -> UnifiedIncomingMessage {
    UnifiedIncomingMessage {
        id: "msg-1".into(),
        platform: PluginType::Telegram,
        chat_id: chat_id.into(),
        user: UnifiedUser {
            id: user_id.into(),
            username: None,
            display_name: "Test".into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: text.into(),
            attachments: None,
        },
        timestamp: 0,
        reply_to_message_id: None,
        action: None,
        raw: None,
    }
}

/// Unauthorized user should receive a pairing code response.
#[tokio::test]
async fn unauthorized_user_gets_pairing_response() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let repo: Arc<dyn aionui_db::IChannelRepository> = Arc::new(aionui_db::SqliteChannelRepository::new(pool.clone()));
    let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));

    let pref_repo: Arc<dyn aionui_db::IClientPreferenceRepository> =
        Arc::new(aionui_db::SqliteClientPreferenceRepository::new(pool));
    let settings = Arc::new(ChannelSettingsService::new(pref_repo));

    let pairing = Arc::new(PairingService::new(repo.clone(), bus));
    let session_mgr = Arc::new(SessionManager::new(repo));
    let executor = Arc::new(ActionExecutor::new(pairing, Arc::clone(&session_mgr), settings));

    let msg = make_text_message("unknown_user", "chat_1", "hello");
    let result = executor.handle_incoming_message(&msg).await.unwrap();

    match result {
        MessageResult::Action(response) => {
            let text = response.text.unwrap();
            assert!(text.len() > 5, "expected pairing response, got: {text}");
        }
        other => panic!("expected Action, got: {other:?}"),
    }
}
