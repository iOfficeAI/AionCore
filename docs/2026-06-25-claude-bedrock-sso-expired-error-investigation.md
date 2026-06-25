# Claude Bedrock SSO Expired Error Investigation

## TL;DR

Conversation `5555555` did receive an error, but AionUI displayed the wrong level of detail.

The real error was:

```text
Internal error: API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.
```

The displayed error was:

```text
The upstream Agent failed while handling the request
Agent internal error (code -32603)
```

Root cause:

1. Claude Code was using Bedrock/AWS SSO.
2. The AWS SSO token had expired.
3. Claude ACP returned that as JSON-RPC internal error `-32603`.
4. AionCore classified the error as `UNKNOWN_UPSTREAM_ERROR`, `retryable=true`.
5. The useful `aws sso login` instruction stayed only in logs and was not surfaced in the conversation.

Correct future behavior:

1. Detect `Token is expired` / `aws sso login` / `SSO session`.
2. Classify it as `USER_LLM_PROVIDER_AUTH_FAILED`.
3. Mark it `retryable=false`.
4. Show the actionable `aws sso login` instruction to the user.
5. Do not auto-replay the turn.

## Context

This note records a local investigation for the conversation named `5555555`.
The fix is intentionally deferred to a later branch.

Local environment:

- Data directory: `/Users/zhoukai/.aionui-dev`
- Database: `/Users/zhoukai/.aionui-dev/aionui-backend.db`
- Log file: `/Users/zhoukai/Library/Logs/AionUi-Dev/2026-06-25.log`
- Conversation id: `3c2c564b`
- Conversation name: `5555555`
- Agent: Claude Code builtin agent, `agent_metadata.id = 2d23ff1c`
- ACP session id: `fc6de19b-d25f-445e-ba7e-b6cc14b64c3d`

## Timeline

All timestamps are local time from `/Users/zhoukai/Library/Logs/AionUi-Dev/2026-06-25.log`.

```text
16:33:51  Conversation 3c2c564b was created from assistant/agent Claude Code.
16:33:51  First user message "3" was persisted as msg_id=95a40709.
16:36:45  First turn was cancelled by user and completed as finish with no text.
16:43:38  Second user message "你好啊" was persisted as msg_id=0d3bf8ec.
16:43:40  ACP session/prompt started for Claude.
16:46:34  Claude process wrote the real Bedrock/AWS SSO token-expired error to stderr.
16:46:34  AionCore classified the failure as UnknownUpstreamError retryable=true.
16:46:34  AionCore deferred the clean terminal error for possible auto replay.
16:46:34  AionCore started auto replay attempt 2.
16:49:40  A generic tips/error message was persisted to the conversation.
```

## User Visible Symptom

AionUI displayed an error tip in the conversation, but the message was too generic:

```json
{
  "content": "The upstream Agent failed while handling the request",
  "error": {
    "code": "UNKNOWN_UPSTREAM_ERROR",
    "detail": "Agent internal error (code -32603)",
    "feedback_recommended": true,
    "message": "The upstream Agent failed while handling the request",
    "ownership": "unknown_upstream",
    "resolution": {
      "kind": "send_feedback",
      "target": "feedback"
    },
    "retryable": true
  },
  "type": "error"
}
```

The corresponding message row:

```sql
SELECT id, msg_id, type, status, hidden, created_at, substr(content,1,1000)
FROM messages
WHERE conversation_id = '3c2c564b'
ORDER BY created_at ASC;
```

Result:

```text
95a40709|95a40709|text|finish|0|1782376431917|{"content":"3"}
0d3bf8ec|0d3bf8ec|text|finish|0|1782377018422|{"content":"你好啊"}
93c9f568||tips|error|0|1782377380583|{"content":"The upstream Agent failed while handling the request","error":{"code":"UNKNOWN_UPSTREAM_ERROR","detail":"Agent internal error (code -32603)","feedback_recommended":true,"message":"The upstream Agent failed while handling the request","ownership":"unknown_upstream","resolution":{"kind":"send_feedback","target":"feedback"},"retryable":true},"type":"error"}
```

This proves the UI did render the backend error. The failure is not "no error message". The failure is that the backend persisted the wrong public error payload.

## Backend Evidence

The real upstream error is present in the local AionUI log.

Log file:

```text
/Users/zhoukai/Library/Logs/AionUi-Dev/2026-06-25.log
```

Relevant lines:

```text
14203:[2026-06-25 16:46:34.900] [warn]  [aioncore] aionui_ai_agent::capability::cli_process::spawn_sdk: CLI process stderr pid=27935 stderr="message: \"Internal error: API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.\","
14206:[2026-06-25 16:46:34.900] [debug] [aioncore] send_message{conversation_id=3c2c564b msg_id=def4d5d4}: aionui_ai_agent::protocol::acp: [ACP] <- $session/prompt direction="agent_response" method="session/prompt" payload_bytes=228 payload_json=false session_id="none"
14207:[2026-06-25 16:46:34.900] [error] [aioncore] send_message{conversation_id=3c2c564b msg_id=def4d5d4}: aionui_ai_agent::manager::acp::agent: ACP send_message failed error=Internal error: API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile. ({"errorKind":"unknown"}) close_reason_summary=API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile. ({"errorKind":"unknown"})
14208:[2026-06-25 16:46:34.901] [info]  [aioncore] consume_with_send_error{conversation_id=3c2c564b msg_id=def4d5d4 turn_id=turn_25707d8e}: aionui_conversation::stream_relay: StreamRelay received terminal event event_type="Error" elapsed_ms=174570 text_len=0 error_code=Some(UnknownUpstreamError) retryable=Some(true)
14209:[2026-06-25 16:46:34.901] [info]  [aioncore] consume_with_send_error{conversation_id=3c2c564b msg_id=def4d5d4 turn_id=turn_25707d8e}: aionui_conversation::stream_relay: StreamRelay deferred clean terminal error for possible auto replay event_type="Error" elapsed_ms=174570
14210:[2026-06-25 16:46:34.901] [info]  [aioncore] aionui_conversation::turn_recovery_policy: conversation turn recovery decision agent_type=Acp backend="claude" error_code=Some(UnknownUpstreamError) retryable=Some(true) lifecycle=Active already_replayed=false safe_to_auto_replay=true session_recovery_signal=None saw_visible_output=false saw_tool_or_side_effect=false persisted_assistant_output=false decision=AutoReplayOnce { reason: AgentErrorRecovery, safe_to_auto_replay: true, session_recovery_signal: None }
14211:[2026-06-25 16:46:34.901] [info]  [aioncore] aionui_conversation::turn_orchestrator: conversation turn auto replay starting conversation_id=3c2c564b turn_id=turn_25707d8e attempt=1 next_attempt=2 backend="claude" error_code=Some(UnknownUpstreamError) retryable=Some(true) reason=AgentErrorRecovery
```

Agent health state after the failure:

```sql
SELECT id, name, last_check_status, last_check_error_code, last_check_error_message, last_failure_at
FROM agent_metadata
WHERE id = '2d23ff1c';
```

Result:

```text
2d23ff1c|Claude Code|offline|session_send_failed|Agent internal error (code -32603)|1782377380582
```

## Root Cause

The user's local Claude Code uses Bedrock/AWS SSO authentication. The AWS SSO token had expired.

The raw actionable error was:

```text
Internal error: API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.
```

The immediate user action is to run:

```bash
aws sso login
```

with the corresponding AWS profile.

## Why AionUI Showed A Generic Error

The failure path currently treats the ACP `-32603` internal error as an unknown upstream failure.

Observed classification:

- `code = UNKNOWN_UPSTREAM_ERROR`
- `ownership = unknown_upstream`
- `detail = Agent internal error (code -32603)`
- `retryable = true`
- `resolution = send_feedback`

That classification is wrong for this case. The raw error is a provider/authentication problem and is actionable by the user.

The bad classification has two effects:

1. The UI loses the actionable `aws sso login` instruction.
2. The conversation recovery policy sees `retryable=true` and auto-replays once, even though retrying cannot fix an expired SSO token.

This is why the user saw a delayed/generic error instead of an immediate actionable auth error.

## Likely Fix Area

Primary module:

```text
crates/aionui-ai-agent/src/protocol/send_error.rs
```

Relevant behavior to adjust:

- ACP `AgentInternal { code: -32603, message, data }` classification.
- Provider-auth free-text heuristics.
- Retryability for auth failures.

Secondary modules to verify:

```text
crates/aionui-conversation/src/turn_recovery_policy.rs
crates/aionui-conversation/src/stream_relay.rs
crates/aionui-conversation/src/turn_orchestrator.rs
```

The recovery behavior should not auto-replay non-retryable provider auth failures.

## Non-Goals

Do not fix this by changing frontend copy only.

The frontend displayed what the backend persisted:

```text
UNKNOWN_UPSTREAM_ERROR
Agent internal error (code -32603)
```

The fix belongs in backend error classification first. After the backend returns a specific error payload, the frontend may improve presentation, but that is secondary.

Do not fix this by only disabling auto replay globally.

Auto replay is useful for real retryable ACP/session failures. The issue here is incorrect retryability. Expired AWS SSO credentials are not retryable until the user logs in again.

## Expected Classification

For text containing any of the following signatures:

- `Token is expired`
- `aws sso login`
- `SSO session`

Expected stream error:

```text
code = USER_LLM_PROVIDER_AUTH_FAILED
ownership = user_llm_provider
retryable = false
feedback_recommended = false
resolution.kind = check_provider_credentials
```

The user-visible detail should include the actionable instruction where safe:

```text
Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.
```

## Expected User Experience

When Claude Code Bedrock/AWS SSO is expired, the conversation should show an error close to:

```text
Claude Code could not authenticate with the model provider.

Token is expired. To refresh this SSO session run:
aws sso login
```

The exact copy can differ, but it must preserve the action the user can take.

The error should not suggest feedback as the primary action. It should point the user to provider credentials / local login.

## Suggested Regression Tests

Add a failing test before implementation in:

```text
crates/aionui-ai-agent/src/protocol/send_error.rs
```

Suggested case:

```rust
#[test]
fn classifies_bedrock_sso_token_expired_as_provider_auth_failure() {
    assert_acp_classification(
        AcpError::AgentInternal {
            message: "Internal error: API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.".into(),
            code: -32603,
            data: Some(json!({ "errorKind": "unknown" })),
        },
        AgentErrorCode::UserLlmProviderAuthFailed,
        AgentErrorOwnership::UserLlmProvider,
        AgentErrorResolutionKind::CheckProviderCredentials,
    );
}
```

Also assert:

```rust
assert_eq!(err.stream_error().retryable, Some(false));
assert_eq!(err.stream_error().feedback_recommended, Some(false));
```

If this path currently sanitizes away the original message, decide explicitly whether the user-visible `detail` may include this text. In this specific case the text is not a secret and is directly actionable.

## Acceptance Criteria

Future branch should be considered fixed only if all of these are true:

1. `Token is expired ... aws sso login ...` no longer maps to `UNKNOWN_UPSTREAM_ERROR`.
2. The stream error code is `USER_LLM_PROVIDER_AUTH_FAILED`.
3. `retryable` is `false`.
4. `feedback_recommended` is `false`.
5. The user-visible detail includes the useful `aws sso login` instruction, unless a deliberate security review decides otherwise.
6. The turn recovery policy does not auto-replay this error.
7. Existing unknown upstream errors that do not match provider/auth signatures remain classified as `UNKNOWN_UPSTREAM_ERROR`.
8. Existing provider auth tests for API key errors still pass.

Suggested verification commands:

```bash
cargo test -p aionui-ai-agent send_error
cargo test -p aionui-conversation turn_recovery
```

Run broader affected-crate tests if the implementation touches conversation recovery:

```bash
cargo test -p aionui-ai-agent -p aionui-conversation
```

## Notes

This should be fixed in a separate branch, not in the current migration/assistant-agent-id unification branch.
