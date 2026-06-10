/// Explicit Team visibility decisions for a Team-originated message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamVisibilityPolicy {
    pub write_mailbox: bool,
    pub insert_user_visible_bubble: bool,
    pub insert_teammate_visible_bubble: bool,
    pub allow_hidden_conversation_message: bool,
    pub strip_system_notes: bool,
}

impl TeamVisibilityPolicy {
    pub fn user_message() -> Self {
        Self {
            write_mailbox: true,
            insert_user_visible_bubble: true,
            insert_teammate_visible_bubble: false,
            allow_hidden_conversation_message: false,
            strip_system_notes: true,
        }
    }

    pub fn teammate_message() -> Self {
        Self {
            write_mailbox: true,
            insert_user_visible_bubble: false,
            insert_teammate_visible_bubble: true,
            allow_hidden_conversation_message: false,
            strip_system_notes: false,
        }
    }

    pub fn hidden_runtime_message() -> Self {
        Self {
            write_mailbox: false,
            insert_user_visible_bubble: false,
            insert_teammate_visible_bubble: false,
            allow_hidden_conversation_message: true,
            strip_system_notes: false,
        }
    }
}

/// Remove `[SYSTEM NOTE: ...]` blocks from user-visible bubbles.
///
/// The original content may still be delivered to agents through mailbox/wake
/// input; this helper is only for UI projection.
pub fn strip_system_notes(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[SYSTEM NOTE:") {
        result.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find(']') {
            let mut next = &rest[start + end + 1..];
            if result.ends_with('\n') && next.starts_with('\n') {
                next = &next[1..];
            }
            rest = next;
        } else {
            rest = &rest[start..];
            break;
        }
    }
    result.push_str(rest);
    result.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_policy_is_visible_and_strips_notes() {
        let policy = TeamVisibilityPolicy::user_message();
        assert!(policy.write_mailbox);
        assert!(policy.insert_user_visible_bubble);
        assert!(!policy.insert_teammate_visible_bubble);
        assert!(!policy.allow_hidden_conversation_message);
        assert!(policy.strip_system_notes);
    }

    #[test]
    fn teammate_policy_is_visible_without_note_stripping() {
        let policy = TeamVisibilityPolicy::teammate_message();
        assert!(policy.write_mailbox);
        assert!(!policy.insert_user_visible_bubble);
        assert!(policy.insert_teammate_visible_bubble);
        assert!(!policy.allow_hidden_conversation_message);
        assert!(!policy.strip_system_notes);
    }

    #[test]
    fn strip_system_notes_removes_complete_blocks() {
        let got = strip_system_notes("Visible\n[SYSTEM NOTE: internal]\ntext");
        assert_eq!(got, "Visible\ntext");
    }
}
