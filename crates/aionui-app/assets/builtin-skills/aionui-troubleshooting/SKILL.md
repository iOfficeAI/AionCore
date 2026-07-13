---
name: aionui-troubleshooting
description: >-
  Diagnose a running AionUi installation: inspect stuck or errored conversations, read provider health, scheduled task state, MCP server health, team member state, backend health, and aioncore logs. Use when the user reports AionUi is misbehaving, a conversation is stuck, an LLM/provider call is failing, a scheduled task did not run, an MCP server has no tools, a team member is hung, or they ask to troubleshoot AionUi.
---

# AionUi Troubleshooting

Use the bundled `aioncore diagnose` CLI for read-only troubleshooting. It uses
the runtime context injected into the current agent conversation, so do not
discover ports or call backend endpoints by hand.

Examples in this skill use English. When reporting findings or writing user
content, use the user's language.

## Rules

1. Use only `"$AIONUI_HELPER_BIN" diagnose ...`.
2. Start with `diagnose overview` for broad "what is wrong" requests.
3. Use named diagnose commands first. Use `diagnose http get` only when no named
   command covers the diagnostic need.
4. Treat every command as read-only. To change AionUi configuration, use the
   separate `aionui-config` skill.
5. Never print raw provider, MCP header, token, password, or secret values. The
   CLI redacts known secret fields by default, but summarize sensitive findings
   carefully.
6. A single `running` snapshot does not prove a hang. Re-check the same
   conversation and compare runtime fields, message progress, and logs.

## Capability Discovery

When unsure which command or stdin fields are available, ask the CLI:

```bash
"$AIONUI_HELPER_BIN" diagnose capabilities
```

The output is the agent-readable contract: domains, command names, stdin JSON
fields, selector behavior, redacted fields, and the controlled HTTP escape
hatch.

## Runtime Context

The main stable selector is the current conversation:

```json
{
  "conversation_id": "current"
}
```

The CLI resolves `"current"` from `AIONUI_CONVERSATION_ID`. Commands that read
the backend also use `AIONUI_BASE_URL` and `AIONUI_USER_ID` from the same
runtime context.

## Start Wide

For a vague "AionUi is broken" report, run:

```bash
"$AIONUI_HELPER_BIN" diagnose overview
```

Use the overview to decide where to drill in:

- `providers.unhealthy` means inspect provider health.
- `mcp.enabled_but_no_tools` means inspect MCP startup/tool registration.
- `cron.failing` means inspect scheduled task state.
- `running_conversations` means inspect the conversation runtime repeatedly.

## Commands By Symptom

### Conversation Stuck Or Errored

Inspect the current conversation:

```bash
"$AIONUI_HELPER_BIN" diagnose conversations get <<'JSON'
{
  "conversation_id": "current"
}
JSON
```

Inspect a known conversation:

```bash
"$AIONUI_HELPER_BIN" diagnose conversations get <<'JSON'
{
  "conversation_id": "conv_123"
}
JSON
```

Read recent messages:

```bash
"$AIONUI_HELPER_BIN" diagnose conversations messages <<'JSON'
{
  "conversation_id": "current",
  "limit": 30,
  "errors_only": true
}
JSON
```

Interpretation:

- `state=running` with `is_processing=true` is only a suspected hang after
  repeated checks show no `turn_id`, runtime, or message progress.
- `state=waiting_confirmation` or `pending_confirmations > 0` means the turn is
  waiting for user approval, not hung.

### Provider Or Model Failure

```bash
"$AIONUI_HELPER_BIN" diagnose providers summary
```

Look for non-`healthy` model health, stale `last_check`, high latency, or an
error string. The summary is redacted; do not ask for raw provider JSON unless
the named command is insufficient.

### Scheduled Task Did Not Run

```bash
"$AIONUI_HELPER_BIN" diagnose cron summary
```

Check `enabled`, `last_status`, `last_error`, `next_run_at`, `last_run_at`,
`run_count`, and `retry_count` when present.

### MCP Server Has No Tools

```bash
"$AIONUI_HELPER_BIN" diagnose mcp summary
```

An enabled server with `tool_count=0` usually means startup failed, the command
is unavailable, credentials are invalid, or the server crashed before tool
registration.

### Team Member Hung

```bash
"$AIONUI_HELPER_BIN" diagnose teams summary
```

Find the member's `conversation_id`, then drill into that conversation:

```bash
"$AIONUI_HELPER_BIN" diagnose conversations get <<'JSON'
{
  "conversation_id": "member_conv_123"
}
JSON
```

### Backend Health

```bash
"$AIONUI_HELPER_BIN" diagnose health
```

Use this to confirm the backend is reachable and read the core version/build
metadata.

### Logs

Tail logs when the runtime provides `AIONUI_LOG_DIR`:

```bash
"$AIONUI_HELPER_BIN" diagnose logs tail <<'JSON'
{
  "lines": 100,
  "errors_only": true,
  "conversation_id": "current"
}
JSON
```

If `AIONUI_LOG_DIR` is unavailable and the user gives a log directory, pass it
explicitly:

```bash
"$AIONUI_HELPER_BIN" diagnose logs tail <<'JSON'
{
  "log_dir": "/Users/alex/Library/Logs/AionUi",
  "lines": 100,
  "errors_only": true,
  "conversation_id": "conv_123"
}
JSON
```

Known benign noise: `No onPostToolUseHook found for tool use ID` warnings can
appear around tool calls and are not automatically the root cause.

## Controlled HTTP Escape Hatch

Use this only when a named command does not cover the diagnostic read.

```bash
"$AIONUI_HELPER_BIN" diagnose http get <<'JSON'
{
  "path": "/api/teams",
  "reason": "Inspect team fields not covered by diagnose teams summary."
}
JSON
```

Constraints:

- GET only.
- Path must be `/health` or start with `/api/`.
- Output is redacted and may be truncated.
- Prefer adding a named CLI command later if the same raw read becomes common.

## Data Source Map

| Concern | Command |
| --- | --- |
| Backend alive / version | `diagnose health` |
| Cross-domain snapshot | `diagnose overview` |
| Conversation runtime | `diagnose conversations get` |
| Conversation messages | `diagnose conversations messages` |
| Provider health | `diagnose providers summary` |
| Scheduled jobs | `diagnose cron summary` |
| MCP servers | `diagnose mcp summary` |
| Teams and member state | `diagnose teams summary` |
| Logs | `diagnose logs tail` |
| Uncovered read-only API | `diagnose http get` |

## Safety Notes

- This skill diagnoses; it does not repair.
- For configuration changes, switch to `aionui-config`.
- For scheduled task creation or updates, use the `cron` skill or
  `aionui-config` cron commands.
- When reporting results, explain evidence and uncertainty: "suspected stuck"
  after one snapshot, "confirmed stuck" only after repeated unchanged snapshots.
