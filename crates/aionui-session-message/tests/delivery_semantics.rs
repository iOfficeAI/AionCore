//! Validation order, bad paths, and the happy path of cross-session delivery.
//!
//! The validation order is a wire contract: when several conditions hold at
//! once, which code comes back must be deterministic (spec §6.3), or every
//! bad-path assertion in this file becomes order-dependent.

mod common;

use aionui_api_types::{SessionDeliveryStatus, SessionSendMessageRequest, SessionToolErrorCode};
use common::{OTHER_USER, USER, setup};

fn request(to: &str, message: &str) -> SessionSendMessageRequest {
    SessionSendMessageRequest {
        to: to.to_owned(),
        message: message.to_owned(),
    }
}

// ── Validation order (spec §6.3) ────────────────────────────────────

#[tokio::test]
async fn all_four_conditions_at_once_returns_feature_disabled() {
    // The single most important ordering assertion: it pins the whole chain.
    let ctx = setup().await;
    ctx.disable_feature(USER).await;
    let from = ctx.create_team_conversation("conv_from", "team_1").await;
    ctx.exhaust_rate_gate(&from.id, "conv_missing");

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_missing", "hi"))
        .await
        .expect_err("must fail");

    assert_eq!(error.code(), SessionToolErrorCode::FeatureDisabled, "{error}");
}

#[tokio::test]
async fn a_team_sender_is_rejected_before_the_schema_is_checked() {
    let ctx = setup().await;
    let from = ctx.create_team_conversation("conv_from", "team_1").await;

    // `to: "*"` would be schema_validation_failed if the order were wrong.
    let error = ctx
        .service
        .send(USER, &from.id, &request("*", "hi"))
        .await
        .expect_err("must fail");

    assert_eq!(error.code(), SessionToolErrorCode::SenderIsTeam, "{error}");
}

#[tokio::test]
async fn rate_limit_is_checked_before_the_target_lookup() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.exhaust_rate_gate(&from.id, "conv_missing");

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_missing", "hi"))
        .await
        .expect_err("must fail");

    // target_not_found would mean we paid a DB read during a spin storm.
    assert_eq!(error.code(), SessionToolErrorCode::RateLimited, "{error}");
}

#[tokio::test]
async fn the_schema_check_runs_before_the_rate_gate() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.exhaust_rate_gate(&from.id, "*");

    let error = ctx
        .service
        .send(USER, &from.id, &request("*", "hi"))
        .await
        .expect_err("must fail");

    assert_eq!(error.code(), SessionToolErrorCode::SchemaValidationFailed, "{error}");
}

// ── Bad paths (spec §11) ────────────────────────────────────────────

#[tokio::test]
async fn a_name_instead_of_an_id_is_target_not_found() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "重构-鉴权模块", "/w/a").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("重构-鉴权模块", "hi"))
        .await
        .expect_err("names are not addresses");

    assert_eq!(error.code(), SessionToolErrorCode::TargetNotFound, "{error}");
}

#[tokio::test]
async fn a_star_target_is_schema_validation_failed_not_a_broadcast() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("*", "hi"))
        .await
        .expect_err("no broadcast in v1");

    assert_eq!(error.code(), SessionToolErrorCode::SchemaValidationFailed, "{error}");
}

#[tokio::test]
async fn an_empty_message_is_rejected() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "   "))
        .await
        .expect_err("an empty body must not open a turn");

    assert_eq!(error.code(), SessionToolErrorCode::SchemaValidationFailed, "{error}");
}

#[tokio::test]
async fn targeting_yourself_is_rejected() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request(&from.id, "hi"))
        .await
        .expect_err("self-delivery is rejected");

    assert_eq!(error.code(), SessionToolErrorCode::TargetIsSelf, "{error}");
}

#[tokio::test]
async fn another_users_conversation_id_yields_not_found_not_forbidden() {
    // Locks spec §9.1: refuse without leaking that the id exists.
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation_for(OTHER_USER, "conv_theirs", "theirs", "/w/b")
        .await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_theirs", "hi"))
        .await
        .expect_err("cross-user delivery is refused");

    assert_eq!(error.code(), SessionToolErrorCode::TargetNotFound, "{error}");
    assert_eq!(error.http_status(), 404, "403 would confirm the id exists");
}

#[tokio::test]
async fn a_target_deleted_after_the_send_boundary_still_fails_at_cli_time() {
    // The time-gap scenario spec §6.3 insists on: the `@@` reference passed
    // validation minutes ago; the CLI call must re-check.
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    ctx.delete_conversation("conv_target").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect_err("must re-validate at CLI time");

    assert_eq!(error.code(), SessionToolErrorCode::TargetNotFound, "{error}");
}

#[tokio::test]
async fn a_target_that_joined_a_team_after_the_send_boundary_is_rejected() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    ctx.mark_conversation_team("conv_target", "team_1").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect_err("must re-validate at CLI time");

    assert_eq!(error.code(), SessionToolErrorCode::TargetIsTeam, "{error}");
}

#[tokio::test]
async fn delivery_is_refused_while_the_master_switch_is_off() {
    let ctx = setup().await;
    ctx.disable_feature(USER).await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect_err("the panic button must hold");

    assert_eq!(error.code(), SessionToolErrorCode::FeatureDisabled, "{error}");
    assert_eq!(error.http_status(), 403);
}

// ── Happy path ──────────────────────────────────────────────────────

#[tokio::test]
async fn an_idle_target_is_delivered_and_actually_starts_a_turn() {
    // Asserting HTTP success alone would not prove a turn started.
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "接口定完了吗？"))
        .await
        .expect("delivery succeeds");

    assert_eq!(response.status, SessionDeliveryStatus::Delivered);
    let content = ctx.last_user_message_content("conv_target").await;
    assert!(content.starts_with("[[AION_SESSION_MESSAGE]]"), "{content}");
    assert!(content.contains(&format!("reply_to: {}", from.id)), "{content}");
    assert!(content.contains("workspace: same"), "{content}");
    assert!(content.trim_end().ends_with("接口定完了吗？"), "{content}");
}

#[tokio::test]
async fn a_cross_workspace_delivery_states_the_absolute_path_and_the_constraint() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/b").await;

    ctx.service
        .send(USER, &from.id, &request("conv_target", "看下 src/auth.rs"))
        .await
        .unwrap();

    let content = ctx.last_user_message_content("conv_target").await;
    assert!(
        content.contains("workspace: /w/a（与你不同，勿用相对路径，勿假设可读）"),
        "{content}"
    );
}

#[tokio::test]
async fn the_recipient_block_names_the_sender_from_the_row_not_the_caller() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "重构-鉴权模块", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;

    ctx.service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .unwrap();

    let content = ctx.last_user_message_content("conv_target").await;
    assert!(content.contains("from: 重构-鉴权模块\tconv_from"), "{content}");
}

// ── Queueing ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_busy_target_without_midturn_support_is_queued_not_delivered() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect("busy must not be an error");

    assert_eq!(response.status, SessionDeliveryStatus::Queued);
    assert_eq!(ctx.queue.len_for("conv_target"), 1);
    assert_eq!(
        ctx.user_message_count("conv_target").await,
        0,
        "a queued message must not be persisted yet"
    );
}

#[tokio::test]
async fn a_busy_midturn_capable_target_takes_the_message_into_its_running_turn() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, agent) = ctx.make_busy("conv_target", true);
    let turn_before = ctx.active_turn_id("conv_target").expect("target is mid-turn");

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect("mid-turn delivery succeeds");

    assert_eq!(response.status, SessionDeliveryStatus::Delivered);
    assert!(ctx.queue.is_empty(), "mid-turn delivery must not queue");
    // `status: delivered` alone does NOT prove the message merged into the
    // running turn — an ordinary new-turn send reports `delivered` too. The
    // active turn id must be UNCHANGED.
    assert_eq!(
        ctx.active_turn_id("conv_target").as_deref(),
        Some(turn_before.as_str()),
        "the message must ride the SAME turn, not open a new one"
    );
    let delivered = agent.delivered_midturn.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1, "delivered through the mid-turn path exactly once");
    assert!(
        delivered[0].content.starts_with("[[AION_SESSION_MESSAGE]]"),
        "{}",
        delivered[0].content
    );
}

/// The branch mid-turn interjection added that is easy to miss: `send_message`
/// REFUSES mid-turn delivery while a permission/question card is pending (the
/// card is the required answer channel), falls through to the claim, and yields
/// Busy. So a midturn-capable target can queue too — the queue is NOT only for
/// ACP-like backends. Without this test the queue path looks unreachable for
/// claude/codex and someone will "simplify" it away.
#[tokio::test]
async fn a_midturn_capable_target_blocked_on_a_confirmation_card_is_queued_not_delivered() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, agent) = ctx.make_busy_awaiting_confirmation("conv_target");

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect("a pending card must queue, not error");

    assert_eq!(response.status, SessionDeliveryStatus::Queued);
    assert_eq!(ctx.queue.len_for("conv_target"), 1);
    assert!(
        agent.delivered_midturn.lock().unwrap().is_empty(),
        "mid-turn must be refused while a card is pending"
    );
}

// ── Rate limiting ───────────────────────────────────────────────────

#[tokio::test]
async fn the_pair_gate_trips_and_broadcasts_an_event_without_the_message_body() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    ctx.broadcaster.take_events();

    let secret = "SECRET-BODY-DO-NOT-BROADCAST";
    let mut last = None;
    for _ in 0..=aionui_session_message::rate_limit::PAIR_LIMIT {
        last = Some(ctx.service.send(USER, &from.id, &request("conv_target", secret)).await);
    }
    let error = last.unwrap().expect_err("the last one must trip the gate");
    assert_eq!(error.code(), SessionToolErrorCode::RateLimited, "{error}");

    let event = ctx
        .broadcaster
        .find("sessionMessage.rateLimited")
        .expect("the gate must broadcast");
    assert_eq!(event.data["user_id"], serde_json::json!(USER));
    assert_eq!(event.data["from_conversation_id"], serde_json::json!("conv_from"));
    assert_eq!(event.data["to_conversation_id"], serde_json::json!("conv_target"));
    assert_eq!(event.data["to_name"], serde_json::json!("B"));
    assert_eq!(event.data["gate"], serde_json::json!("pair"));
    assert!(event.data["window_count"].is_number());
    let rendered = event.data.to_string();
    assert!(
        !rendered.contains(secret),
        "the event must never carry the message body: {rendered}"
    );
}

#[tokio::test]
async fn tripping_one_pair_still_allows_a_different_target() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    ctx.create_conversation("conv_c", "C", "/w/a").await;

    for _ in 0..=aionui_session_message::rate_limit::PAIR_LIMIT {
        let _ = ctx.service.send(USER, &from.id, &request("conv_b", "hi")).await;
    }

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_c", "hi"))
        .await
        .expect("a different target must still be reachable");
    assert_eq!(response.status, SessionDeliveryStatus::Delivered);
}

/// The OUTBOUND gate, distinct from the pair gate above: it bounds one sender's
/// total fan-out, so it has to trip even when no single pair is over its own
/// limit. Asserted through the service rather than only over the limiter, so the
/// error code and status the CLI sees are pinned too.
#[tokio::test]
async fn the_outbound_gate_trips_across_many_different_targets() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;

    let pair_limit = aionui_session_message::rate_limit::PAIR_LIMIT;
    let outbound_limit = aionui_session_message::rate_limit::OUTBOUND_LIMIT;
    // Spread over enough targets that the per-pair budget is never the binding
    // constraint.
    let targets: Vec<String> = (0..outbound_limit + 1).map(|index| format!("conv_t{index}")).collect();
    for id in &targets {
        ctx.create_conversation(id, "T", "/w/a").await;
    }

    let mut last = None;
    for id in &targets {
        last = Some(ctx.service.send(USER, &from.id, &request(id, "hi")).await);
    }

    let error = last
        .unwrap()
        .expect_err("the send past the outbound budget must be refused");
    assert_eq!(error.code(), SessionToolErrorCode::RateLimited, "{error}");
    assert_eq!(error.http_status(), 429, "{error}");
    // Proves the OUTBOUND gate tripped, not the pair gate: every target here got
    // exactly one message, far below the pair limit.
    assert!(
        outbound_limit > pair_limit,
        "this test only isolates the outbound gate while it is the looser of the two"
    );

    let event = ctx
        .broadcaster
        .find("sessionMessage.rateLimited")
        .expect("the gate must broadcast");
    assert_eq!(event.data["gate"], serde_json::json!("outbound"));
    assert_eq!(event.data["user_id"], serde_json::json!(USER));
}

// ── Queue capacity ──────────────────────────────────────────────────

/// A single sender can never see `queue_full`, and that is worth pinning:
/// `PER_TARGET_LIMIT` is 20 while the pair gate allows 10 per (from, to) window,
/// so the eleventh send to the same target comes back `rate_limited` with the
/// backlog only half full. Anyone reading the error table would expect
/// `queue_full` here.
#[tokio::test]
async fn one_sender_hits_the_pair_gate_long_before_the_backlog_fills() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    let pair_limit = aionui_session_message::rate_limit::PAIR_LIMIT as usize;
    for index in 0..pair_limit {
        let response = ctx
            .service
            .send(USER, &from.id, &request("conv_target", "hi"))
            .await
            .unwrap_or_else(|error| panic!("send {index} should queue, got {error}"));
        assert_eq!(response.status, SessionDeliveryStatus::Queued);
    }

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "hi"))
        .await
        .expect_err("the pair gate must refuse the next one");
    assert_eq!(error.code(), SessionToolErrorCode::RateLimited, "{error}");
    assert!(
        pair_limit < aionui_session_message::queue::PER_TARGET_LIMIT,
        "if the pair budget ever reaches the queue cap this test stops isolating the gates"
    );
    assert_eq!(ctx.queue.len_for("conv_target"), pair_limit);
}

/// `queue_full` was only ever asserted over the queue in isolation. This pins
/// what a CALLER sees — the code and status the CLI contract promises — and it
/// takes THREE senders to get there: the per-target cap is 20 while each sender
/// is capped at 10 per pair, so filling the backlog needs two senders and a
/// third to be refused.
#[tokio::test]
async fn a_full_per_target_backlog_is_refused_as_queue_full() {
    let ctx = setup().await;
    let pair_limit = aionui_session_message::rate_limit::PAIR_LIMIT as usize;
    let per_target = aionui_session_message::queue::PER_TARGET_LIMIT;
    let senders_needed = per_target.div_ceil(pair_limit);

    let mut senders = Vec::new();
    for index in 0..=senders_needed {
        senders.push(ctx.create_conversation(&format!("conv_from{index}"), "A", "/w/a").await);
    }
    ctx.create_conversation("conv_target", "B", "/w/a").await;
    // Busy without mid-turn support → every send queues instead of delivering.
    let (_claim, _agent) = ctx.make_busy("conv_target", false);

    let mut queued = 0;
    for sender in &senders[..senders_needed] {
        for _ in 0..pair_limit {
            if queued == per_target {
                break;
            }
            let response = ctx
                .service
                .send(USER, &sender.id, &request("conv_target", "hi"))
                .await
                .expect("a send within both budgets must queue");
            assert_eq!(response.status, SessionDeliveryStatus::Queued);
            queued += 1;
        }
    }
    assert_eq!(ctx.queue.len_for("conv_target"), per_target, "the backlog must be full");

    // A fresh sender: its own pair and outbound windows are empty, so the only
    // thing left to refuse it is the backlog cap.
    let error = ctx
        .service
        .send(USER, &senders[senders_needed].id, &request("conv_target", "hi"))
        .await
        .expect_err("the send past the per-target cap must be refused");
    assert_eq!(error.code(), SessionToolErrorCode::QueueFull, "{error}");
    assert_eq!(error.http_status(), 409, "{error}");
    assert_eq!(
        ctx.queue.len_for("conv_target"),
        per_target,
        "a refused send must not grow the backlog"
    );
}

// ── guard_list_access ───────────────────────────────────────────────

#[tokio::test]
async fn listing_is_refused_when_the_feature_is_off() {
    let ctx = setup().await;
    ctx.disable_feature(USER).await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;

    let error = ctx
        .service
        .guard_list_access(USER, &from.id)
        .await
        .expect_err("handing the agent a list it cannot act on wastes a round trip");
    assert_eq!(error.code(), SessionToolErrorCode::FeatureDisabled, "{error}");
}

#[tokio::test]
async fn listing_is_refused_for_a_team_caller() {
    let ctx = setup().await;
    let from = ctx.create_team_conversation("conv_from", "team_1").await;

    let error = ctx
        .service
        .guard_list_access(USER, &from.id)
        .await
        .expect_err("team callers must not use this surface at all");
    assert_eq!(error.code(), SessionToolErrorCode::SenderIsTeam, "{error}");
}

#[tokio::test]
async fn listing_is_allowed_for_an_ordinary_caller_with_the_feature_on() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    assert!(ctx.service.guard_list_access(USER, &from.id).await.is_ok());
}

/// A restart window must not eat queued work.
///
/// Regression from a live run: the user restarted a conversation's runtime while
/// two cross-session messages were queued for it. The cancel hook correctly KEPT
/// them (`TurnCancelCause::RuntimeRestart`), and 300ms later the drainer dropped
/// both with "hard delivery error, reason=Conversation runtime is restarting" —
/// silent message loss, and it made the hook's cause distinction pointless.
///
/// Goes through the REAL `send_message` rather than a fake sink, because the
/// defect lived in how its error was classified.
#[tokio::test]
async fn a_restarting_target_keeps_its_queue_and_is_drained_once_the_restart_finishes() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "A", "/w/a").await;
    ctx.create_conversation("conv_target", "B", "/w/a").await;

    // Queue one the ordinary way: target busy on a backend that cannot take a
    // mid-turn message.
    let (claim, _agent) = ctx.make_busy("conv_target", false);
    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "survive the restart"))
        .await
        .expect("busy must not be an error");
    assert_eq!(response.status, SessionDeliveryStatus::Queued);

    // The turn ends (as a restart cancels it) and the runtime enters its restart
    // window — the state the drainer used to treat as fatal.
    drop(claim);
    ctx.conversation_service
        .runtime_state()
        .begin_restart("conv_target")
        .expect("no other restart is in flight");

    let drainer = ctx.drainer();
    drainer.tick_once().await;
    assert_eq!(
        ctx.queue.len_for("conv_target"),
        1,
        "a restart is a ~1s window, not a rejection: the message must stay queued"
    );
    assert_eq!(
        ctx.user_message_count("conv_target").await,
        0,
        "nothing is delivered while the runtime is restarting"
    );

    // Restart done: the conversation is now idle, which is exactly what the
    // queued item was waiting for.
    ctx.conversation_service.runtime_state().clear_restarting("conv_target");
    drainer.tick_once().await;

    assert!(ctx.queue.is_empty(), "the item must leave the queue once delivered");
    assert_eq!(ctx.user_message_count("conv_target").await, 1);
    let content = ctx.last_user_message_content("conv_target").await;
    assert!(content.contains("survive the restart"), "{content}");
}
