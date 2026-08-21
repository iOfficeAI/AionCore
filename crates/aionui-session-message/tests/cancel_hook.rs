//! Where the cancel hook is attached, proven against a real
//! `ConversationService::cancel`.
//!
//! `cancel` has four branches and only two of them actually cancel anything.
//! Hooking the top of the function would pass every positive test here and
//! silently drop messages in production, so the negative assertion below is the
//! load-bearing one.

mod common;

use std::sync::{Arc, Mutex};

use aionui_api_types::{SessionDeliveryStatus, SessionSendMessageRequest};
use aionui_common::{OnConversationTurnCancelled, TurnCancelCause};
use aionui_session_message::QueueClearingCancelHook;
use common::{USER, setup};

fn request(to: &str, message: &str) -> SessionSendMessageRequest {
    SessionSendMessageRequest {
        to: to.to_owned(),
        message: message.to_owned(),
    }
}

/// Records the `cause` each notification carried, so the wiring between
/// `restart_runtime` and the hook can be asserted directly rather than inferred
/// from the queue's end state.
#[derive(Default)]
struct RecordingCancelHook {
    causes: Mutex<Vec<TurnCancelCause>>,
}

#[async_trait::async_trait]
impl OnConversationTurnCancelled for RecordingCancelHook {
    async fn on_turn_cancelled(&self, _user_id: &str, _conversation_id: &str, _turn_id: &str, cause: TurnCancelCause) {
        self.causes.lock().unwrap().push(cause);
    }
}

#[tokio::test]
async fn cancelling_the_active_turn_clears_the_queue_and_no_new_turn_starts() {
    let ctx = setup().await;
    ctx.conversation_service
        .with_turn_cancelled_hook(Arc::new(QueueClearingCancelHook::new(ctx.queue.clone())));
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    // Busy without mid-turn support → the delivery queues.
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .unwrap();
    assert_eq!(response.status, SessionDeliveryStatus::Queued);
    assert_eq!(ctx.queue.len_for("conv_target"), 1);

    let turn_id = ctx.active_turn_id("conv_target").expect("target is busy");
    ctx.conversation_service
        .cancel(USER, "conv_target", &turn_id, &ctx.task_manager)
        .await
        .unwrap();

    assert_eq!(ctx.queue.len_for("conv_target"), 0, "stop must actually stop");

    // And the drainer has nothing left to resurrect the conversation with.
    ctx.drainer().tick_once().await;
    assert_eq!(
        ctx.user_message_count("conv_target").await,
        0,
        "no new turn may be started after the cancel"
    );
}

/// The negative assertion that pins the hook's placement. `cancel` returns
/// `Ok` on a mismatched `turn_id` without cancelling anything, so a hook at the
/// top of the function would clear the queue here — silently losing a message
/// the user never asked to drop.
#[tokio::test]
async fn a_no_op_cancel_with_a_mismatched_turn_id_must_not_clear_the_queue() {
    let ctx = setup().await;
    ctx.conversation_service
        .with_turn_cancelled_hook(Arc::new(QueueClearingCancelHook::new(ctx.queue.clone())));
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    ctx.service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .unwrap();
    assert_eq!(ctx.queue.len_for("conv_target"), 1);

    ctx.conversation_service
        .cancel(USER, "conv_target", "turn_that_is_not_active", &ctx.task_manager)
        .await
        .expect("a mismatched turn id is still an Ok no-op");

    assert_eq!(
        ctx.queue.len_for("conv_target"),
        1,
        "a cancel that cancelled nothing must not clear the queue"
    );
}

/// The deferred branch: the turn id matches but no agent has registered yet.
/// That IS a real cancel intent (the orchestrator applies it as soon as the task
/// appears), so the queue must be cleared.
#[tokio::test]
async fn a_cancel_that_arrives_before_the_agent_registers_still_clears_the_queue() {
    let ctx = setup().await;
    ctx.conversation_service
        .with_turn_cancelled_hook(Arc::new(QueueClearingCancelHook::new(ctx.queue.clone())));
    ctx.create_conversation("conv_target", "B", "/w/a").await;

    // Claim a turn WITHOUT registering an agent, which is the state the
    // deferred branch exists for.
    let _claim = ctx
        .conversation_service
        .runtime_state()
        .try_claim_turn("conv_target", "turn_active")
        .expect("claim the turn");
    ctx.queue
        .push(aionui_session_message::queue::PendingDelivery {
            to: "conv_target".to_owned(),
            user_id: USER.to_owned(),
            from_conversation_id: "conv_from".to_owned(),
            message: "queued".to_owned(),
            expires_at_ms: 10_000_000,
        })
        .unwrap();

    ctx.conversation_service
        .cancel(USER, "conv_target", "turn_active", &ctx.task_manager)
        .await
        .unwrap();

    assert_eq!(
        ctx.queue.len_for("conv_target"),
        0,
        "a deferred cancel is a real cancel intent"
    );
}

/// Cancelling one conversation must not touch another's backlog.
#[tokio::test]
async fn cancelling_one_target_leaves_another_targets_backlog_alone() {
    let ctx = setup().await;
    ctx.conversation_service
        .with_turn_cancelled_hook(Arc::new(QueueClearingCancelHook::new(ctx.queue.clone())));
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    ctx.create_conversation("conv_c", "C", "/w/a").await;
    let (_claim_b, _agent_b) = ctx.make_busy("conv_b", false);
    let (_claim_c, _agent_c) = ctx.make_busy("conv_c", false);

    ctx.service
        .send(USER, &from.id, &request("conv_b", "hi"))
        .await
        .unwrap();
    ctx.service
        .send(USER, &from.id, &request("conv_c", "hi"))
        .await
        .unwrap();
    assert_eq!(ctx.queue.total_len(), 2);

    let turn_b = ctx.active_turn_id("conv_b").unwrap();
    ctx.conversation_service
        .cancel(USER, "conv_b", &turn_b, &ctx.task_manager)
        .await
        .unwrap();

    assert_eq!(ctx.queue.len_for("conv_b"), 0);
    assert_eq!(ctx.queue.len_for("conv_c"), 1, "an unrelated target must be untouched");
}

/// Without a registered hook the queue is untouched — proves the positive tests
/// above are measuring the hook, not some incidental side effect of `cancel`.
#[tokio::test]
async fn without_the_hook_registered_a_cancel_leaves_the_queue_alone() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    ctx.service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .unwrap();
    let turn_id = ctx.active_turn_id("conv_target").unwrap();

    ctx.conversation_service
        .cancel(USER, "conv_target", &turn_id, &ctx.task_manager)
        .await
        .unwrap();

    assert_eq!(
        ctx.queue.len_for("conv_target"),
        1,
        "clearing must come from the hook, not from cancel itself"
    );
}

// ── Runtime restart is not a stop ────────────────────────────────────
//
// `restart_runtime` cancels the active turn purely so the agent process can be
// killed and rebuilt, and it leaves the conversation IDLE — the state a queued
// delivery has been waiting for. Discarding the backlog there would drop a
// message one tick before it became deliverable, with only a log to show for it.

#[tokio::test]
async fn the_user_cancel_route_reports_a_user_requested_cause() {
    let ctx = setup().await;
    let recorder = Arc::new(RecordingCancelHook::default());
    ctx.conversation_service.with_turn_cancelled_hook(recorder.clone());
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);
    let turn_id = ctx.active_turn_id("conv_target").unwrap();

    ctx.conversation_service
        .cancel(USER, "conv_target", &turn_id, &ctx.task_manager)
        .await
        .unwrap();

    assert_eq!(
        recorder.causes.lock().unwrap().as_slice(),
        [TurnCancelCause::UserRequested]
    );
}

#[tokio::test]
async fn restarting_the_runtime_reports_a_runtime_restart_cause() {
    let ctx = setup().await;
    let recorder = Arc::new(RecordingCancelHook::default());
    ctx.conversation_service.with_turn_cancelled_hook(recorder.clone());
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    // The rebuild that follows the cancel cannot succeed against a stub task
    // manager, and does not need to: the hook fires before it, so the recorded
    // cause is what this test is about.
    let _ = ctx
        .conversation_service
        .restart_runtime(USER, "conv_target", &ctx.task_manager)
        .await;

    assert_eq!(
        recorder.causes.lock().unwrap().as_slice(),
        [TurnCancelCause::RuntimeRestart],
        "a process recycle must be distinguishable from a user stop"
    );
}

#[tokio::test]
async fn restarting_the_runtime_keeps_the_pending_deliveries() {
    let ctx = setup().await;
    ctx.conversation_service
        .with_turn_cancelled_hook(Arc::new(QueueClearingCancelHook::new(ctx.queue.clone())));
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    ctx.service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .unwrap();
    assert_eq!(ctx.queue.len_for("conv_target"), 1);

    let _ = ctx
        .conversation_service
        .restart_runtime(USER, "conv_target", &ctx.task_manager)
        .await;

    assert_eq!(
        ctx.queue.len_for("conv_target"),
        1,
        "a runtime restart must not silently discard work aimed at the conversation"
    );
}
