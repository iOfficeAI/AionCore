use crate::work_coordinator::WorkPriority;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkSource {
    UserMessage,
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
            Self::UserMessage | Self::UserIntervention => WorkPriority::Foreground,
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
        matches!(self, Self::UserMessage | Self::UserIntervention)
    }

    pub(crate) fn requires_mailbox_message(self) -> bool {
        matches!(
            self,
            Self::UserMessage
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
