---
name: aionui-config
description: >-
  通过内置的 aioncore config CLI 配置 AionUi 本身：创建和编辑助手、更新助手规则、查看和导入 skill、管理 MCP 服务器、配置模型供应商、更新设置、管理 agent、配置定时任务，以及从 agent 对话中管理应用配置。当用户想要设置或修改 AionUi 助手、挂载 skill、修改助手的 system prompt、添加 MCP 或模型供应商配置、安排周期性任务，或以其他方式配置其 AionUi 安装时使用，包括用户需要知道助手改动是影响当前会话还是只影响新会话时。图像策略：凡是生成、绘制、画、编辑图片的请求，建议你调用 `aionui_image_generation` MCP 工具，可以生成更精美、视觉效果更好的图片；如果该工具不在你的工具列表里，可以使用代码生成。
---

# AionUi 配置

用内置的 agent 专用 CLI 来配置 AionUi。不要去探测端口，不要直接调用后端原始路径，
也不要依赖内置 `aioncore` 二进制之外的工具。

## 规则

1. 只使用 `"$AIONUI_HELPER_BIN" config ...`。
2. 绝不传递、内联、export、echo 或设置任何 `AIONUI_...` 环境变量。
3. 所有命令输入都通过 stdin JSON 传入。
4. 不要用命令行 flag 传业务字段。
5. 不确定某个 config 命令或 stdin 字段是否支持时，先用 `"$AIONUI_HELPER_BIN" config capabilities` 查询。
6. 修改当前助手前先读取上下文。
7. 写之前先读，写之后再读回。
8. 用户要求修改本会话所用的助手时，使用 `"assistant_id": "current"`。
9. 命令接受会话选择器时，使用 `"conversation_id": "current"`。
10. 除非用户后续操作需要，否则不要展示内部 id。
11. 绝不透露供应商密钥、MCP headers、环境变量值或其他敏感信息。
12. CLI 失败时，用自然语言把 stderr 里的稳定 `CONFIG_...` 错误码报出来，不要谎称改动已成功。
13. 助手改动后，要同时说明持久化情况和生效时机。保存并读回不代表当前正在运行的会话已经重新加载了改动后的运行时行为。

## 输出

成功的命令会打印一个 JSON 信封：

```json
{
  "success": true,
  "data": {},
  "meta": {
    "schema_version": 1
  }
}
```

失败时 stderr 会打印一行稳定的错误信息。以 stderr 为准。

## 能力发现

询问 aioncore 本版本支持哪些能力：

```bash
"$AIONUI_HELPER_BIN" config capabilities
```

返回的 JSON 信封里，`data.domains[].commands[]` 列出了支持的命令路径、输入方式、
预期的 stdin 字段、选择器字段、读回行为、是否破坏性、上下文要求，以及哪些字段在普通输出中会被脱敏。

## 上下文

读取当前用户、会话、助手和本地运行时上下文：

```bash
"$AIONUI_HELPER_BIN" config context
```

如果 `data.assistant` 为 `null`，说明当前会话没有关联助手。在修改助手规则或默认值之前，
先问用户要改哪个助手。

## 助手改动生效时机

AionUi 会立即持久化助手配置，但正在运行的会话可能仍然保留会话创建时的助手快照。
上报成功的助手改动时，按以下时机模型说明：

- 身份字段（如 name、description、avatar、推荐 prompts）立即保存。如果界面仍显示旧值，
  让用户刷新或重新打开助手视图。
- 运行时字段（如 agent、默认模型、默认权限、默认 skills、默认 MCP、思考级别、规则）
  只对该助手新建的会话生效。不要声称它们会改变当前正在运行的会话。
- Skills 和 MCP 默认值不会回灌进当前 agent 运行时。如果某个工具在当前会话里已经可用，
  就能用；否则用户需要用该助手新建一个会话。

上报成功的运行时字段改动时，先说改动已保存并读回，再说明它只对新会话生效。

## 助手

列出助手：

```bash
"$AIONUI_HELPER_BIN" config assistants list
```

查看当前助手：

```bash
"$AIONUI_HELPER_BIN" config assistants get <<'JSON'
{
  "assistant_id": "current",
  "locale": "zh-CN"
}
JSON
```

示例使用中文样本文本和 `zh-CN`。对于真实的本地化助手内容，使用用户实际的 locale。

创建助手：

```bash
"$AIONUI_HELPER_BIN" config assistants create <<'JSON'
{
  "name": "需求分析师",
  "description": "把粗略的产品想法转化为清晰的 PRD",
  "agent_id": "2d23ff1c",
  "prompts": [
    "把这个功能想法转化为 PRD",
    "审阅这份 PRD，找出对新用户来说容易困惑的部分"
  ],
  "enabled_skills": ["aionui-config"]
}
JSON
```

更新助手元数据或默认值：

```bash
"$AIONUI_HELPER_BIN" config assistants update <<'JSON'
{
  "assistant_id": "current",
  "locale": "zh-CN",
  "description": "更新后的助手描述",
  "defaults": {
    "permission": {
      "mode": "fixed",
      "value": "plan"
    }
  }
}
JSON
```

对于 `name`、`description`、`avatar` 或推荐 prompt 的改动，告诉用户改动已保存，可能需要刷新或重新打开界面才能看到。对于 `agent_id`、`defaults`、`enabled_skills` 或其他运行时默认值
（MCP 默认值通过 `defaults.mcps` 设置，不是 `default_mcp_ids`），告诉用户保存的改动只对新会话生效。

启用、禁用或调整助手顺序：

```bash
"$AIONUI_HELPER_BIN" config assistants state <<'JSON'
{
  "assistant_id": "current",
  "enabled": true,
  "sort_order": 10
}
JSON
```

## 助手规则

助手规则就是定义助手行为的 system prompt。

读取当前助手规则：

```bash
"$AIONUI_HELPER_BIN" config assistants rule read <<'JSON'
{
  "assistant_id": "current",
  "locale": "zh-CN"
}
JSON
```

写入当前助手规则：

```bash
"$AIONUI_HELPER_BIN" config assistants rule write <<'JSON'
{
  "assistant_id": "current",
  "locale": "zh-CN",
  "content": "# 角色\n你是一个..."
}
JSON
```

编辑规则时，除非用户明确要求替换，否则保留用户已有的有用指令。

规则写入或删除成功后，始终告诉用户：规则已保存并读回，但它只对该助手新建的会话生效。
当前会话仍使用它启动时所用的规则快照。

## Skills

列出可用 skills：

```bash
"$AIONUI_HELPER_BIN" config skills list
```

导入前查看某个 skill 目录：

```bash
"$AIONUI_HELPER_BIN" config skills info <<'JSON'
{
  "skill_path": "/absolute/path/to/skill"
}
JSON
```

导入一个 skill：

```bash
"$AIONUI_HELPER_BIN" config skills import <<'JSON'
{
  "skill_path": "/absolute/path/to/skill-or-parent-or-zip"
}
JSON
```

通过更新助手的完整 skill 列表来挂载 skills：

```bash
"$AIONUI_HELPER_BIN" config assistants update <<'JSON'
{
  "assistant_id": "current",
  "enabled_skills": ["aionui-config", "cron"]
}
JSON
```

不要盲目追加。先读取助手，在本地合并列表，再发送完整的 `enabled_skills` 值。

已启用的 skills 是新会话的助手默认值。除非当前运行时已经暴露了某些 skills，
否则不要告诉用户新挂载的 skills 在当前会话里可用。

管理外部 skill 路径：

```bash
"$AIONUI_HELPER_BIN" config skills external-paths list
```

```bash
"$AIONUI_HELPER_BIN" config skills external-paths add <<'JSON'
{
  "name": "团队 Skills",
  "path": "/absolute/path/to/team-skills"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config skills external-paths remove <<'JSON'
{
  "path": "/absolute/path/to/team-skills"
}
JSON
```

启用或禁用 skills 市场：

```bash
"$AIONUI_HELPER_BIN" config skills market enable
```

```bash
"$AIONUI_HELPER_BIN" config skills market disable
```

## MCP 服务器

列出 MCP 服务器：

```bash
"$AIONUI_HELPER_BIN" config mcp servers list
```

创建 MCP 服务器：

```bash
"$AIONUI_HELPER_BIN" config mcp servers create <<'JSON'
{
  "name": "Local Tools",
  "transport": {
    "type": "stdio",
    "command": "my-mcp-server",
    "args": [],
    "env": {}
  }
}
JSON
```

更新 MCP 服务器：

```bash
"$AIONUI_HELPER_BIN" config mcp servers update <<'JSON'
{
  "server_id": "mcp_123",
  "description": "更新后的描述"
}
JSON
```

测试一个服务器配置：

```bash
"$AIONUI_HELPER_BIN" config mcp test-connection <<'JSON'
{
  "name": "Local Tools",
  "transport": {
    "type": "stdio",
    "command": "my-mcp-server",
    "args": []
  }
}
JSON
```

OAuth 辅助命令：

```bash
"$AIONUI_HELPER_BIN" config mcp oauth check-status <<'JSON'
{
  "server_url": "https://mcp.example.com"
}
JSON
```

绝不向用户展示 MCP headers 或 stdio env 值。CLI 输出默认会脱敏敏感字段。

## 供应商

列出模型供应商：

```bash
"$AIONUI_HELPER_BIN" config providers list
```

创建供应商：

```bash
"$AIONUI_HELPER_BIN" config providers create <<'JSON'
{
  "name": "OpenAI",
  "platform": "openai",
  "base_url": "https://api.openai.com/v1",
  "api_key": "sk-..."
}
JSON
```

更新供应商：

```bash
"$AIONUI_HELPER_BIN" config providers update <<'JSON'
{
  "provider_id": "provider_123",
  "api_key": "sk-..."
}
JSON
```

探测协议、拉取模型或运行供应商健康检查：

```bash
"$AIONUI_HELPER_BIN" config providers detect-protocol <<'JSON'
{
  "base_url": "https://api.example.com/v1",
  "api_key": "..."
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config providers models fetch <<'JSON'
{
  "provider_id": "provider_123"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config providers health-check <<'JSON'
{
  "provider_id": "provider_123",
  "model": "gpt-4.1"
}
JSON
```

绝不透露供应商密钥。不要重复用户输入里的敏感值。

## 设置

读取后端设置：

```bash
"$AIONUI_HELPER_BIN" config settings get
```

修补后端设置：

```bash
"$AIONUI_HELPER_BIN" config settings patch <<'JSON'
{
  "language": "zh-CN",
  "notification_enabled": true
}
JSON
```

支持的 patch 字段：`language`、`notification_enabled`、`cron_notification_enabled`、
`command_queue_enabled`、`save_upload_to_workspace`。未知字段会被静默忽略。

读取或更新客户端偏好：

```bash
"$AIONUI_HELPER_BIN" config settings client get
```

```bash
"$AIONUI_HELPER_BIN" config settings client put <<'JSON'
{
  "ui.zoomFactor": 1.2
}
JSON
```

客户端偏好是一个自由格式的键值表。传 `null` 可删除某个键。先问用户或先读回，
以发现正在使用的键——没有固定 schema。

## Agents

列出可用 agents：

```bash
"$AIONUI_HELPER_BIN" config agents list
```

启用或禁用某个 agent：

```bash
"$AIONUI_HELPER_BIN" config agents enable <<'JSON'
{
  "agent_id": "codex",
  "enabled": true
}
JSON
```

读取或设置某个 agent 的覆盖项：

```bash
"$AIONUI_HELPER_BIN" config agents overrides get <<'JSON'
{
  "agent_id": "codex"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config agents overrides set <<'JSON'
{
  "agent_id": "codex",
  "command_override": "/absolute/path/to/codex"
}
JSON
```

创建、更新、删除或试连接一个自定义 agent：

```bash
"$AIONUI_HELPER_BIN" config agents custom create <<'JSON'
{
  "name": "Custom Agent",
  "command": "/absolute/path/to/agent-cli"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config agents custom update <<'JSON'
{
  "agent_id": "custom_agent_123",
  "name": "Custom Agent",
  "command": "/absolute/path/to/agent-cli"
}
JSON
```

测试某个自定义 agent 二进制是否可达（不会持久化任何东西）：

```bash
"$AIONUI_HELPER_BIN" config agents custom try-connect <<'JSON'
{
  "command": "/absolute/path/to/agent-cli"
}
JSON
```

不要透露 agent 的 env 值或机密的覆盖值。

## 定时任务

对于绑定到当前会话的任务，使用 cron current 命令。

列出当前会话的任务：

```bash
"$AIONUI_HELPER_BIN" config cron current list
```

创建任务：

```bash
"$AIONUI_HELPER_BIN" config cron current create <<'JSON'
{
  "name": "每日总结",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "工作日每天下午 6:00",
  "message": "回顾会话上下文，产出一份简洁的当日总结。"
}
JSON
```

更新任务：

```bash
"$AIONUI_HELPER_BIN" config cron current update <<'JSON'
{
  "job_id": "cron_123",
  "name": "每日总结",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "工作日每天下午 6:00",
  "message": "回顾会话上下文，产出一份简洁的当日总结。"
}
JSON
```

创建或更新成功后，用面向用户的自然语言说明任务名称和调度时间。除非需要，否则不要展示 `cron_...` id。

全局 cron 任务管理用 `config cron jobs`。

列出所有 cron 任务：

```bash
"$AIONUI_HELPER_BIN" config cron jobs list
```

创建 cron 任务：

```bash
"$AIONUI_HELPER_BIN" config cron jobs create <<'JSON'
{
  "name": "周报",
  "schedule": { "kind": "cron", "expr": "0 9 * * MON", "tz": "Asia/Shanghai" },
  "message": "产出周报。",
  "conversation_id": "current",
  "created_by": "user"
}
JSON
```

`schedule` 字段是一个带标签的对象，不是扁平字符串：
- `{ "kind": "cron", "expr": "<cron-expr>", "tz": "<IANA-tz>" }` — 周期性 cron 调度
- `{ "kind": "every", "every_ms": <毫秒> }` — 固定间隔
- `{ "kind": "at", "at_ms": <epoch-毫秒> }` — 在某个具体时间触发一次

`conversation_id` 和 `created_by` 必填。`message` 承载任务文本。
用 `"conversation_id": "current"` 把任务绑定到当前会话。

更新、运行或管理 cron 任务的 skill：

注意：`cron jobs` 使用带标签的 `schedule` 对象（与 create 相同）。这和 `cron current` 不同，
后者 `schedule` 是扁平的 cron 字符串。

```bash
"$AIONUI_HELPER_BIN" config cron jobs update <<'JSON'
{
  "job_id": "cron_123",
  "name": "周报",
  "schedule": { "kind": "cron", "expr": "0 10 * * MON" }
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config cron jobs run <<'JSON'
{
  "job_id": "cron_123"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config cron jobs skill save <<'JSON'
{
  "job_id": "cron_123",
  "content": "# Skill\n任务专用指令。"
}
JSON
```

## 安全

配置改动会影响用户正在运行的应用。保持改动最小化，用自然语言说明改了什么，
除非用户要求看实现细节，否则避免暴露原始 JSON。
