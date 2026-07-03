---
name: aionui-config
description: >-
  Configure AionUi itself through its backend API — create and edit assistants (name, avatar, system prompt, quick-start prompts, engine), import and attach skills, manage MCP servers, configure LLM providers (add/edit a model endpoint, set the API key, fetch the model list, pick the default model), change app/UI settings (language, theme, font size, zoom, notifications), and create or manage scheduled tasks (cron jobs) from a natural-language schedule. Use when the user wants you to set up an AionUi assistant, sink a skill into AionUi's skill registry, attach skills to an assistant, change an assistant's avatar or system prompt, add or configure an MCP server, add an LLM/model provider or API key, switch the default model, change the theme or language, schedule a recurring or one-off task ("every morning at 9", "remind me in 2 hours", "run this daily"), or otherwise configure their AionUi installation. This is "Agent-assisted AionUi configuration": you act on the user's behalf via the local backend.
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

# AionUi Config

Configure a running AionUi installation by calling its backend (aioncore) REST API.
Everything here has been verified end-to-end against a live backend.

## How it works

AionUi is front/back separated. The Electron UI talks to a local `aioncore`
backend over HTTP. Assistants, skills, and their rules all live behind that
backend — there is no config file to edit anymore. You configure AionUi by
calling the API.

The backend port is **dynamic** (it changes every launch and is not persisted
to a file), so the first step is always to discover it.

## Setup

A helper script wraps discovery + requests. Use it for every call.

```bash
cd <this-skill-dir>
python3 scripts/aionui_api.py discover          # prints e.g. http://127.0.0.1:57282
```

If `discover` fails, AionUi is not running — tell the user to launch it, don't guess a port.

Helper commands (all print the JSON response):

```bash
python3 scripts/aionui_api.py get    <path>
python3 scripts/aionui_api.py post   <path> '<json-body>'
python3 scripts/aionui_api.py put    <path> '<json-body>'
python3 scripts/aionui_api.py patch  <path> '<json-body>'
python3 scripts/aionui_api.py delete <path>
```

## Golden rule: read before you write

Before editing anything, `get` the current state and show the user what you're
about to change. Configuration changes take effect on the user's live app.
After every write, read it back to confirm. The user does not need a dry-run
unless they ask, but they should always see what changed.

---

## Assistants

An assistant has two parts stored separately:

1. **Metadata** — name, description, avatar, engine, quick-start prompts, defaults.
   Lives in the assistant record (`/api/assistants`).
2. **System prompt (rules)** — the long instruction text that gives the
   assistant its behavior. Stored in a **separate file** (`storage_mode:
   user_file`), written via a dedicated endpoint. Creating an assistant does
   NOT set its system prompt — that's a second call.

Assistant `source` is `builtin` (shipped with the app, limited edits), `user`
(custom, fully editable), or `generated` (auto-materialized from an online ACP
agent — identity fields locked, not deletable). Custom IDs look like
`custom-<digits>-<hex>`.

### List / inspect

```bash
python3 scripts/aionui_api.py get /api/assistants
# Add ?locale=<loc> to load the per-locale rules file into rules.content:
python3 scripts/aionui_api.py get "/api/assistants/<id>?locale=zh-CN"
```

> `?locale=` is optional. Without it, `rules.content` falls back to the
> assistant's inline rule content (empty if none is stored). With it, content is
> loaded from the per-locale rule file. Pass `?locale=` whenever you need the
> locale-specific prompt — it's recommended, not required.

### Create

`POST /api/assistants`. Only `name` is required. The backend assigns the `id`.

```bash
python3 scripts/aionui_api.py post /api/assistants '{
  "name": "需求梳理官",
  "description": "以新人视角梳理产品需求文档(PRD)",
  "agent_id": "<engine-agent-id>",
  "prompts": [
    "我来描述一个需求,你帮我梳理成一份 PRD",
    "review 这份 PRD,挑出对新人不友好的地方"
  ]
}'
```

Key fields in the create/update body:

| Field | Meaning |
| --- | --- |
| `name`, `description` | display text (required: name) |
| `agent_id` | **engine binding** — the id of an installed agent (see "Picking the engine" below). This, not `preset_agent_type`, is what actually sets the engine |
| `preset_agent_type` | display/i18n hint only; does **not** bind the engine in the current backend |
| `prompts` | quick-start prompts shown on the assistant (NOT the system prompt) |
| `avatar` | emoji, image URL, `data:` URI, or absolute local path |
| `enabled_skills` | skill names attached to this assistant |
| `custom_skill_names` | extra custom skill names to attach beyond `enabled_skills` |
| `disabled_builtin_skills` | builtin skill names to turn OFF for this assistant |
| `recommended_prompts` (+ `recommended_prompts_i18n`) | optional secondary prompt set |
| `models`, `name_i18n`, `description_i18n`, `prompts_i18n` | optional |
| `defaults` | per-assistant defaults — see below (settable on **create**, not just update) |

> The create and update bodies take the same fields. On GET, the assistant also
> carries read-only `context` / `context_i18n` and `last_used_at` (unix ms) — you
> can't set those via POST/PUT.

### Picking the engine (`agent_id`)

The engine is bound by the request-body field **`agent_id`**, whose value is an
installed agent's id — not a friendly name like `"claude"`. Read the available
agents first and copy the id you want:

```bash
# the LIST response is flat — each row carries agent_id + agent directly:
python3 scripts/aionui_api.py get /api/assistants
#   {"id": "...", "agent_id": "2d23ff1c", "agent": {"type": "acp", "acp_backend": "claude"}, ...}
# (only the single-assistant detail read nests these under an `engine` block:
#   GET /api/assistants/<id>?locale=en -> "engine": {"agent_id": "...", "agent": {...}})
# reuse an existing agent_id for a new assistant on the same engine:
python3 scripts/aionui_api.py put /api/assistants/<id> '{"agent_id":"2d23ff1c"}'
```

> If you omit `agent_id` on create, the backend does NOT default to a CLI engine:
> with at least one enabled provider it falls back to `aionrs` (its built-in
> agent), and with no provider configured it returns a 400. CLI engines
> (`claude`, `gemini`, `codex`, …) must be opted into explicitly with their
> `agent_id` — an Anthropic key alone doesn't put the Claude CLI on `PATH`.
> On read, the bound engine shows up in the assistant's `engine.agent_id` /
> `engine.agent.acp_backend`; the create-body `preset_agent_type` is display-only
> and reads back `null`, so don't rely on it to tell you the engine.

### Per-assistant defaults

`defaults` holds four entries; each is `{mode}` or `{mode, value}`. `mode:"auto"`
means "inherit the global default / let the user pick each time" and carries NO
`value`. `mode:"fixed"` locks the assistant to `value` (the user can't change it
while using this assistant). Send only the entries you want to change; read the
assistant first to keep the others.

Every entry's `mode` is only ever `auto` or `fixed` — those two are the only
modes the backend accepts. The `value` is what `fixed` locks to:

| Entry | `fixed` → `value` is | Example |
| --- | --- | --- |
| `model` | a model name (string) | `{"mode":"fixed","value":"gemini-2.5-pro"}` |
| `permission` | a permission-name string (free-form; the backend does not enum-validate it). Common names: `plan`, `default` | `{"mode":"fixed","value":"plan"}` |
| `skills` | skill names (string[]) | `{"mode":"fixed","value":["aionui-config"]}` |
| `mcps` | MCP server names (string[]) | `{"mode":"fixed","value":["filesystem"]}` |

> `permission.value` is whatever permission name the active agent/permission
> system understands — it is stored as an opaque string, not checked against a
> fixed list. (`acceptEdits` / `bypassPermissions` / `dontAsk` are **agent-level
> YOLO IDs**, a separate concept — don't assume they're valid here.)

```bash
python3 scripts/aionui_api.py put /api/assistants/<id> '{
  "defaults": {
    "model":      {"mode": "fixed", "value": "gemini-2.5-pro"},
    "permission": {"mode": "fixed", "value": "plan"},
    "skills":     {"mode": "auto"},
    "mcps":       {"mode": "fixed", "value": ["filesystem"]}
  }
}'
```

> Verified end-to-end: the backend stores all four entries verbatim and returns
> them on the `?locale=` detail read. On read, `defaults` is always present with
> all four entries — a brand-new assistant has each set to `{"mode":"auto"}` (no
> `value`), never `null`. Lock one by sending `{"mode":"fixed","value":...}`.

### Update

`PUT /api/assistants/<id>`, sending only the fields you want to change. The `id`
comes from the URL path — a body `id`, if sent, is ignored.

### Set the system prompt (rules)

This is the separate second step — the actual behavior of the assistant.

```bash
python3 scripts/aionui_api.py post /api/skills/assistant-rule/write '{
  "assistant_id": "<id>",
  "content": "<full system prompt markdown>",
  "locale": "zh-CN"
}'
```

Read it back:

```bash
python3 scripts/aionui_api.py post /api/skills/assistant-rule/read '{"assistant_id":"<id>","locale":"zh-CN"}'
```

For multi-line / long prompts, write the text to a temp file and build the JSON
body in Python rather than inlining a giant shell string.

> To wipe an assistant's rule across all locales, `DELETE
> /api/skills/assistant-rule/<assistant-id>`. A parallel trio exists for
> per-assistant **skill** content (distinct from the shared registry):
> `POST /api/skills/assistant-skill/{read,write}` (same body shape as the rule
> endpoints) and `DELETE /api/skills/assistant-skill/<assistant-id>`.

### Avatar

The `avatar` field accepts an emoji (`"📋"`), an image URL, a `data:` URI, or an
absolute local path. A self-contained inline SVG `data:` URI is a good default —
no external dependency, renders offline:

```bash
python3 scripts/aionui_api.py put /api/assistants/<id> '{"avatar":"data:image/svg+xml;base64,<...>"}'
```

> `GET /api/assistants/<id>/avatar` also serves the raw avatar binary (Content-Type
> inferred; 404 if none) — handy for an `<img src>`, but a `data:` URI is still the
> better default for self-contained config.

### Enable / disable / reorder

`PATCH /api/assistants/<id>/state` with any of `enabled`, `sort_order`, and
`last_used_at` (unix ms). Disabling hides the assistant from the homepage and
team picker without deleting it.

### Delete

`DELETE /api/assistants/<id>` — only `source: user` assistants can be deleted.
`builtin` and `generated` assistants can only be disabled (check the `deletable`
flag on the detail read).

### Bulk import

`POST /api/assistants/import` inserts many assistants at once (e.g. restoring a
backup or migrating from a legacy Electron config). Body is `{"assistants":
[<CreateAssistantRequest>, …]}`; the response reports `imported` / `skipped` /
`failed` counts plus a per-row `errors` array. It's insert-only — it won't
overwrite an existing id.

---

## Skills

A skill is a folder containing a `SKILL.md` (YAML frontmatter `name` +
`description`, then instruction body). The `description` decides when the agent
auto-triggers the skill, so write it carefully.

Four sources: `builtin` (`~/.aionui/builtin-skills/`), `custom`
(`~/.aionui/skills/`), `cron` (`~/.aionui/cron/skills/`, per-scheduled-task
skills), `extension` (external, symlinked).

### List / inspect the registry

```bash
python3 scripts/aionui_api.py get /api/skills
python3 scripts/aionui_api.py get /api/skills/paths          # where skills live on disk
python3 scripts/aionui_api.py post /api/skills/info '{"skill_path":"/abs/path/to/skill-folder"}'  # read a SKILL.md's name/description WITHOUT importing
```

### Import a skill into the registry

`POST /api/skills/import` copies the skill(s) into the user skills dir and
registers them. The one endpoint handles all three source shapes: a single skill
folder, a PARENT folder containing many skills, or a `.zip` package.

```bash
python3 scripts/aionui_api.py post /api/skills/import '{"skill_path":"/abs/path/to/skill-or-parent-or-zip"}'
python3 scripts/aionui_api.py get  /api/skills/import-limits   # server-side max file/total byte caps
python3 scripts/aionui_api.py get  /api/skills/import-history  # recent import records
```

> For external skills you keep editing in place (registered without copying), see
> "Discover & manage skill sources" below (`external-paths`) — that's how a live,
> non-copied source is wired in. There is no `import-symlink` endpoint; the
> reverse operation, `POST /api/skills/export-symlink`
> (`{"skill_path":"...","target_dir":"..."}`), symlinks an already-installed skill
> back out to an external directory.

> Caution: importing (copy) from a path that is ALREADY inside the user skills
> dir can race with the copy step. When editing an installed skill, edit the
> files in place, then re-import from a separate staging copy — or just verify
> the SKILL.md is non-empty afterwards. An empty SKILL.md unregisters the skill.

### Discover & manage skill sources

For skills that live outside the standard dirs:

```bash
python3 scripts/aionui_api.py post   /api/skills/scan '{"folder_path":"/abs/dir"}'   # find skills under a dir
python3 scripts/aionui_api.py get    /api/skills/detect-paths                  # candidate skill locations
python3 scripts/aionui_api.py get    /api/skills/detect-external               # external skill dirs
python3 scripts/aionui_api.py get    /api/skills/external-paths                # list registered external paths
python3 scripts/aionui_api.py post   /api/skills/external-paths '{"name":"<label>","path":"/abs/dir"}'  # add one (both required)
python3 scripts/aionui_api.py delete /api/skills/external-paths '{"path":"/abs/dir"}'   # remove one (path only)
```

The **skills market** is a separate, app-wide toggle:
`POST /api/skills/market/enable` and `/disable`.

### Attach a skill to an assistant

Put the skill's `name` into the assistant's `enabled_skills`:

```bash
python3 scripts/aionui_api.py put /api/assistants/<id> '{"enabled_skills":["skill-a","skill-b"]}'
```

> `enabled_skills` is the full set — include every skill you want kept, not just
> the new one. Read the assistant first to get the current list.

### Delete a skill

```bash
python3 scripts/aionui_api.py delete /api/skills/<skill-name>
```

---

## MCP servers

AionUi can connect to MCP servers. The whole lifecycle is available under
`/api/mcp/*` and is verified end-to-end (create / list / toggle / delete).

### List

```bash
python3 scripts/aionui_api.py get /api/mcp/servers
```

Each server has `id`, `name`, `description`, `enabled`, `builtin`, and a
`transport`. Builtin servers (`builtin: true`) ship with the app — don't delete
those; create `builtin: false` ones for the user.

### Transport shapes

The `transport` object is one of:

| Type | Fields | For |
| --- | --- | --- |
| `stdio` | `command`, `args?` (string[]), `env?` (map) | local process servers (npx/uvx/binaries) |
| `sse` | `url`, `headers?` (map) | remote Server-Sent-Events servers (legacy) |
| `http` | `url`, `headers?` (map) | remote HTTP servers (Streamable HTTP) |

> `headers` is an optional string→string map for auth (e.g. `{"Authorization":
> "Bearer …"}`). The REST API accepts exactly these three `type` values —
> `stdio`, `sse`, `http`. Use `http` for Streamable-HTTP servers; there is no
> separate `streamable_http` transport type at this layer (sending it fails
> deserialization).

### Create

`POST /api/mcp/servers`. Required: `name`, `transport`. Set `builtin: false`.

```bash
# stdio (local) — e.g. a filesystem server via npx
python3 scripts/aionui_api.py post /api/mcp/servers '{
  "name": "filesystem",
  "description": "local filesystem access",
  "transport": {"type": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/some/dir"]},
  "builtin": false
}'

# remote (http)
python3 scripts/aionui_api.py post /api/mcp/servers '{
  "name": "my-remote",
  "transport": {"type": "http", "url": "https://example.com/mcp"},
  "builtin": false
}'
```

### Test connection before trusting it

`POST /api/mcp/test-connection` with the same server body actually connects and
returns the server's tool list (or an error / `needs_auth`). Good to run after
creating a remote server.

### Fetch one / toggle / update / delete

```bash
python3 scripts/aionui_api.py get    /api/mcp/servers/<id>          # one server by id
python3 scripts/aionui_api.py post   /api/mcp/servers/<id>/toggle   # enable <-> disable
python3 scripts/aionui_api.py put    /api/mcp/servers/<id> '{"description":"..."}'
python3 scripts/aionui_api.py delete /api/mcp/servers/<id>
```

> Two more list-level helpers exist: `POST /api/mcp/servers/import`
> (`{"servers":[…]}`, bulk-restore a set at once) and `GET /api/mcp/agent-configs`
> (scans installed Agent CLIs and returns their existing MCP configs — a source
> for one-click import).

> Remote servers may need OAuth: `/api/mcp/oauth/check-status`,
> `/api/mcp/oauth/login`, `/api/mcp/oauth/logout` (all `post`), and
> `GET /api/mcp/oauth/authenticated` which lists the server URLs that already
> have a stored token. Only touch these if a `test-connection` came back with
> `needs_auth`.

---

## LLM providers (models & API keys)

This is where the actual models come from. A **provider** is one upstream
(Gemini, an OpenAI-compatible endpoint, Anthropic, Bedrock, …) holding a
`base_url`, an `api_key`, and the list of `models` it exposes. Assistants then
pick a model from an enabled provider. The whole lifecycle is verified
end-to-end (list / create / fetch-models / detect-protocol / update / delete).

> **Secret-handling rule:** `GET /api/providers` returns every `api_key` in
> **plaintext** (a pre-launch convention for the local-store → backend migration;
> it may be masked in a future release, so confirm before relying on it). Never
> paste a provider response into chat, a commit, a log, or a memory file. When
> you must show the user a provider, redact the key (`sk-…last4`). Treat keys the
> user gives you the same way.

### List / inspect

```bash
python3 scripts/aionui_api.py get /api/providers
```

Each provider: `id`, `platform`, `name`, `base_url`, `api_key`, `models`
(string[]), `enabled`, `capabilities`, `is_full_url`.

`platform` selects how the backend talks to and lists models for the upstream:

- `anthropic` (alias `claude`), `gemini`, `bedrock` — native protocols.
- `vertex-ai`, `minimax` — known vendors with a hardcoded model list (no live
  fetch).
- `new-api` — OpenAI protocol with `/v1` path enforcement.
- `dashscope-coding` — DashScope coding endpoint.
- `custom` (the default) → **OpenAI-compatible**. Use this for OpenAI itself,
  DeepSeek, OpenRouter, Ollama, vLLM, and any other OpenAI-protocol endpoint,
  with its `base_url`.

When unsure, run `detect-protocol` (below) — it fills `platform` and `models`
for you instead of guessing.

### Detect the protocol before creating (recommended)

Given a `base_url` + `api_key`, the backend probes the endpoint and tells you
which protocol it speaks and what models it has. Use this to fill `platform` and
`models` correctly instead of guessing.

```bash
python3 scripts/aionui_api.py post /api/providers/detect-protocol '{
  "base_url": "https://api.deepseek.com/v1",
  "api_key": "sk-..."
}'
# -> {"protocol":"openai","confidence":90,"models":[...]}
```

Required on this body: just `base_url` + `api_key` (no `platform` — the backend
detects it). Optional: `timeout` (ms), `preferred_protocol` (try a given protocol
first — one of `openai`, `anthropic`, `gemini`, `unknown`), and `test_all_keys`
(bool — probe every key when `api_key` holds several).

`fetch-models` (`POST /api/providers/fetch-models`) returns just the model list
for a not-yet-saved endpoint. Its body differs from detect-protocol: `platform`,
`base_url`, `api_key` are all **required** (plus optional `bedrock_config`,
`try_fix`).

### Test a provider connection

- `POST /api/agents/provider-health-check` with `{"provider_id":"<id>","model":"<model>"}`
  checks that a saved provider+model actually answers. (This lives on the agents
  router — it's what surfaces an assistant's availability.)
- `POST /api/bedrock/test-connection` validates AWS Bedrock credentials before
  you save a Bedrock provider.

### Create

`POST /api/providers`. Required: `platform`, `name`, `base_url`. Provide
`models` (the ones to expose) and `enabled`.

```bash
python3 scripts/aionui_api.py post /api/providers '{
  "platform": "custom",
  "name": "DeepSeek",
  "base_url": "https://api.deepseek.com/v1",
  "api_key": "sk-...",
  "models": ["deepseek-chat", "deepseek-reasoner"],
  "enabled": true
}'
```

### Refresh the live model list of a saved provider

```bash
python3 scripts/aionui_api.py post /api/providers/<id>/models '{"try_fix": false}'
```

Returns the models the upstream currently advertises — use it to refresh a
provider's `models` after the vendor adds new ones.

### Update / delete

```bash
python3 scripts/aionui_api.py put    /api/providers/<id> '{"models": ["...","..."]}'
python3 scripts/aionui_api.py delete /api/providers/<id>
```

> Send only the fields you want changed on `put`. To rotate a key, `put`
> `{"api_key": "..."}`. To disable a provider without losing it, `put`
> `{"enabled": false}`.

### Which model an assistant uses

Lock a model to an assistant via its per-assistant `defaults.model`
(`{"mode":"fixed","value":"<model-name>"}`) — see *Per-assistant defaults*
above. The model name must be one the provider exposes (`get /api/providers`).

---

## Global & client settings

Two stores, both verified:

- `GET /api/settings` — app-level switches: `language`, `notification_enabled`,
  `cron_notification_enabled`, `command_queue_enabled`, `save_upload_to_workspace`.
  Update them with `PATCH /api/settings` (partial — not PUT).
- `GET /api/settings/client` — read the larger UI/runtime key-value store:
  `language`, `theme.activeId` (`light`/`dark`/custom), `ui.zoomFactor`,
  `ui.fontSize.{chat,markdown,code}`, `webui.desktop.allowRemote`, …
- `PUT /api/settings/client` — batch-update that store.

`PUT /api/settings/client` is a **partial merge** — send only the keys you want
to change (a key set to `null` deletes it). Its response carries no data, so
always read the store back to confirm.

```bash
python3 scripts/aionui_api.py get /api/settings/client
python3 scripts/aionui_api.py put /api/settings/client '{"ui.zoomFactor": 1.0}'
python3 scripts/aionui_api.py get /api/settings/client   # confirm — PUT returns no body
```

> To set which model a given assistant uses, configure that assistant's
> `defaults.model` (see *Per-assistant defaults*) — not a global setting.

---

## Engines (agents)

`GET /api/agents/management` lists the available engines (`aionrs`, `claude`,
`codex`, …). There is **no** bare `GET /api/agents` — that path 404s; always use
the `/management` sub-path. Each row is rich: alongside `id`, `name`, `enabled`
(toggled on), `installed` (spawn command resolvable on `$PATH`), `team_capable`
(can run in a team), `backend`, `agent_type`, and a `status` of `online` /
`offline` / `missing` / `unchecked`, it also carries `config_options`,
`available_modes`, `available_models` (when the engine advertises them), plus
`last_check_*` diagnostics. `unchecked` means no persisted connectivity check has
run for that row yet. Check `installed` (and `status`) before binding an
assistant to that engine (via its `agent_id` — see *Picking the engine* above).

The management row is the supported engine catalog surface. Do not call legacy
agent refresh endpoints; connectivity checks are explicit per-agent operations.

---

## Scheduled tasks (cron)

Create and manage scheduled tasks ("run this every morning at 9", "remind me in
two hours", "every 30 minutes do X") over the REST API — `/api/cron/*` is a
full, verified CRUD surface. Translate the user's natural-language schedule into
one of three `schedule` shapes, then `POST /api/cron/jobs`.

### The schedule (`schedule` is a tagged union — the `kind` field picks the shape)

| Natural language | `schedule` body |
| --- | --- |
| "at 3pm today / on this exact date" (one-shot) | `{"kind":"at","at_ms":<unix-ms>}` |
| "every 30 minutes / every 2 hours" (fixed interval) | `{"kind":"every","every_ms":<ms>}` |
| "every day at 9am / every Monday" (calendar) | `{"kind":"cron","expr":"0 9 * * *","tz":"Asia/Shanghai"}` |

- **`cron.expr` takes a standard 5-field crontab** (`min hour day month weekday`).
  The backend auto-prepends the seconds field, so `0 9 * * *` means 09:00 daily.
  (A 6-field `sec min hour day month weekday` is also accepted as-is.)
- **`cron.tz`** is an IANA timezone name (`Asia/Shanghai`, `America/New_York`,
  `UTC`). Omit it and the expression runs in UTC — always set it to the user's
  zone for "9am" to mean their 9am.
- **`every.every_ms`** must be `> 0`. There is no documented lower bound, but be
  sensible — don't schedule a sub-minute loop unless asked.
- **`at.at_ms`** is a Unix timestamp in **milliseconds**. A past time is accepted
  by the API but will not run, so compute it from "now" in the user's zone.
- An optional `description` can go inside any schedule variant for a
  human-readable label.

### Required fields on `POST /api/cron/jobs`

```bash
python3 scripts/aionui_api.py post /api/cron/jobs '{
  "name": "每日早报",
  "schedule": {"kind": "cron", "expr": "0 9 * * *", "tz": "Asia/Shanghai"},
  "message": "总结今天的科技新闻",
  "conversation_id": "<conv-id>",
  "created_by": "agent",
  "execution_mode": "new_conversation",
  "agent_config": {"name": "AionUi Butler", "assistant_id": "<assistant-id>"}
}'
```

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | ✅ | display name of the task |
| `schedule` | ✅ | one of the three shapes above |
| `conversation_id` | ✅ | the conversation the task is tied to — get one from `GET /api/conversations` (or create one). Even `new_conversation` jobs need this set |
| `created_by` | ✅ | `"agent"` when you create it on the user's behalf, `"user"` for a user-initiated one. **Only these two values** |
| `message` (or `prompt`) | — | the instruction sent on each run. `message` wins if both are given; with neither, the run sends an empty prompt |
| `execution_mode` | — | `"existing"` (default) reuses `conversation_id` every run; `"new_conversation"` spins up a fresh conversation each run |
| `agent_config` | — | which assistant runs the task. **In practice required for a new job**: omit it and the API 400s with *"assistant_id is required for new cron jobs"*. Pass `{"name":"<label>","assistant_id":"<id>"}` |
| `description` | — | optional longer description |

> `agent_config` is strict (`deny_unknown_fields`): only `name`, `assistant_id`,
> `cli_path`, `mode`, `model_id`, `model`, `config_options`, `workspace` are
> accepted. Legacy keys `backend`, `agent_type`, `custom_agent_id`, `is_preset`
> are **rejected** — don't send them. Get the `assistant_id` from
> `GET /api/assistants`.

The response is the created job (HTTP 201) with its generated `id` (prefixed
`cron_…`), resolved `agent_type`, and a `state` block (`next_run_at_ms`,
`run_count`, …).

### List / inspect / change / run / delete

```bash
python3 scripts/aionui_api.py get    /api/cron/jobs                       # all jobs
python3 scripts/aionui_api.py get    "/api/cron/jobs?conversation_id=<id>"  # jobs for one conversation
python3 scripts/aionui_api.py get    /api/cron/jobs/<id>                   # one job
python3 scripts/aionui_api.py put    /api/cron/jobs/<id> '{"enabled": false}'   # partial update (pause)
python3 scripts/aionui_api.py post   /api/cron/jobs/<id>/run              # run it once right now
python3 scripts/aionui_api.py delete /api/cron/jobs/<id>                  # remove it
python3 scripts/aionui_api.py get    /api/cron/jobs/<id>/conversations    # conversations this job has spawned
python3 scripts/aionui_api.py get    /api/cron/jobs/<id>/skill            # {"has_skill": bool}
python3 scripts/aionui_api.py post   /api/cron/jobs/<id>/skill '{"content":"<SKILL.md body>"}'  # attach/replace a per-job skill
python3 scripts/aionui_api.py delete /api/cron/jobs/<id>/skill            # remove the attached skill
```

> A job can carry its own inline **skill** (a `SKILL.md`-style instruction body)
> via `.../skill` — this is the `cron` skill source. Handy when a scheduled task
> needs bespoke instructions that shouldn't live in the shared registry.

`PUT` is a partial update — send only what changes (`name`, `description`,
`enabled`, `schedule`, `message`, `execution_mode`, `agent_config`,
`conversation_title`, `max_retries`). Read the job back to confirm its
`schedule` and `state.next_run_at_ms` after any change.

> Note: an `existing`-mode job can't have its assistant changed after creation
> (`agent_config` on update is rejected for ongoing-conversation jobs) — that's
> by design, the ongoing conversation keeps its original assistant.

---

## Verification checklist

After a configuration task, confirm with reads:

1. Assistant in `get /api/assistants`? Right name, avatar, engine?
2. System prompt set? `assistant-rule/read` returns the expected text.
3. Skill in `get /api/skills` with `source: custom`?
4. Skill attached? Assistant detail `enabled_skills` contains it.
5. MCP server in `get /api/mcp/servers`, enabled, right transport?
6. Provider in `get /api/providers`, enabled, right `models`? (redact the key)
7. Settings changed? `get /api/settings/client` shows the new value.
8. Scheduled task created? `get /api/cron/jobs` lists it, `enabled: true`, with
   the expected `schedule` and a non-null `state.next_run_at_ms`.
9. Tell the user to refresh / reopen the AionUi view to see changes.

## Out of scope (handled elsewhere)

Some backend areas have `/api/*` endpoints but are intentionally NOT this
skill's job — they already have dedicated tooling, so don't reach for the raw
API here:

- **Teams** (`/api/teams/*`) — create Teams through the Team UI or REST API.
  Once a Team session is active, Team agents use the `team_*` MCP tools
  provided by the per-Team `aionui-team` server.

This skill stays focused on *configuration*: assistants, skills, MCP servers,
LLM providers, app settings, and scheduled tasks.

## Not yet covered

Conversation repair (recovering a broken session from its logs) is not covered
here yet.

Channel and extension management (`/api/channel/*`, `/api/extensions/*`) now have
stable request bodies and tests in the backend — they're no longer "unverified",
but they're a large enough surface (plugin pairing, per-extension i18n/permissions,
theme/assistant/skill extensions) that they belong in a dedicated skill rather
than bolted onto this one. Document them there with bodies confirmed against the
live backend — don't guess.
