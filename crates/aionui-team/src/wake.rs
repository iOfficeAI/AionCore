use std::fmt;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeamWakeSource {
    UserMessage,
    UserIntervention,
    McpSendMessage,
    McpShutdownRequest,
    SpawnWelcome,
    SpawnAttachFailure,
    IdleNotification,
    InterruptedNotification,
    CrashNotification,
    InactivityTimeout,
    ShutdownRejected,
    RecoveryDrain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeamWakeClass {
    Foreground,
    Background,
    SystemRecovery,
    Lifecycle,
}

impl TeamWakeSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::UserIntervention => "user_intervention",
            Self::McpSendMessage => "mcp_send_message",
            Self::McpShutdownRequest => "mcp_shutdown_request",
            Self::SpawnWelcome => "spawn_welcome",
            Self::SpawnAttachFailure => "spawn_attach_failure",
            Self::IdleNotification => "idle_notification",
            Self::InterruptedNotification => "interrupted_notification",
            Self::CrashNotification => "crash_notification",
            Self::InactivityTimeout => "inactivity_timeout",
            Self::ShutdownRejected => "shutdown_rejected",
            Self::RecoveryDrain => "recovery_drain",
        }
    }

    pub(crate) fn class(self) -> TeamWakeClass {
        match self {
            Self::UserMessage | Self::UserIntervention => TeamWakeClass::Foreground,
            Self::McpSendMessage | Self::IdleNotification | Self::InterruptedNotification => TeamWakeClass::Background,
            Self::CrashNotification
            | Self::InactivityTimeout
            | Self::SpawnAttachFailure
            | Self::ShutdownRejected
            | Self::RecoveryDrain => TeamWakeClass::SystemRecovery,
            Self::SpawnWelcome | Self::McpShutdownRequest => TeamWakeClass::Lifecycle,
        }
    }

    pub(crate) fn bypasses_pause(self) -> bool {
        matches!(self.class(), TeamWakeClass::Foreground | TeamWakeClass::Lifecycle)
    }

    pub(crate) fn resumes_paused_slot(self) -> bool {
        matches!(self, Self::UserMessage | Self::UserIntervention)
    }
}

impl fmt::Display for TeamWakeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
