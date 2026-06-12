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
        }
    }
}

impl fmt::Display for TeamWakeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
