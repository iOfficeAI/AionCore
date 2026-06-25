---
name: aionui-config
description: >-
  Configure AionUi itself through its backend API — create and edit assistants (name, avatar, system prompt, quick-start prompts, engine), import and attach skills, manage MCP servers, configure LLM providers (add/edit a model endpoint, set the API key, fetch the model list, pick the default model), and change app/UI settings (language, theme, font size, zoom, notifications). Use when the user wants you to set up an AionUi assistant, sink a skill into AionUi's skill registry, attach skills to an assistant, change an assistant's avatar or system prompt, add or configure an MCP server, add an LLM/model provider or API key, switch the default model, change the theme or language, or otherwise configure their AionUi installation. This is "Agent-assisted AionUi configuration": you act on the user's behalf via the local backend.
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

Assistant `source` is `builtin` (shipped with the app, limited edits) or
`user` (custom, fully editable). Custom IDs look like `custom-<digits>-<hex>`.

### List / inspect

```bash
python3 scripts/aionui_api.py get /api/assistants
# Detail MUST include ?locale=<loc> to inline the rules content:
python3 scripts/aionui_api.py get "/api/assistants/<id>?locale=zh-CN"
```

> Without `?locale=`, `rules.content` comes back empty even when a prompt exists.
> That's expected — the rules live in a per-locale file, loaded on demand.

### Create

`POST /api/assistants`. Only `name` is required. The backend assigns the `id`.

```bash
python3 scripts/aionui_api.py post /api/assistants '{
  "name": "需求梳理官",
  "description": "以新人视角梳理产品需求文档(PRD)",
  "preset_agent_type": "claude",
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
| `preset_agent_type` | engine: `claude`, `aionrs`, `codex`, … |
| `prompts` | quick-start prompts shown on the assistant (NOT the system prompt) |
| `avatar` | emoji, image URL, `data:` URI, or absolute local path |
| `enabled_skills` | skill names attached to this assistant |
| `models`, `name_i18n`, `description_i18n`, `prompts_i18n` | optional |
| `defaults` | per-assistant defaults — see below |

### Per-assistant defaults

`defaults` holds four entries; each is `{mode}` or `{mode, value}`. `mode:"auto"`
means "inherit the global default / let the user pick each time" and carries NO
`value`. `mode:"fixed"` locks the assistant to `value` (the user can't change it
while using this assistant). Send only the entries you want to change; read the
assistant first to keep the others.

| Entry | `fixed` → `value` is | Example |
| --- | --- | --- |
| `model` | a model name (string) | `{"mode":"fixed","value":"gemini-2.5-pro"}` |
| `permission` | a permission mode (string): `plan`, `default`, `acceptEdits`, `bypassPermissions` (yolo), `dontAsk` | `{"mode":"fixed","value":"plan"}` |
| `skills` | skill names (string[]) | `{"mode":"fixed","value":["aionui-config"]}` |
| `mcps` | MCP server names (string[]) | `{"mode":"fixed","value":["filesystem"]}` |

```bash
python3 scripts/aionui_api.py put /api/assistants/<id> '{
  "id": "<id>",
  "defaults": {
    "model":      {"mode": "fixed", "value": "gemini-2.5-pro"},
    "permission": {"mode": "fixed", "value": "plan"},
    "skills":     {"mode": "auto"},
    "mcps":       {"mode": "fixed", "value": ["filesystem"]}
  }
}'
```

> Verified end-to-end: the backend stores all four entries verbatim and returns
> them on the `?locale=` detail read. A brand-new assistant has `defaults: null`
> until you set them.

### Update

`PUT /api/assistants/<id>` with `{"id": "<id>", ...fields to change}`. Send only
the fields you want to change.

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

### Avatar

The `avatar` field accepts an emoji (`"📋"`), an image URL, a `data:` URI, or an
absolute local path. A self-contained inline SVG `data:` URI is a good default —
no external dependency, renders offline:

```bash
python3 scripts/aionui_api.py put /api/assistants/<id> '{"id":"<id>","avatar":"data:image/svg+xml;base64,<...>"}'
```

### Enable / disable / reorder

`PATCH /api/assistants/<id>/state` with `enabled` and/or `sort_order`. Disabling
hides the assistant from the homepage and team picker without deleting it.

### Delete

`DELETE /api/assistants/<id>` — only `source: user` assistants can be deleted.
Builtins can only be disabled.

---

## Skills

A skill is a folder containing a `SKILL.md` (YAML frontmatter `name` +
`description`, then instruction body). The `description` decides when the agent
auto-triggers the skill, so write it carefully.

Three sources: `builtin` (`~/.aionui/builtin-skills/`), `custom`
(`~/.aionui/skills/`), `extension` (external, symlinked).

### List the registry

```bash
python3 scripts/aionui_api.py get /api/skills
# Where skills live:
python3 scripts/aionui_api.py get /api/skills/paths
```

### Import a skill into the registry

`POST /api/skills/import` copies a skill folder into the user skills dir and
registers it. It also accepts a parent folder containing multiple skills or a
zip package.

```bash
python3 scripts/aionui_api.py post /api/skills/import '{"skill_path":"/abs/path/to/skill-folder"}'
```

> Caution: importing from a path that is ALREADY inside the user skills dir can
> race with the copy step. When editing an installed skill, edit the files in
> place, then re-import from a separate staging copy — or just verify the
> SKILL.md is non-empty afterwards. An empty SKILL.md unregisters the skill.

### Attach a skill to an assistant

Put the skill's `name` into the assistant's `enabled_skills`:

```bash
python3 scripts/aionui_api.py put /api/assistants/<id> '{"id":"<id>","enabled_skills":["skill-a","skill-b"]}'
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
| `sse` | `url` | remote Server-Sent-Events servers |
| `http` / `streamable_http` | `url` | remote HTTP servers |

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

### Toggle / update / delete

```bash
python3 scripts/aionui_api.py post   /api/mcp/servers/<id>/toggle   # enable <-> disable
python3 scripts/aionui_api.py put    /api/mcp/servers/<id> '{"description":"..."}'
python3 scripts/aionui_api.py delete /api/mcp/servers/<id>
```

> Remote servers may need OAuth: `/api/mcp/oauth/check-status`,
> `/api/mcp/oauth/login`, `/api/mcp/oauth/logout`. Only touch these if a
> `test-connection` came back with `needs_auth`.

---

## LLM providers (models & API keys)

This is where the actual models come from. A **provider** is one upstream
(Gemini, an OpenAI-compatible endpoint, Anthropic, Bedrock, …) holding a
`base_url`, an `api_key`, and the list of `models` it exposes. Assistants then
pick a model from an enabled provider. The whole lifecycle is verified
end-to-end (list / create / fetch-models / detect-protocol / update / delete).

> **Secret-handling rule:** `GET /api/providers` returns every `api_key` in
> **plaintext**. Never paste a provider response into chat, a commit, a log, or
> a memory file. When you must show the user a provider, redact the key
> (`sk-…last4`). Treat keys the user gives you the same way.

### List / inspect

```bash
python3 scripts/aionui_api.py get /api/providers
```

Each provider: `id`, `platform`, `name`, `base_url`, `api_key`, `models`
(string[]), `enabled`, `capabilities`, `is_full_url`.

`platform` is one of: `gemini`, `anthropic`, `bedrock`, `custom`. **Any
OpenAI-compatible endpoint (OpenAI, DeepSeek, OpenRouter, Ollama, vLLM, …) uses
`custom`** with its `base_url` — there is no per-vendor platform for those.

### Detect the protocol before creating (recommended)

Given a `base_url` + `api_key`, the backend probes the endpoint and tells you
which protocol it speaks and what models it has. Use this to fill `platform` and
`models` correctly instead of guessing.

```bash
python3 scripts/aionui_api.py post /api/providers/detect-protocol '{
  "platform": "custom",
  "base_url": "https://api.deepseek.com/v1",
  "api_key": "sk-..."
}'
# -> {"protocol":"openai","confidence":90,"models":[...]}
```

`fetch-models` (`POST /api/providers/fetch-models`, same body) returns just the
model list for a not-yet-saved endpoint.

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
- `GET /api/settings/client` / `PUT /api/settings/client` — the larger UI/runtime
  key-value store: `language`, `theme.activeId` (`light`/`dark`/custom),
  `ui.zoomFactor`, `ui.fontSize.{chat,markdown,code}`, `webui.desktop.allowRemote`, …

`PUT /api/settings/client` is a **partial merge** — send only the keys you want
to change. Read first, change one key, read back.

```bash
python3 scripts/aionui_api.py get /api/settings/client
python3 scripts/aionui_api.py put /api/settings/client '{"ui.zoomFactor": 1.0}'
```

> To set which model a given assistant uses, configure that assistant's
> `defaults.model` (see *Per-assistant defaults*) — not a global setting.

---

## Engines (agents)

`GET /api/agents` lists the available engines (`aionrs`, `claude`, `codex`, …)
with `enabled` / `available` flags and capabilities — useful before setting an
assistant's `preset_agent_type`, to confirm that engine is installed and
reachable. `POST /api/agents/refresh` re-scans custom agents.

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
8. Tell the user to refresh / reopen the AionUi view to see changes.

## Out of scope (handled elsewhere)

Some backend areas have `/api/*` endpoints but are intentionally NOT this
skill's job — they already have dedicated tooling, so don't reach for the raw
API here:

- **Teams** (`/api/teams/*`) — use the team MCP tools (`aion_create_team`,
  `team_spawn_agent`, `team_send_message`, …), not raw calls.
- **Cron / scheduled jobs** (`/api/cron/*`) — created and managed through their
  own flow (scheduling tools / the AionUi cron UI), not this skill.

This skill stays focused on *configuration*: assistants, skills, MCP servers,
LLM providers, and app settings.

## Not yet covered

Conversation repair (recovering a broken session from its logs) and
channel/extension management (`/api/channel/*`, `/api/extensions/*`) have
endpoints in the backend but are not verified in this skill yet. Add them once
their request bodies are confirmed against the live backend — don't guess.
