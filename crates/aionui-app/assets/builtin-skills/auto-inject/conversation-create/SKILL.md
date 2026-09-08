---
name: conversation-create
description: Create a new conversation for this user with the bundled aioncore CLI. Use when the user asks to open, start, or spin up a new conversation.
---

# Conversation Create Skill

Create a new conversation for this user with the bundled agent-facing CLI. By
default the new conversation reuses this conversation's working directory and
assistant; you can point it at another directory or another assistant instead.

## Rules

1. Use this ONLY when the user asked for a new conversation. Never create one
   on your own initiative.
2. `name` is required — pick a short name that describes the task, in the
   user's language.
3. Omit `workspace` to reuse this conversation's directory; pass an absolute
   path of an existing directory to use another one.
4. Omit `assistant_id` to reuse this conversation's assistant. To use another
   assistant, get its id from `"$AIONUI_HELPER_BIN" config assistants list`
   first.
5. Never pass, inline, export, echo, or set any `AIONUI_...` environment
   variable. Call `"$AIONUI_HELPER_BIN" conversation ...` directly and pass the
   payload through a stdin heredoc. Do not write payload JSON files to disk.
6. If this conversation belongs to a team, do not use this skill.
7. Creating does NOT send anything to the new conversation, does NOT open it,
   and does NOT switch the user's current conversation. Report exactly what
   happened: "created" — nothing more.
8. On failure, report the error from stderr/stdout in plain prose; never claim
   the conversation was created.

## Creating a conversation

```bash
"$AIONUI_HELPER_BIN" conversation create <<'JSON'
{
  "name": "重构鉴权模块",
  "workspace": "/absolute/path/to/repo",
  "assistant_id": "asst_xxx"
}
JSON
```

Only `name` is required; drop `workspace` / `assistant_id` to inherit.

## Result

`data.id` is the new conversation's id; `data.name` and `data.workspace` echo
what was persisted; `data.assistant` (when present) is `{ id, name, backend }`.
When `success` is `true` the conversation already exists.

## Exact schemas

```bash
"$AIONUI_HELPER_BIN" conversation capabilities
```
