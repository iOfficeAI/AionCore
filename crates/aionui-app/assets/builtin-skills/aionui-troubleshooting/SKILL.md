---
name: aionui-troubleshooting
description: >-
  Diagnose a running AionUi installation — locate and inspect conversations (including stuck/running ones), read aioncore logs, check LLM provider health, list scheduled cron jobs and their last run status, inspect teams and member state, and check MCP server health. Use when the user reports that AionUi is misbehaving: a conversation is stuck or errored, an LLM/provider call is failing, a scheduled task did not run, an MCP server has no tools, a team member is hung, or they just ask "what's wrong with AionUi" / "排查一下 aionui". Engine-agnostic — works the same for claude / aionrs / gemini / openclaw conversations.
---

> **⚠️ Platform note — read before running any command.** The shell snippets in this skill are written for **macOS / Linux** (bash/zsh). Always check which OS you are on first. On **Windows** do **not** run them verbatim — the underlying tool/CLI commands are usually cross-platform, but the surrounding shell syntax is not. Translate it to PowerShell before running:
>
> | bash (macOS / Linux) | PowerShell (Windows) |
> | --- | --- |
> | `a && b` | run as two steps, or `a; if ($?) { b }` |
> | `cat <<'EOF' \| tool …` (heredoc) | write the text to a temp file, then pipe/pass that file to the tool |
> | `VAR=$(cmd)` … `$VAR` | `$VAR = cmd` … `$VAR` |
> | `cmd > /dev/null` | `cmd > $null` |
> | `… \| grep PAT` | `… \| Select-String PAT` |
> | `… \| jq …` | `… \| ConvertFrom-Json`, then read the fields |
> | `python3 x.py` | `python x.py` (or `py x.py`) |
> | `~/dir`, `/tmp` | `$env:USERPROFILE\dir`, `$env:TEMP` |
> | `cp` / `mkdir -p` / `rm -rf` | `Copy-Item` / `New-Item -ItemType Directory -Force` / `Remove-Item -Recurse -Force` |
>
> If a command has no obvious Windows equivalent, prefer the built-in file/HTTP tools over raw shell.

# AionUi Troubleshooting

Diagnose a running AionUi installation by reading its **project-level** data:
the aioncore REST API, the unified SQLite store, and the aioncore log files.

This is **engine-agnostic**. AionUi runs conversations on several backends
(`acp`/claude, `aionrs`, `gemini`), but troubleshooting goes
through AionUi's own data — the `conversations` API, the unified `messages`
table, provider health, crons, teams, MCP — so the same checks work no matter
which engine a conversation uses. Do **not** reach into engine-specific
transcript files (e.g. `~/.claude/projects/*.jsonl` or
`~/.aionui/aionrs-sessions/*.json`); they are implementation details of one
backend and are already covered by the unified `messages` table.

## How it works

AionUi is front/back separated: the Electron UI talks to a local `aioncore`
backend. The backend's REST port is **dynamic** (aioncore launches with
`--port 0`), so the first step is always discovery. The helper script discovers
everything from the running process and wraps every read.

## Setup

One self-contained helper — no external dependencies, read-only everywhere.

```bash
python3 scripts/aion_diag.py discover
```

`discover` finds the live backend and prints what every other command relies on:

```json
{
  "pid": 86716,
  "base_url": "http://127.0.0.1:58188",
  "port": 58188,
  "log_dir": "/Users/you/Library/Logs/AionUi",
  "data_dir": "/Users/you/.aionui",
  "version": "2.1.18",
  "db_path": "/Users/you/.aionui/aionui-backend.db"
}
```

How discovery works (and why it's robust):
- The long-lived backend process is `aioncore`; short-lived helper subprocesses
  may include `mcp-team-stdio` for active Team sessions.
- Reads `--log-dir`, `--data-dir`, `--app-version` straight from its argv — so
  if the user changed the log directory, **we follow it**, never hardcode it.
- The REST port is NOT in argv (`--port 0`), so the script probes every port the
  process listens on and keeps the one that answers `/health` with `status:ok`.

If `discover` exits with an error / code 3, AionUi is **not running**. Tell the
user to launch it — do not guess a port.

> The script redacts secrets (`api_key`, tokens, …) in all output. Provider
> records contain plaintext API keys; never print raw provider JSON yourself —
> go through the helper.

## Golden rule: start wide, then drill in

For "something is wrong with AionUi" with no specifics, run `overview` first —
it's a one-shot snapshot across health, providers, MCP, crons, and running
conversations. Then drill into whatever it flags.

```bash
python3 scripts/aion_diag.py overview
```

---

## Commands by symptom

All commands take the discovered backend automatically. Output is JSON unless
noted.

### "A conversation is stuck / errored / behaving wrong"

```bash
python3 scripts/aion_diag.py conversations [--limit N]   # list: id, name, engine type, status
python3 scripts/aion_diag.py conversation <id>           # status + runtime + recent errors + stuck hint
python3 scripts/aion_diag.py messages <id> [--limit N] [--errors]   # message history from SQLite
```

- `conversation <id>` is the workhorse. It returns the live `runtime` block
  (`state`, `task_status`, `is_processing`, `turn_id`), the 5 most recent
  **error** messages, and a `stuck_hint` when `state=running` +
  `is_processing=true`.
- **Stuck detection is comparative, not absolute.** A single `running` snapshot
  is normal — that may just be the active turn. To confirm a hang, run
  `conversation <id>` a few times seconds apart: if `turn_id` and runtime never
  change while no new messages arrive, it's stuck. Cross-check with
  `logs --conv <id>`.
- `messages <id> --errors` pulls just the failed messages/tool-calls for that
  conversation from the unified `messages` table (engine-agnostic).

### "An LLM / provider call is failing"

```bash
python3 scripts/aion_diag.py providers
```

Lists every configured provider with its `model_health` (`status`, `latency`,
`last_check`) and an `unhealthy_models` summary. A provider whose models show
non-`healthy` status, huge `latency`, or a stale `last_check` is the suspect.
Then confirm with the log (filter by the provider's base_url or id):

```bash
python3 scripts/aion_diag.py logs --errors --lines 100
```

> `model_health` only holds the **most recent** check per model — there's no
> historical error log in it. For the actual failure cause (timeout, 401, 429,
> bad base_url), the aioncore log is the source of truth.

### "A scheduled task didn't run"

```bash
python3 scripts/aion_diag.py crons
```

This reads the `cron_jobs` table from the SQLite store directly (read-only). A
REST API for crons does exist (`/api/cron/jobs`, used by the `aionui-config`
skill to *create/manage* jobs), but for diagnosis the table read is more direct
and surfaces the run-state columns in one shot. It surfaces a `failing` list
(jobs whose `last_status` is `error` or `missed`) plus every job's `schedule_*`,
`last_status`, `last_error`, `next_run_at`, `last_run_at`, `run_count`,
`retry_count`. Check `enabled`, compare `next_run_at` to now, and read
`last_error` for failed jobs.

### "An MCP server has no tools / isn't working"

```bash
python3 scripts/aion_diag.py mcp
```

Lists MCP servers with `enabled`, `transport`, and `tool_count`. It flags any
server that is **enabled but exposes 0 tools** — the classic "failed to start"
signature (bad command, missing binary, crashed on launch). Then check the log
around startup. A `disabled` server with 0 tools is fine — the user turned it off.

### "A team / team member is hung"

```bash
python3 scripts/aion_diag.py teams
```

Lists teams and members; for each member it resolves the linked
`conversation_id` to its live `conv_state`. A member stuck in `running` (while
others are `idle`) is the one to drill into with `conversation <member-conv-id>`.

### "Is the backend even alive? What version?"

```bash
python3 scripts/aion_diag.py health      # GET /health: status + core version + build_time
python3 scripts/aion_diag.py discover    # also shows app version, port, dirs
```

### Reading logs

```bash
python3 scripts/aion_diag.py logs [--lines N] [--errors] [--conv <id>]
```

- Tails the latest `*.aioncore.log` in the discovered `log_dir` (NDJSON: one
  JSON object per line — timestamp, level, target, HTTP status/path).
- `--errors` keeps only ERROR/WARN/error/panic lines.
- `--conv <id>` keeps only lines mentioning that conversation id — the fastest
  way to see one conversation's request/response trace.
- Need an arbitrary endpoint not wrapped above? `python3 scripts/aion_diag.py
  get /api/<path>` does a raw (redacted) GET.

> Known log noise: `"No onPostToolUseHook found for tool use ID: ..."` WARNs fire
> on nearly every tool call and are benign SDK-level chatter — don't treat them
> as the fault.

---

## Data source map

| Concern | Source | Access |
| --- | --- | --- |
| Backend alive / version | `GET /health` | REST |
| Conversation list + runtime state | `GET /api/conversations[/{id}]` | REST |
| Conversation messages / errors | `messages` table (by `conversation_id`) | SQLite (read-only) |
| LLM provider health | `GET /api/providers` → `model_health` | REST (api_key redacted) |
| Scheduled jobs | `cron_jobs` table (REST `/api/cron/jobs` exists too) | SQLite (read-only) |
| Teams + members | `GET /api/teams` | REST |
| MCP servers | `GET /api/mcp/servers` | REST |
| Logs | `*.aioncore.log` in `--log-dir` | File tail |

`db_path` and `log_dir` are always taken from `discover` (process argv), never
hardcoded, so they track whatever the user configured.

## Verification / safety notes

- All SQLite access is opened **read-only** (`mode=ro`) — diagnosis never mutates
  the live store.
- All output is run through secret redaction. If you ever need to show a provider
  record, use the helper's `providers` command, not a raw `get`.
- Confirm "stuck" only after repeated snapshots — one `running` reading is not a
  fault.
- This skill is **read-only diagnosis**. To *change* configuration (create/edit
  assistants, add MCP servers, attach skills), use the separate
  `aionui-config` skill.

## Not yet covered

- No streaming/live log follow — only tail-on-demand.
- No historical provider-error timeline (only the latest `model_health` check);
  reconstruct history from the logs.
- Repairing a broken conversation (vs. diagnosing it) is out of scope.
