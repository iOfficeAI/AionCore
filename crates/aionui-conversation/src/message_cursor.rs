use aionui_db::MessagePageCursor;
use base64::Engine;

use crate::ConversationError;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct MessageCursorV1 {
    created_at: i64,
    id: String,
}

pub fn encode_message_cursor(cursor: &MessagePageCursor) -> Result<String, ConversationError> {
    let json = serde_json::to_vec(&MessageCursorV1 {
        created_at: cursor.created_at,
        id: cursor.id.clone(),
    })
    .map_err(|e| ConversationError::internal(format!("Failed to encode message cursor: {e}")))?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    Ok(format!("v1.{encoded}"))
}

pub fn decode_message_cursor(raw: &str) -> Result<MessagePageCursor, ConversationError> {
    let encoded = raw
        .strip_prefix("v1.")
        .ok_or_else(|| ConversationError::bad_request("invalid message cursor"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ConversationError::bad_request("invalid message cursor"))?;
    let cursor: MessageCursorV1 =
        serde_json::from_slice(&bytes).map_err(|_| ConversationError::bad_request("invalid message cursor"))?;
    if cursor.id.is_empty() {
        return Err(ConversationError::bad_request("invalid message cursor"));
    }
    Ok(MessagePageCursor {
        created_at: cursor.created_at,
        id: cursor.id,
    })
}

#[cfg(test)]
mod tests {
    use aionui_db::MessagePageCursor;

    use super::*;

    #[test]
    fn message_cursor_round_trips_created_at_and_id() {
        let key = MessagePageCursor {
            created_at: 1234,
            id: "msg-b".to_string(),
        };

        let cursor = encode_message_cursor(&key).unwrap();

        assert_eq!(decode_message_cursor(&cursor).unwrap(), key);
    }

    #[test]
    fn message_cursor_rejects_invalid_shapes() {
        for raw in ["", "v2.abc", "v1.%%%%", "v1.e30", "v1.eyJjcmVhdGVkX2F0IjoxLCJpZCI6IiJ9"] {
            let err = decode_message_cursor(raw).unwrap_err();
            assert!(matches!(err, ConversationError::BadRequest { reason } if reason == "invalid message cursor"));
        }
    }
}
