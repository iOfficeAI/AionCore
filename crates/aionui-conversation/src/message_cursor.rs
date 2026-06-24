use base64::Engine;

use crate::ConversationError;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct MessageCursorV1 {
    sequence: i64,
}

pub fn encode_message_cursor(sequence: i64) -> Result<String, ConversationError> {
    let json = serde_json::to_vec(&MessageCursorV1 { sequence })
        .map_err(|e| ConversationError::internal(format!("Failed to encode message cursor: {e}")))?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    Ok(format!("v1.{encoded}"))
}

pub fn decode_message_cursor(raw: &str) -> Result<i64, ConversationError> {
    let encoded = raw
        .strip_prefix("v1.")
        .ok_or_else(|| ConversationError::bad_request("invalid message cursor"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ConversationError::bad_request("invalid message cursor"))?;
    let cursor: MessageCursorV1 =
        serde_json::from_slice(&bytes).map_err(|_| ConversationError::bad_request("invalid message cursor"))?;
    if cursor.sequence < 1 {
        return Err(ConversationError::bad_request("invalid message cursor"));
    }
    Ok(cursor.sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_cursor_round_trips_sequence() {
        let cursor = encode_message_cursor(42).unwrap();
        assert_eq!(decode_message_cursor(&cursor).unwrap(), 42);
    }

    #[test]
    fn message_cursor_rejects_invalid_shapes() {
        for raw in ["", "v2.abc", "v1.%%%%", "v1.e30", "v1.eyJzZXF1ZW5jZSI6MH0"] {
            let err = decode_message_cursor(raw).unwrap_err();
            assert!(matches!(err, ConversationError::BadRequest { reason } if reason == "invalid message cursor"));
        }
    }
}
