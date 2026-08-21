//! Delivery semantics.
//!
//! The core invariant: delivery DELEGATES to
//! `ConversationService::send_message` — the human send path — instead of
//! writing a second one. That makes "cross-session delivery ≡ a human pressing
//! send" a structural fact, and it makes the team-conversation rejection free:
//! that path already refuses team-owned conversations
//! (`aionui-conversation/src/service.rs`, the `team_id_from_extra` guard in
//! `send_message`), so no matter where an agent obtained a team conversation
//! id, it gets a 403.
//!
//! `send_message`, never `run_agent_turn`: the former ends in
//! `spawn_user_turn` and returns immediately; the latter awaits the whole turn
//! and would let one target block the single drainer for minutes.

use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    SendMessageRequest, SendMessageResponse, SessionDeliveryStatus, SessionMessageRateLimitedPayload, SessionRateGate,
    SessionSendMessageRequest, SessionSendMessageResponse, WebSocketMessage,
};
use aionui_common::constants::{AIONUI_SESSION_MESSAGE_END_MARKER, AIONUI_SESSION_MESSAGE_MARKER};
use aionui_conversation::session_mentions::{team_id_from_extra_str, workspace_from_extra};
use aionui_conversation::{ConversationError, ConversationService};
use aionui_db::{IConversationRepository, ISettingsRepository};
use aionui_realtime::EventBroadcaster;
use async_trait::async_trait;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::drainer::{DeliveryOutcome, DeliverySink, DrainGate};
use crate::error::SessionMessageError;
use crate::queue::{DeliveryQueue, PendingDelivery, TTL_MS};
use crate::rate_limit::{RateLimiter, RateVerdict};

/// Build the recipient-side block.
///
/// The short reply pointer after `reply_to` is deliberate (~10-15 tokens): the
/// recipient must know it can reply at all, or the reply path is dead. The full
/// schema does not go here — that is what `session capabilities` is for.
pub fn build_session_message_block(from_name: &str, from_id: &str, workspace_field: &str, reply_to: &str) -> String {
    format!(
        "{AIONUI_SESSION_MESSAGE_MARKER}\n\
         from: {from_name}\t{from_id}\n\
         workspace: {workspace_field}\n\
         reply_to: {reply_to}\t（回信: session send-message, to=reply_to）\n\
         {AIONUI_SESSION_MESSAGE_END_MARKER}"
    )
}

/// Block first, then the body: the block is context, not an attachment.
pub fn compose_delivery_content(block: &str, message: &str) -> String {
    format!("{block}\n\n{message}")
}

/// `workspace:` value for the recipient block.
///
/// Reports the SENDER's path, because that is what the recipient cannot infer.
/// An unknown sender workspace renders as `unknown` — never `same` — for the
/// same reason as the sender-side block: claiming `same` would tell the agent
/// relative paths are safe when we do not know that.
fn recipient_workspace_field(sender_workspace: Option<&str>, target_workspace: Option<&str>) -> String {
    match (sender_workspace, target_workspace) {
        (Some(sender), Some(target)) if sender == target => "same".to_owned(),
        (Some(sender), _) => format!("{sender}（与你不同，勿用相对路径，勿假设可读）"),
        (None, _) => "unknown（与你不同，勿用相对路径，勿假设可读）".to_owned(),
    }
}

pub struct SessionMessageDeps {
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub settings_repo: Arc<dyn ISettingsRepository>,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub broadcaster: Arc<dyn EventBroadcaster>,
    pub queue: Arc<DeliveryQueue>,
    pub rate_limiter: Arc<RateLimiter>,
    pub notify: Arc<Notify>,
}

pub struct SessionMessageService {
    deps: SessionMessageDeps,
}

impl SessionMessageService {
    pub fn new(deps: SessionMessageDeps) -> Self {
        Self { deps }
    }

    pub fn queue(&self) -> &Arc<DeliveryQueue> {
        &self.deps.queue
    }

    /// The global toggle, per user (`system_settings.user_id` is its PK).
    /// A read failure is treated as ENABLED — the toggle is an opt-out panic
    /// button, and a transient DB error must not silently disable a feature the
    /// user left on.
    pub async fn is_enabled_for(&self, user_id: &str) -> bool {
        match self.deps.settings_repo.get_settings(user_id).await {
            Ok(Some(settings)) => settings.cross_session_message_enabled,
            // No row yet → defaults, and the default is on.
            Ok(None) => true,
            Err(error) => {
                warn!(
                    user_id,
                    error = %error,
                    "cross-session toggle lookup failed; treating the feature as enabled"
                );
                true
            }
        }
    }

    /// Validation order is FIXED (spec §6.3). When several conditions hold at
    /// once, which code comes back must be deterministic or the tests are
    /// flaky:
    ///
    /// 1. `feature_disabled` — most global, cheapest
    /// 2. `sender_is_team`
    /// 3. `schema_validation_failed`
    /// 4. `rate_limited` — BEFORE the target lookup: during a spin storm we
    ///    must not pay a DB read per junk request
    /// 5. target checks — `target_is_self` / `target_not_found` / `target_is_team`
    /// 6. deliver via the human send path
    /// 7. queue on busy — only here can `queue_full` happen
    pub async fn send(
        &self,
        user_id: &str,
        from_conversation_id: &str,
        req: &SessionSendMessageRequest,
    ) -> Result<SessionSendMessageResponse, SessionMessageError> {
        // 1
        if !self.is_enabled_for(user_id).await {
            warn!(
                from_conversation_id,
                outcome = "rejected",
                error_code = "feature_disabled",
                "cross-session delivery refused"
            );
            return Err(SessionMessageError::FeatureDisabled);
        }

        // 2 — the sender's own row. A teammate's agent holds a
        // ConversationHelper token too, so without this check it could use this
        // CLI to reach ordinary conversations, bypassing the team mailbox.
        let sender = self.load_sender(user_id, from_conversation_id).await?;
        if team_id_from_extra_str(&sender.extra).is_some() {
            warn!(
                from_conversation_id,
                outcome = "rejected",
                error_code = "sender_is_team",
                "cross-session delivery refused"
            );
            return Err(SessionMessageError::SenderIsTeam {
                id: from_conversation_id.to_owned(),
            });
        }

        // 3
        let to = req.to.trim();
        if to.is_empty() || to == "*" {
            return Err(SessionMessageError::SchemaValidation {
                reason: "`to` must be a conversation id; names and `*` are not supported".to_owned(),
            });
        }
        if req.message.trim().is_empty() {
            return Err(SessionMessageError::SchemaValidation {
                reason: "`message` must not be empty".to_owned(),
            });
        }

        // 4
        if let RateVerdict::Tripped { gate, window_count } =
            self.deps.rate_limiter.check_and_record(from_conversation_id, to)
        {
            self.broadcast_rate_limited(user_id, &sender.name, from_conversation_id, to, gate, window_count)
                .await;
            warn!(
                from_conversation_id,
                to_conversation_id = to,
                window_count,
                gate = ?gate,
                outcome = "rejected",
                error_code = "rate_limited",
                "cross-session rate gate tripped"
            );
            return Err(SessionMessageError::RateLimited);
        }

        // 5 — re-checked here even though the send boundary already validated
        // the `@@` reference: there is real elapsed time between the user
        // picking a target and the agent calling the CLI minutes later, during
        // which the target may have been deleted or joined a team.
        if to == from_conversation_id {
            return Err(SessionMessageError::TargetIsSelf { id: to.to_owned() });
        }
        let target = self
            .deps
            .conversation_repo
            .get(user_id, to)
            .await
            .map_err(|error| SessionMessageError::TransportUnavailable {
                reason: error.to_string(),
            })?
            // Scoped by user_id, so another user's id lands here: 404, not 403
            // — refuse without leaking that the id exists.
            .ok_or_else(|| SessionMessageError::TargetNotFound { id: to.to_owned() })?;
        if team_id_from_extra_str(&target.extra).is_some() {
            return Err(SessionMessageError::TargetIsTeam { id: to.to_owned() });
        }

        // 6
        let sender_workspace = workspace_from_extra(&sender.extra);
        let target_workspace = workspace_from_extra(&target.extra);
        let block = build_session_message_block(
            &sender.name,
            from_conversation_id,
            &recipient_workspace_field(sender_workspace.as_deref(), target_workspace.as_deref()),
            from_conversation_id,
        );
        let content = compose_delivery_content(&block, &req.message);

        match self.deliver_now(user_id, to, content.clone()).await {
            Ok(sent) => {
                info!(
                    from_conversation_id,
                    to_conversation_id = to,
                    outcome = "delivered",
                    // `send_message` may have merged the message into the
                    // target's already-running turn instead of opening a new
                    // one. Both map to `delivered` on the wire, but without this
                    // field the two are indistinguishable in production.
                    delivered_midturn = sent.delivered_midturn,
                    "cross-session delivery accepted"
                );
                Ok(SessionSendMessageResponse {
                    status: SessionDeliveryStatus::Delivered,
                    to: to.to_owned(),
                })
            }
            // 7
            Err(DeliverAttemptError::Transient(reason)) => {
                // `content` already carries the recipient block, composed above
                // while both rows were in hand — the drainer re-sends these
                // bytes verbatim rather than rebuilding anything.
                self.deps.queue.push(PendingDelivery {
                    to: to.to_owned(),
                    user_id: user_id.to_owned(),
                    from_conversation_id: from_conversation_id.to_owned(),
                    message: content,
                    expires_at_ms: self.deps.queue.clock().now_ms() + TTL_MS,
                })?;
                self.deps.notify.notify_one();
                info!(
                    from_conversation_id,
                    to_conversation_id = to,
                    outcome = "queued",
                    // Which transient condition it was — a busy turn, a pending
                    // confirmation card, or a runtime restart. Logged once per
                    // enqueue (never per retry, which would be one line a
                    // second), and it is an error string, never a payload.
                    reason = %reason,
                    "cross-session delivery queued; target not ready"
                );
                Ok(SessionSendMessageResponse {
                    status: SessionDeliveryStatus::Queued,
                    to: to.to_owned(),
                })
            }
            Err(DeliverAttemptError::Hard(reason)) => Err(SessionMessageError::TransportUnavailable { reason }),
        }
    }

    /// `list` / `targets` / `mentionable` share the send path's two most global
    /// checks: the feature toggle and the team-sender rule. `capabilities` does
    /// NOT go through this — it is a static contract that touches no
    /// conversation data, so it stays available even when the feature is off.
    ///
    /// An EMPTY `from_conversation_id` skips the team-sender check rather than
    /// failing. The runtime route always has a token-bound caller, but the
    /// `@@` picker's `current_conversation_id` is optional (a picker opened
    /// outside a conversation has no sender to validate). Treating "no sender"
    /// as a missing row would turn the toggle check into a 409.
    pub async fn guard_list_access(
        &self,
        user_id: &str,
        from_conversation_id: &str,
    ) -> Result<(), SessionMessageError> {
        if !self.is_enabled_for(user_id).await {
            return Err(SessionMessageError::FeatureDisabled);
        }
        if from_conversation_id.is_empty() {
            return Ok(());
        }
        let sender = self.load_sender(user_id, from_conversation_id).await?;
        if team_id_from_extra_str(&sender.extra).is_some() {
            return Err(SessionMessageError::SenderIsTeam {
                id: from_conversation_id.to_owned(),
            });
        }
        Ok(())
    }

    /// The caller's own row. A missing row means the runtime token named a
    /// conversation that no longer exists, which is a transport-level problem
    /// rather than a target problem — hence `TransportUnavailable`, not
    /// `TargetNotFound`.
    async fn load_sender(
        &self,
        user_id: &str,
        from_conversation_id: &str,
    ) -> Result<aionui_db::models::ConversationRow, SessionMessageError> {
        self.deps
            .conversation_repo
            .get(user_id, from_conversation_id)
            .await
            .map_err(|error| SessionMessageError::TransportUnavailable {
                reason: error.to_string(),
            })?
            .ok_or_else(|| SessionMessageError::TransportUnavailable {
                reason: format!("sender conversation missing: {from_conversation_id}"),
            })
    }

    /// One attempt through the human send path. `content` already carries the
    /// recipient block.
    ///
    /// Returns the whole `SendMessageResponse` rather than `()` so callers can
    /// log `delivered_midturn`. Note what `Busy` means here: `send_message`
    /// handles mid-turn delivery internally, so `Busy` covers BOTH "backend
    /// cannot take a mid-turn message" AND "it can, but that turn is blocked on
    /// a pending confirmation card". The second case means even a claude/codex
    /// target can queue.
    async fn deliver_now(
        &self,
        user_id: &str,
        to: &str,
        content: String,
    ) -> Result<SendMessageResponse, DeliverAttemptError> {
        let request = SendMessageRequest {
            content,
            files: Vec::new(),
            // The delivered body is already fully composed; `@@` resolution is
            // a sender-side concern and must not re-run here.
            sessions: Vec::new(),
            inject_skills: Vec::new(),
            // Not hidden: the user opening the recipient conversation should
            // see this message. (cron uses hidden=true because its prompt is
            // template boilerplate; this is real content.)
            hidden: false,
        };
        match self
            .deps
            .conversation_service
            .send_message(user_id, to, request, &self.deps.task_manager)
            .await
        {
            Ok(response) => Ok(response),
            Err(error) => Err(classify_delivery_failure(error)),
        }
    }

    async fn broadcast_rate_limited(
        &self,
        user_id: &str,
        from_name: &str,
        from_conversation_id: &str,
        to: &str,
        gate: SessionRateGate,
        window_count: u32,
    ) {
        let to_name = self
            .deps
            .conversation_repo
            .get(user_id, to)
            .await
            .ok()
            .flatten()
            .map_or_else(|| to.to_owned(), |row| row.name);
        let payload = SessionMessageRateLimitedPayload {
            user_id: user_id.to_owned(),
            from_conversation_id: from_conversation_id.to_owned(),
            from_name: from_name.to_owned(),
            to_conversation_id: to.to_owned(),
            to_name,
            window_count,
            gate,
        };
        let Ok(value) = serde_json::to_value(&payload) else {
            warn!("failed to serialise sessionMessage.rateLimited payload");
            return;
        };
        self.deps
            .broadcaster
            .broadcast(WebSocketMessage::new("sessionMessage.rateLimited", value));
    }
}

#[derive(Debug)]
enum DeliverAttemptError {
    /// Try again on the next tick. Carries the rendered cause for logging only.
    Transient(String),
    /// A real answer: drop the item and `warn`.
    Hard(String),
}

/// Which `send_message` failures deserve another tick.
///
/// TRANSIENT is not just "busy". The second arm is a fixed regression: a user
/// restarting a conversation's runtime while messages were queued for it saw
/// them vanish, because `RuntimeRestarting` fell into the catch-all and the
/// drainer dropped the head. The cancel hook had deliberately KEPT that queue
/// (`TurnCancelCause::RuntimeRestart`) and was undone one tick later — silent
/// message loss, the worst failure mode this feature has, plus it made the
/// hook's cause distinction meaningless.
///
/// A restart is a ~1s window after which the conversation is IDLE — precisely
/// what a pending delivery is waiting for — so it is retried, not discarded.
/// The 10-minute TTL still bounds it: a restart that never finishes ends in an
/// expiry `warn`, not an unbounded retry loop.
///
/// Everything else is a real answer (gone, refused, archived) and must still be
/// dropped, or a bad reference would be retried until its TTL for no reason.
fn classify_delivery_failure(error: ConversationError) -> DeliverAttemptError {
    match &error {
        ConversationError::Busy { .. } | ConversationError::RuntimeRestarting { .. } => {
            DeliverAttemptError::Transient(error.to_string())
        }
        _ => DeliverAttemptError::Hard(error.to_string()),
    }
}

#[async_trait]
impl DeliverySink for SessionMessageService {
    async fn deliver(&self, item: &PendingDelivery) -> DeliveryOutcome {
        match self.deliver_now(&item.user_id, &item.to, item.message.clone()).await {
            Ok(_) => DeliveryOutcome::Delivered,
            Err(DeliverAttemptError::Transient(_)) => DeliveryOutcome::Busy,
            Err(DeliverAttemptError::Hard(reason)) => DeliveryOutcome::HardError(reason),
        }
    }
}

#[async_trait]
impl DrainGate for SessionMessageService {
    async fn is_enabled_for(&self, user_id: &str) -> bool {
        SessionMessageService::is_enabled_for(self, user_id).await
    }
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
