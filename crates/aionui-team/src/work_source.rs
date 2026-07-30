use crate::work_coordinator::WorkPriority;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkSource {
    UserMessage,
    /// A user message recognized as a native backend slash command (e.g.
    /// `/compact`). Aligned with `UserMessage` semantics (same `Foreground`
    /// lane → FIFO, no preemption) but flagged so the coordinator batches it as
    /// a single-message turn and the wake path sends the bare command (ELECTRON-3RN).
    UserCommand,
    UserIntervention,
    McpSendMessage,
    McpShutdownRequest,
    SpawnWelcome,
    TeamMembershipChanged,
    SpawnAttachFailure,
    IdleNotification,
    InterruptedNotification,
    ShutdownRejected,
    RecoveryDrain,
}

impl WorkSource {
    pub(crate) fn priority(self) -> WorkPriority {
        match self {
            Self::UserMessage | Self::UserCommand | Self::UserIntervention => WorkPriority::Foreground,
            Self::McpShutdownRequest | Self::ShutdownRejected => WorkPriority::Control,
            Self::McpSendMessage
            | Self::SpawnWelcome
            | Self::TeamMembershipChanged
            | Self::SpawnAttachFailure
            | Self::IdleNotification
            | Self::InterruptedNotification
            | Self::RecoveryDrain => WorkPriority::Background,
        }
    }

    pub(crate) fn resumes_paused_slot(self) -> bool {
        matches!(self, Self::UserMessage | Self::UserCommand | Self::UserIntervention)
    }

    pub(crate) fn requires_mailbox_message(self) -> bool {
        matches!(
            self,
            Self::UserMessage
                | Self::UserCommand
                | Self::UserIntervention
                | Self::McpSendMessage
                | Self::McpShutdownRequest
                | Self::SpawnWelcome
                | Self::SpawnAttachFailure
                | Self::InterruptedNotification
                | Self::ShutdownRejected
                | Self::RecoveryDrain
        )
    }
}

impl fmt::Display for WorkSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::UserMessage => "user_message",
            Self::UserCommand => "user_command",
            Self::UserIntervention => "user_intervention",
            Self::McpSendMessage => "mcp_send_message",
            Self::McpShutdownRequest => "mcp_shutdown_request",
            Self::SpawnWelcome => "spawn_welcome",
            Self::TeamMembershipChanged => "team_membership_changed",
            Self::SpawnAttachFailure => "spawn_attach_failure",
            Self::IdleNotification => "idle_notification",
            Self::InterruptedNotification => "interrupted_notification",
            Self::ShutdownRejected => "shutdown_rejected",
            Self::RecoveryDrain => "recovery_drain",
        };
        formatter.write_str(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC3 (ELECTRON-3RN): `UserCommand` is aligned with `UserMessage` so it
    /// shares the Foreground lane (FIFO, no preemption) and mailbox/paused
    /// semantics, but carries its own `as_str` for the recognition log.
    #[test]
    fn user_command_matches_user_message_semantics() {
        assert_eq!(WorkSource::UserCommand.priority(), WorkPriority::Foreground);
        assert_eq!(WorkSource::UserCommand.priority(), WorkSource::UserMessage.priority());
        assert!(WorkSource::UserCommand.resumes_paused_slot());
        assert!(WorkSource::UserCommand.requires_mailbox_message());
        assert_eq!(WorkSource::UserCommand.to_string(), "user_command");
    }
}
