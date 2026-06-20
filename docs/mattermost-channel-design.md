# Mattermost Channel Design

## Goal

Add Mattermost as a built-in channel plugin named `mattermost`.

The plugin follows the existing `aionui-channel` architecture used by Telegram,
Lark, DingTalk, and Weixin. It is not an extension runtime plugin and uses only
the `mattermost` platform name.

## Non-Goals

- Do not implement a generic JavaScript extension channel runtime.
- Do not add a separate bot-specific plugin type.
- Do not distinguish bot tokens from user personal access tokens in the channel
  manager API. Mattermost accepts both as bearer tokens for the REST and
  WebSocket APIs when the token has sufficient permissions.
- Do not add new database tables or migrations.

## Existing Channel Pattern

Current built-in channels use the same lifecycle:

1. `PluginType` contains a fixed platform variant.
2. `plugins/mod.rs` creates a platform-specific Rust plugin from `PluginType`.
3. `ChannelManager::enable_plugin` stores encrypted config, initializes the
   plugin, starts it, and marks it as running.
4. The plugin receives platform-specific events, converts them to
   `UnifiedIncomingMessage`, and sends them through `PluginCallbacks.message_tx`.
5. `ChannelOrchestrator` dispatches the message to pairing, actions, sessions,
   and the selected agent/model configuration for that platform.
6. Replies are sent through `ChannelManager` using the plugin id string.

Mattermost should use the same lifecycle.

## Plugin Type and Feature

Add:

- `PluginType::Mattermost`
- `mattermost` feature in `crates/aionui-channel/Cargo.toml`
- `crates/aionui-channel/src/plugins/mattermost/`
- Mattermost entry in the built-in plugin status list exposed by
  `GET /api/channel/plugins`
- Mattermost support in channel settings sync validation
- Mattermost support in conversation naming and source mapping

The plugin id and serialized platform value are `mattermost`.

## Configuration

The Mattermost plugin uses the shared `PluginConfig` structure:

Credentials:

- `accessToken` or `access_token`: required bearer token

Options:

- `serverUrl` or `server_url`: required base URL, for example
  `https://mattermost.example.com`
- `allowedChannelIds` or `allowed_channel_ids`: optional comma-separated channel
  id allow-list
- `replyInThread` or `reply_in_thread`: optional boolean, default `true`
- `ignoreSelfMessages` or `ignore_self_messages`: optional boolean, default
  `true`

Sensitive values must remain inside the encrypted plugin config and must not be
included in logs or API status responses.

## Mattermost API Use

The plugin uses Mattermost API v4.

Startup and test:

- `GET /api/v4/users/me` validates the token and loads the current user id.

Incoming messages:

- Connect to `/api/v4/websocket`.
- Send an authentication message with the access token after the socket opens.
- Handle `posted` events.
- Parse the event `data.post` JSON payload.
- Ignore deleted, empty, or unsupported posts.
- Ignore self messages when `ignoreSelfMessages` is enabled.
- Apply `allowedChannelIds` when configured.

Outgoing messages:

- `POST /api/v4/posts`
- `channel_id` is the incoming chat id.
- `message` is the outgoing text.
- If `replyInThread` is enabled, set `root_id` to the incoming root id or post id
  when replying from a channel session.

Mattermost accepts markdown-like plain text in post messages. The channel
formatter should not HTML-escape normal responses for Mattermost.

## Message Mapping

Incoming Mattermost posts map to `UnifiedIncomingMessage`:

- `id`: Mattermost post id
- `platform`: `PluginType::Mattermost`
- `chat_id`: Mattermost channel id
- `user.id`: Mattermost user id
- `user.display_name`: username when present, otherwise user id
- `content.type`: `text`
- `content.text`: post message
- `timestamp`: `create_at / 1000`
- `reply_to_message_id`: root id when present
- `raw`: sanitized event/post metadata without credentials

Mattermost should map to `ConversationSource::Aionui` unless a dedicated source
variant exists in `aionui_common`. This matches reserved Slack/Discord behavior.
Conversation names should use the short prefix `mm`.

## UI Contract

AionUi should expose Mattermost as the built-in channel id `mattermost`.

The existing extension contribution can be removed or hidden once the built-in
channel is available. Agent/model settings should persist under:

- `assistant.mattermost.agent`
- `assistant.mattermost.defaultModel`

## Observability

Add low-volume structured logs for lifecycle and hard-to-observe failures:

- plugin initialized
- REST identity loaded
- WebSocket connected/authenticated
- WebSocket reconnect scheduled
- plugin stopped
- invalid/malformed inbound post handled safely

Logs must not include tokens, raw request headers, raw credentials, or full
message bodies.

## Tests

Add tests at the same level as existing channel plugins:

- `PluginType::Mattermost` parse/display/serde behavior
- factory creates Mattermost plugin behind the `mattermost` feature
- config parsing accepts camelCase and snake_case keys
- missing token/server URL fails validation
- incoming `posted` event maps to `UnifiedIncomingMessage`
- self messages and disallowed channels are ignored
- outgoing post request payload includes `channel_id`, `message`, and optional
  `root_id`

Run affected checks first:

```bash
cargo fmt --all -- --check
cargo clippy -p aionui-channel --features mattermost -- -D warnings
cargo test -p aionui-channel --features mattermost
```

Before pushing, use `just push`.

## Consistency Checklist

| Area | Existing channels | Mattermost design |
| --- | --- | --- |
| Plugin identity | Fixed `PluginType` variant | `PluginType::Mattermost` |
| Build gating | Cargo feature per platform | `mattermost` feature |
| Implementation language | Rust plugin module | Rust plugin module |
| Config storage | `PluginConfig`, encrypted credentials | Same |
| Manager lifecycle | `enable_plugin` initializes and starts | Same |
| Startup restore | `restore_plugins` by `PluginType` | Same |
| Inbound flow | Platform event to `UnifiedIncomingMessage` | Same |
| Outbound flow | `send_message` / `edit_message` | Same |
| Agent/model keys | `assistant.{platform}.*` | `assistant.mattermost.*` |
| Settings sync | `PluginType::from_str_opt(platform)` | `mattermost` parses successfully |
| Formatter | Platform-specific reply formatting | Plain text / markdown-like output |
| Logs | lifecycle and warnings without secrets | Same |
