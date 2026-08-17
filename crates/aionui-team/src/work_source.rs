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
    LeadIntervention,
    McpSendMessage,
    McpShutdownRequest,
    SpawnWelcome,
    TeamMembershipChanged,
    SpawnAttachFailure,
    /// A teammate exhausted its mailbox delivery retries and was paused. Wakes
    /// the lead so a stalled teammate cannot go unnoticed. Deliberately does not
    /// resume a paused slot: it is addressed to the lead, not the stalled slot.
    DeliveryFailureNotification,
    IdleNotification,
    InterruptedNotification,
    ShutdownRejected,
    RecoveryDrain,
}

impl WorkSource {
    pub(crate) fn priority(self) -> WorkPriority {
        match self {
            Self::UserMessage | Self::UserCommand | Self::UserIntervention | Self::LeadIntervention => {
                WorkPriority::Foreground
            }
            Self::McpShutdownRequest | Self::ShutdownRejected => WorkPriority::Control,
            Self::McpSendMessage => WorkPriority::Directed,
            Self::SpawnWelcome
            | Self::TeamMembershipChanged
            | Self::SpawnAttachFailure
            | Self::DeliveryFailureNotification
            | Self::IdleNotification
            | Self::InterruptedNotification
            | Self::RecoveryDrain => WorkPriority::Background,
        }
    }

    pub(crate) fn resumes_paused_slot(self) -> bool {
        matches!(
            self,
            Self::UserMessage | Self::UserCommand | Self::UserIntervention | Self::LeadIntervention
        )
    }

    pub(crate) fn requires_mailbox_message(self) -> bool {
        matches!(
            self,
            Self::UserMessage
                | Self::UserCommand
                | Self::UserIntervention
                | Self::LeadIntervention
                | Self::McpSendMessage
                | Self::McpShutdownRequest
                | Self::SpawnWelcome
                | Self::SpawnAttachFailure
                | Self::DeliveryFailureNotification
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
            Self::LeadIntervention => "lead_intervention",
            Self::McpSendMessage => "mcp_send_message",
            Self::McpShutdownRequest => "mcp_shutdown_request",
            Self::SpawnWelcome => "spawn_welcome",
            Self::TeamMembershipChanged => "team_membership_changed",
            Self::SpawnAttachFailure => "spawn_attach_failure",
            Self::DeliveryFailureNotification => "delivery_failure_notification",
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

    #[test]
    fn mcp_send_message_uses_the_directed_lane() {
        assert_eq!(WorkSource::McpSendMessage.priority(), WorkPriority::Directed);
    }

    #[test]
    fn delivery_failure_notification_wakes_the_lead_without_resuming_a_paused_slot() {
        assert_eq!(
            WorkSource::DeliveryFailureNotification.priority(),
            WorkPriority::Background
        );
        assert!(WorkSource::DeliveryFailureNotification.requires_mailbox_message());
        assert!(
            !WorkSource::DeliveryFailureNotification.resumes_paused_slot(),
            "the notice is addressed to the lead; it must not un-pause the stalled slot"
        );
        assert_eq!(
            WorkSource::DeliveryFailureNotification.to_string(),
            "delivery_failure_notification"
        );
    }

    #[test]
    fn lead_intervention_is_foreground_mailbox_work_that_resumes_pause() {
        assert_eq!(WorkSource::LeadIntervention.priority(), WorkPriority::Foreground);
        assert!(WorkSource::LeadIntervention.resumes_paused_slot());
        assert!(WorkSource::LeadIntervention.requires_mailbox_message());
    }
}
