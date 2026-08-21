//! End-to-end: A delivers → B processes → B replies via `reply_to` → A opens a
//! turn. Plus the structural guarantees that must hold no matter what the new
//! routes do.

mod common;

use std::sync::Arc;

use aionui_api_types::{SendMessageRequest, SessionDeliveryStatus, SessionSendMessageRequest};
use aionui_conversation::ConversationError;
use aionui_session_message::QueueClearingCancelHook;
use common::{USER, setup};

fn request(to: &str, message: &str) -> SessionSendMessageRequest {
    SessionSendMessageRequest {
        to: to.to_owned(),
        message: message.to_owned(),
    }
}

/// Pull `reply_to` back out of the recipient block, exactly as the receiving
/// agent has to.
fn extract_reply_to(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("reply_to: "))
        .and_then(|rest| rest.split('\t').next())
        .map(str::to_owned)
}

#[tokio::test]
async fn a_delivers_b_replies_and_a_opens_a_turn() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;

    ctx.service
        .send(USER, &a.id, &request(&b.id, "接口定完了吗？"))
        .await
        .unwrap();

    let received = ctx.last_user_message_content(&b.id).await;
    let reply_to = extract_reply_to(&received).expect("the block must carry reply_to");
    assert_eq!(reply_to, a.id, "reply_to must address the sender");
    assert!(received.trim_end().ends_with("接口定完了吗？"), "{received}");

    // B's agent replies with the same CLI surface.
    ctx.service
        .send(USER, &b.id, &request(&reply_to, "定完了。"))
        .await
        .unwrap();

    let back = ctx.last_user_message_content(&a.id).await;
    assert!(back.contains(&format!("from: B\t{}", b.id)), "{back}");
    assert!(back.contains(&format!("reply_to: {}", b.id)), "{back}");
    assert!(back.trim_end().ends_with("定完了。"), "{back}");
}

#[tokio::test]
async fn a_busy_non_midturn_target_is_queued_and_delivered_on_the_first_tick_after_it_frees_up() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    // A backend that does NOT support mid-turn delivery, so busy must mean
    // Busy → queue.
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    let (claim, _agent) = ctx.make_busy(&b.id, false);

    let response = ctx.service.send(USER, &a.id, &request(&b.id, "hi")).await.unwrap();
    assert_eq!(response.status, SessionDeliveryStatus::Queued);

    ctx.drainer().tick_once().await;
    assert_eq!(ctx.queue.len_for(&b.id), 1, "still busy → still queued");
    assert_eq!(ctx.user_message_count(&b.id).await, 0);

    // The turn ends: dropping the claim frees the conversation.
    drop(claim);
    ctx.drainer().tick_once().await;
    assert_eq!(
        ctx.queue.len_for(&b.id),
        0,
        "delivered on the first tick after it freed up"
    );
    assert_eq!(ctx.user_message_count(&b.id).await, 1);
    let content = ctx.last_user_message_content(&b.id).await;
    assert!(content.starts_with("[[AION_SESSION_MESSAGE]]"), "{content}");
}

#[tokio::test]
async fn three_queued_messages_reach_the_target_one_turn_each_in_order() {
    // spec §6.4: per-message, FIFO, one turn each — never merged.
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    let (claim, _agent) = ctx.make_busy(&b.id, false);

    for index in 1..=3 {
        ctx.service
            .send(USER, &a.id, &request(&b.id, &format!("msg-{index}")))
            .await
            .unwrap();
    }
    assert_eq!(ctx.queue.len_for(&b.id), 3);

    drop(claim);
    // One per tick is the load-bearing claim: three queued messages must NOT
    // collapse into a single turn.
    ctx.drainer().tick_once().await;
    assert_eq!(ctx.user_message_count(&b.id).await, 1, "one per tick, not merged");
    let after_first = ctx.all_user_message_contents(&b.id).await;
    assert!(
        after_first[0].contains("msg-1"),
        "FIFO: the first queued message goes first, got {after_first:?}"
    );

    ctx.drainer().tick_once().await;
    assert_eq!(ctx.user_message_count(&b.id).await, 2, "still one per tick");
    ctx.drainer().tick_once().await;

    // Assert on the SET of bodies rather than "the last row": with three rows
    // written inside the same millisecond, which one sorts last is a DB
    // tie-break detail, not behaviour this test is about.
    let delivered = ctx.all_user_message_contents(&b.id).await;
    assert_eq!(delivered.len(), 3);
    for index in 1..=3 {
        let needle = format!("msg-{index}");
        assert!(
            delivered.iter().any(|body| body.contains(&needle)),
            "{needle} was never delivered: {delivered:?}"
        );
    }
    // And each arrived as its own message, never merged into one body.
    for body in &delivered {
        let count = (1..=3).filter(|i| body.contains(&format!("msg-{i}"))).count();
        assert_eq!(count, 1, "a delivered message must carry exactly one body: {body}");
    }
}

#[tokio::test]
async fn a_midturn_capable_target_takes_the_message_into_its_running_turn() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    let (_claim, agent) = ctx.make_busy(&b.id, true);
    let turn_before = ctx.active_turn_id(&b.id).expect("B is mid-turn");

    let response = ctx.service.send(USER, &a.id, &request(&b.id, "hi")).await.unwrap();

    assert_eq!(response.status, SessionDeliveryStatus::Delivered);
    assert!(ctx.queue.is_empty(), "mid-turn delivery must not queue");
    // `status: delivered` alone does NOT prove the message merged into the
    // running turn — an ordinary new-turn send reports `delivered` too. The
    // active turn id must be UNCHANGED.
    assert_eq!(
        ctx.active_turn_id(&b.id).as_deref(),
        Some(turn_before.as_str()),
        "the message must ride the SAME turn, not open a new one"
    );
    assert_eq!(agent.delivered_midturn.lock().unwrap().len(), 1);
}

/// The branch mid-turn interjection added that is easy to miss: `send_message`
/// REFUSES mid-turn delivery while a permission/question card is pending (the
/// card is the required answer channel), falls through to the claim, and yields
/// Busy. So a mid-turn-capable target can queue too — the queue is not only for
/// ACP-like backends. Without this test the queue path looks unreachable for
/// claude/codex and someone will "simplify" it away.
#[tokio::test]
async fn a_midturn_capable_target_blocked_on_a_confirmation_card_is_queued_then_drains() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    let (claim, agent) = ctx.make_busy_awaiting_confirmation(&b.id);

    let response = ctx.service.send(USER, &a.id, &request(&b.id, "hi")).await.unwrap();
    assert_eq!(response.status, SessionDeliveryStatus::Queued);
    assert_eq!(ctx.queue.len_for(&b.id), 1);
    assert!(
        agent.delivered_midturn.lock().unwrap().is_empty(),
        "mid-turn must be refused while a card is pending"
    );

    // The card is answered and the turn ends → the next tick delivers.
    agent.resolve_confirmations();
    drop(claim);
    ctx.drainer().tick_once().await;
    assert_eq!(ctx.queue.len_for(&b.id), 0);
    assert_eq!(ctx.user_message_count(&b.id).await, 1);
}

#[tokio::test]
async fn the_send_message_team_guard_cannot_be_bypassed_by_calling_the_layer_below() {
    // spec §11: prove the structural guarantee, not just the route's preamble.
    let ctx = setup().await;
    let team_conv = ctx.create_team_conversation("conv_team", "team_1").await;

    let error = ctx
        .conversation_service
        .send_message(
            USER,
            &team_conv.id,
            SendMessageRequest {
                content: "straight at the bottom layer".to_owned(),
                files: Vec::new(),
                sessions: Vec::new(),
                inject_skills: Vec::new(),
                hidden: false,
            },
            &ctx.task_manager,
        )
        .await
        .expect_err("send_message itself must refuse a team conversation");

    assert!(matches!(error, ConversationError::Forbidden { .. }), "{error:?}");
}

#[tokio::test]
async fn the_rate_limited_event_carries_user_id_and_no_message_body() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    ctx.broadcaster.take_events();

    let secret = "SECRET-BODY-DO-NOT-BROADCAST";
    for _ in 0..=aionui_session_message::rate_limit::PAIR_LIMIT {
        let _ = ctx.service.send(USER, &a.id, &request(&b.id, secret)).await;
    }

    let event = ctx
        .broadcaster
        .find("sessionMessage.rateLimited")
        .expect("the gate must broadcast");
    assert_eq!(event.data["user_id"], serde_json::json!(USER));
    assert_eq!(event.data["from_conversation_id"], serde_json::json!(a.id));
    assert_eq!(event.data["to_conversation_id"], serde_json::json!(b.id));
    assert!(event.data["window_count"].is_number());
    let rendered = event.data.to_string();
    assert!(
        !rendered.contains(secret),
        "the event must never carry the message body: {rendered}"
    );
}

/// A message that sits in the queue past its TTL is dropped, never delivered.
#[tokio::test]
async fn a_queued_message_that_expires_is_dropped_and_never_delivered() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    let (claim, _agent) = ctx.make_busy(&b.id, false);

    ctx.service
        .send(USER, &a.id, &request(&b.id, "too late"))
        .await
        .unwrap();
    assert_eq!(ctx.queue.len_for(&b.id), 1);

    // Past the 10-minute TTL, driven by the injected clock rather than a sleep.
    ctx.clock.advance(aionui_session_message::queue::TTL_MS + 1);
    drop(claim);
    ctx.drainer().tick_once().await;

    assert_eq!(ctx.queue.len_for(&b.id), 0, "expired items must be dropped");
    assert_eq!(
        ctx.user_message_count(&b.id).await,
        0,
        "an expired message must never be delivered"
    );
}

/// The full stop loop: queue a delivery, cancel the target's turn, and confirm
/// the drainer cannot resurrect it. This is the acceptance point spec §6.9 calls
/// the whole reason the hook exists.
#[tokio::test]
async fn cancelling_the_target_stops_the_loop_for_good() {
    let ctx = setup().await;
    ctx.conversation_service
        .with_turn_cancelled_hook(Arc::new(QueueClearingCancelHook::new(ctx.queue.clone())));
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    let b = ctx.create_conversation("conv_b", "B", "/w/shared").await;
    let (_claim, _agent) = ctx.make_busy(&b.id, false);

    ctx.service.send(USER, &a.id, &request(&b.id, "hi")).await.unwrap();
    let turn_id = ctx.active_turn_id(&b.id).unwrap();

    ctx.conversation_service
        .cancel(USER, &b.id, &turn_id, &ctx.task_manager)
        .await
        .unwrap();

    assert_eq!(ctx.queue.len_for(&b.id), 0);
    // Several ticks, to rule out "it comes back a second later".
    for _ in 0..3 {
        ctx.drainer().tick_once().await;
    }
    assert_eq!(
        ctx.user_message_count(&b.id).await,
        0,
        "stop must hold across subsequent ticks"
    );
}

/// Cross-user isolation at the service layer: A cannot reach a conversation
/// owned by someone else, and the refusal does not confirm the id exists.
#[tokio::test]
async fn a_delivery_aimed_at_another_users_conversation_is_refused_as_not_found() {
    let ctx = setup().await;
    let a = ctx.create_conversation("conv_a", "A", "/w/shared").await;
    ctx.create_conversation_for(common::OTHER_USER, "conv_theirs", "theirs", "/w/other")
        .await;

    let error = ctx
        .service
        .send(USER, &a.id, &request("conv_theirs", "hi"))
        .await
        .expect_err("cross-user delivery must be refused");

    assert_eq!(error.http_status(), 404, "403 would confirm the id exists");
    assert_eq!(
        ctx.user_message_count_for(common::OTHER_USER, "conv_theirs").await,
        0,
        "nothing may be persisted into the other user's conversation"
    );
}
