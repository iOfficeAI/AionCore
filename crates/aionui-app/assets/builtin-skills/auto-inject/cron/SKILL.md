---
name: cron
description: Scheduled task management - create, query, update scheduled tasks to automatically execute operations at specified times.
---

# Scheduled Task Skill

You can manage scheduled tasks for the current conversation. Use the bundled HTTP helper script only.

## Rules

1. Each conversation can have at most one scheduled task.
2. Always query existing tasks before creating or updating.
3. Do not ask for extra confirmation after the user has already requested the scheduling change.
4. Never pass, inline, export, echo, or set any `AIONUI_...` environment variable in any shell command. AionUi injects the required runtime environment before the helper runs.
5. Helper commands must directly call `"$AIONUI_HELPER_BIN" cron-helper ...` without any `AIONUI_...=...` prefix.
6. Pass create and update payloads through stdin heredocs attached to the helper command. Do not write payload JSON files to disk.
7. After a successful create or update, send one short final confirmation that a normal user can understand. Include the task name and schedule description. Do not show internal ids such as `cron_...`.
8. If the helper fails, report the failure from stderr/stdout in normal prose and do not claim the task was created.

## Workflow

1. Run `"$AIONUI_HELPER_BIN" cron-helper list`.
2. If the returned `data` array is empty, create the task with `"$AIONUI_HELPER_BIN" cron-helper create <<'JSON'`.
3. If one task exists and the user wants to change it, update that task with `"$AIONUI_HELPER_BIN" cron-helper update --job-id <job-id> <<'JSON'`.
4. If a task already exists and the user is asking for a different additional task, ask how they want to handle the existing task.
5. Report success or failure from the helper output in normal prose, following the final confirmation rule above.

## Payload

Create and update payloads use the same fields:

- `name`: Short descriptive name.
- `schedule`: Standard 5-field cron expression.
- `schedule_description`: Human-readable schedule.
- `message`: Complete, self-contained instruction sent to the AI when the task runs.

The `message` must tell the AI exactly what to do when the task fires. It should not merely restate the user's scheduling request.

| User says                         | Bad message              | Good message                                                                                  |
| --------------------------------- | ------------------------ | --------------------------------------------------------------------------------------------- |
| "Send me hello every day at 10am" | Send me hello            | Reply with exactly: Hello!                                                                    |
| "Remind me to drink water daily"  | Remind me to drink water | Reply with a friendly reminder to drink water.                                                |
| "Summarize AI news every Monday"  | Summarize AI news        | Search for the latest AI news from this week and produce a concise bullet-point summary.      |

## Examples

Query:

```bash
"$AIONUI_HELPER_BIN" cron-helper list
```

Create:

```bash
"$AIONUI_HELPER_BIN" cron-helper create <<'JSON'
{
  "name": "Weekly Meeting Reminder",
  "schedule": "0 9 * * MON",
  "schedule_description": "Every Monday at 9:00 AM",
  "message": "Reply with a short weekly meeting reminder that includes the current date and time."
}
JSON
```

Update:

```bash
"$AIONUI_HELPER_BIN" cron-helper update --job-id cron_123 <<'JSON'
{
  "name": "Daily Summary",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "Weekdays at 6:00 PM",
  "message": "Review today's conversation context and produce a concise end-of-day summary."
}
JSON
```

Multiline message example:

```json
{
  "name": "每日总结",
  "schedule": "0 9 * * *",
  "schedule_description": "每天上午 9 点",
  "message": "第一段，\n第二段，\n第三段。"
}
```

## Cron Expression

Format: `minute hour day-of-month month day-of-week`.

Example: `0 9 * * MON-FRI` means weekdays at 9:00 AM.

Use only standard cron fields and ranges supported by the backend parser. Do not use Quartz-style extensions such as `L`, `L-N`, `W`, `LW`, `#`, or `?`.
