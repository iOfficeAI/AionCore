use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::error::ChannelError;
use crate::types::PluginType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDevelopmentCommand {
    Project,
    RunInfo,
    DiffSummary,
    Test,
    Stop,
    Retry,
    Handoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelDevelopmentContext {
    pub source_user_id: String,
    pub conversation_id: Option<String>,
    pub platform: PluginType,
    pub chat_id: String,
    pub message_thread_id: Option<i64>,
}

#[async_trait]
pub trait ChannelDevelopmentPort: Send + Sync {
    async fn execute(
        &self,
        context: ChannelDevelopmentContext,
        command: ChannelDevelopmentCommand,
    ) -> Result<String, ChannelError>;
}

/// Creates short-lived, tamper-evident browser handoff links for channel users.
/// Authentication is still required by the Web application; the signature only
/// protects the route parameters carried by an untrusted chat client.
#[derive(Clone)]
pub struct DevelopmentHandoffSigner {
    secret: [u8; 32],
    base_path: String,
}

impl DevelopmentHandoffSigner {
    pub fn new(secret: [u8; 32], base_path: impl Into<String>) -> Self {
        let base_path = base_path.into();
        let base_path = if base_path.starts_with('/') {
            std::env::var("AIONUI_PUBLIC_URL")
                .ok()
                .map(|public_url| public_url.trim().trim_end_matches('/').to_owned())
                .filter(|public_url| public_url.starts_with("http://") || public_url.starts_with("https://"))
                .map_or(base_path.clone(), |public_url| format!("{public_url}{base_path}"))
        } else {
            base_path
        };
        Self { secret, base_path }
    }

    pub fn sign(&self, project_id: &str, run_id: &str, expires_at: i64) -> String {
        let payload = handoff_payload(project_id, run_id, expires_at);
        let signature = self.signature(&payload);
        format!(
            "{}?projectId={}&runId={}&expires={expires_at}&signature={signature}",
            self.base_path,
            percent_encode(project_id),
            percent_encode(run_id),
        )
    }

    pub fn verify(&self, project_id: &str, run_id: &str, expires_at: i64, signature: &str, now: i64) -> bool {
        if expires_at < now || expires_at.saturating_sub(now) > 24 * 60 * 60 * 1000 {
            return false;
        }
        let expected = self.signature(&handoff_payload(project_id, run_id, expires_at));
        constant_time_eq(expected.as_bytes(), signature.as_bytes())
    }

    fn signature(&self, payload: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(b"aionui-development-handoff-v1\0");
        digest.update(self.secret);
        digest.update(payload.as_bytes());
        digest.update(self.secret);
        digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

fn handoff_payload(project_id: &str, run_id: &str, expires_at: i64) -> String {
    format!("{project_id}\0{run_id}\0{expires_at}")
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![char::from(byte)]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}
