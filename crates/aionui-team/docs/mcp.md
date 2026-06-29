# Team MCP 通信

> Team 模块对外（对 agent 进程）的"系统调用"层。Agent 通过 MCP 调工具，是它操作 team 的唯一入口。HTTP 层只做 CRUD，不管 agent 之间怎么通信。

---

## 1. Team MCP：一眼看懂

Team MCP 是每个 team session 一个的内部服务，只注入给团内 agent（lead / teammate）。它负责团内协作：发消息、任务板、spawn teammate、rename、shutdown 等。

Team 创建是显式行为：由 Team UI 或 `/api/teams` REST API 发起。普通单聊 agent 不再挂载全局建团 MCP，也不再把已有 conversation 升级为 Team。

---

## 2. 工具清单

### 2.1 Team 内部 MCP

| # | 工具 | 后端已实现 | 权限 | 作用 |
|---|------|:---:|------|------|
| 1 | `team_send_message` | ✅ | 任意 agent | 给某个 slot_id 发消息；`to="*"` 广播（排除自己） |
| 2 | `team_spawn_agent` | ⚠️ 空壳 | Lead only | 动态拉起新 teammate（backend 白名单 `claude / codex`） |
| 3 | `team_task_create` | ✅ | 任意 agent | 新建任务 |
| 4 | `team_task_update` | ✅ | 任意 agent | 改状态 / owner / 依赖 |
| 5 | `team_task_list` | ✅ | 任意 agent | 列所有任务 |
| 6 | `team_members` | ✅ | 任意 agent | 列当前成员+状态 |
| 7 | `team_rename_agent` | ✅ | 任意 agent | 改 slot 显示名 |
| 8 | `team_shutdown_agent` | ✅ | Lead only | 请求某 teammate 下线（写 `shutdown_request` 到对方 mailbox） |
| 9 | `team_describe_assistant` | ❌ | — | 描述某个自定义 assistant 的能力/限制（供 spawn 时参考）|
| 10 | `team_list_models` | ❌ | — | 团内模型列表工具 |

后端缺 9、10。9 和 10 主要服务于 lead 做 spawn 决策，缺失会让 lead 只能按硬编码白名单 `["claude","codex"]` 盲选。

---

## 3. TCP 协议

每个 TeamSession 启动时开一个 TCP listener：

```
127.0.0.1:<随机端口>   (由 OS 分配)
```

### 3.1 帧格式

`[ 4 字节 big-endian 长度 | JSON 负载 ]`

- 长度上限 10 MiB，超过直接 `InvalidData`
- 负载必须是合法 JSON-RPC 2.0 请求或响应
- 参见 `crates/aionui-team/src/mcp/protocol.rs` 的 `read_frame` / `write_frame`

### 3.2 JSON-RPC 2.0

请求：
```json
{ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {...} }
```

响应成功：
```json
{ "jsonrpc": "2.0", "id": 1, "result": {...} }
```

响应错误：
```json
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "..." } }
```

### 3.3 鉴权握手

**第一条请求必须是 `initialize`**，在此之前其它 method 全返回 `INVALID_REQUEST (-32600)`。

请求：
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "auth_token": "<session-generated uuid>",
    "slot_id": "slot-xxx"
  }
}
```

响应：
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2024-11-05",
    "serverInfo": { "name": "aionui-team-mcp", "version": "1.0.0" },
    "capabilities": { "tools": {} }
  }
}
```

`auth_token` 校验失败 → `INVALID_REQUEST`；`slot_id` 被记住，后续 tool 调用以此身份鉴权（比如判断是不是 Lead）。

### 3.4 标准 method

| method | 说明 |
|--------|------|
| `initialize` | 见上 |
| `notifications/initialized` | 客户端无 id 通知，server 回空 result |
| `tools/list` | 返回 `{ "tools": [ToolDescriptor, ...] }` |
| `tools/call` | `params = { "name": "...", "arguments": {...} }`，返回 `{ "content": [{"type":"text","text":"..."}], "isError"?: true }` |

未知 method → `METHOD_NOT_FOUND (-32601)`。
Tool 业务错误 **不走** JSON-RPC error，而是返回 `result.isError=true` + 文本内容（MCP 惯例）。

---

## 4. Agent MCP 注入机制

后端（aionui-backend）负责 agent 进程的完整生命周期：启动 agent、注入 MCP 连接配置、管理 stdio bridge。这是前后端分离架构下后端的职责。

### 4.1 后端两套 MCP 注入机制

后端有两套独立的 MCP 注入机制，team MCP 走第二种：

| | 全局 MCP | 运行时 MCP（team 用这个） |
|---|---|---|
| 来源 | 用户在设置里配的 MCP servers（DB `mcp_servers` 表） | team session 启动时动态生成 |
| 注入时机 | agent 首次创建 session 时（`session/new`） | team session 启动后，写入 conversation extra 或追加到 agent 启动参数 |
| 管理模块 | `aionui-mcp` crate（`build_session_mcp_servers()`） | `aionui-team` crate（`TeamMcpStdioConfig`） |
| 生命周期 | 跟用户配置走，持久化 | 跟 team session 走，session 停止即失效 |
| 类型 | stdio / http / sse（取决于 ACP backend 能力） | 仅 stdio（TCP bridge） |

AionUi 参考实现中，两套在 agent 建 session 时合并注入（`userServers + presetServers + teamServer`）。后端需要在 `ConversationService.send_message` → agent factory → `session/new` 路径上做同样的合并。

### 4.2 Team MCP 注入流程

```
ensure_session(team_id)
    │
    ├─ 1. 启 TCP MCP server → 拿到 port + auth_token
    │
    ├─ 2. 对每个 agent:
    │      ├─ mcp_stdio_config(slot_id) → 生成 per-agent 配置
    │      ├─ conversation.extra 写入 teamMcpStdioConfig
    │      ├─ task_manager.kill(conv_id)                    ← 杀旧 agent 进程
    │      └─ task_manager.get_or_build_task(conv_id, opts) ← 重建，读到新 extra
    │
    └─ 3. 新 agent 进程 session/new 时带 mcpServers → team_* 工具可用
```

### 4.3 Agent 进程重启机制（MCP 动态注入的关键）

MCP 工具列表在 agent `session/new` 时锁定，无法热插拔。team session 是动态创建的（建团或 ensure_session 时才知道 TCP port/token）。因此需要**重启 agent 进程**来注入新的 MCP 配置。

**核心原则**：conversation 不变 + agent 进程重启 + session resume。

```
conversation_id = "conv_123"     ← 不变，消息历史完整保留
agent 旧进程 (session_id=abc)    ← kill
agent 新进程 (session_id=def)    ← rebuild，带 team MCP config
                                    └─ resume 旧 session 上下文（如果 backend 支持）
```

**复用现有 API，零侵入其他业务**：

| 操作 | 现有 API | 位置 |
|------|----------|------|
| 杀旧进程 | `IWorkerTaskManager::kill(conv_id, reason)` | `crates/aionui-ai-agent/src/task_manager.rs` |
| 重建进程 | `IWorkerTaskManager::get_or_build_task(conv_id, opts)` | 同上 |

不需要给 `IWorkerTaskManager` 加新方法（如 `skipCache`）。`kill` 从 DashMap 移除 + 杀进程，`get_or_build_task` 发现不存在就用 factory 重建——两步组合即 restart。

**session resume**（agent 侧）：
- Claude/CodeBuddy：`session/new` + `_meta.claudeCode.options.resume: true` + 旧 `sessionId`
- Codex：`session/load` + 旧 `sessionId`
- 其他：`session/new` + `resumeSessionId`

resume 逻辑已在 `AcpAgentManager::session_resume_and_send`（`acp_agent.rs`）实现。rebuild 后的第一条 `send_message` 自动走 resume 路径——**不需要 team 做额外处理**。

**边界隔离**：
- `kill` + `get_or_build_task` 是通用 agent 生命周期 API，单聊也在用
- team 只是调用方之一，不修改这两个方法的任何逻辑
- 普通单聊不会触发这个组合——只有 team session 启动时才会对 team agent 执行 kill+rebuild
- team agent 的 conversation 通过 `extra.teamId` 标识，与单聊 conversation 天然隔离

### 4.4 ACP 注入链路（stdio 注入方式）

team MCP 通过 ACP 标准的 `session/new` → `mcpServers` 声明注入，ACP CLI 自己 spawn bridge 子进程。后端不直接管 bridge 进程生命周期。

**完整数据流**：

```
conversation.extra (DB)
    │  写入 teamMcpStdioConfig: { port, token, slot_id }
    │
    ▼
build_task_options()                         ← conversation service 已有
    │  extra 原样传入 BuildTaskOptions.extra
    ▼
AcpBuildExtra 反序列化                        ← 需要加字段
    │  team_mcp_stdio_config: Option<TeamMcpStdioConfig>
    ▼
session_new() 构造 payload                    ← 需要改
    │  if config.team_mcp_stdio_config.is_some():
    │    payload["data"]["mcpServers"] = [{
    │      "type": "stdio",
    │      "name": "aionui-team",
    │      "command": "aionui-backend",
    │      "args": ["mcp-bridge"],
    │      "env": [
    │        { "name": "TEAM_MCP_PORT", "value": "<port>" },
    │        { "name": "TEAM_MCP_TOKEN", "value": "<token>" },
    │        { "name": "TEAM_AGENT_SLOT_ID", "value": "<slot_id>" }
    │      ]
    │    }, ...全局 MCP servers...]
    ▼
ACP CLI (claude / codex / ...) 收到 session/new
    │  读 mcpServers → spawn stdio bridge 子进程
    ▼
aionui-backend mcp-bridge (子进程)
    │  stdin/stdout ↔ ACP CLI (JSON-RPC 2.0)
    │  TCP ↔ TeamMcpServer (127.0.0.1:<port>)
    ▼
agent 可调 team_* 工具
```

**需要改动的文件**（最小侵入）：

| 文件 | 改动 | 影响范围 |
|------|------|----------|
| `aionui-ai-agent/src/types.rs` | `AcpBuildExtra` 加 `#[serde(default)] team_mcp_stdio_config: Option<TeamMcpStdioConfig>` | 旧 extra 无此字段 → `None`，零影响 |
| `aionui-ai-agent/src/acp_agent.rs :: session_new()` | 如果 `team_mcp_stdio_config.is_some()`，构造 `mcpServers` 数组追加到 payload | 单聊 config 为 `None` → 不追加，零影响 |
| `aionui-mcp/src/session_injection.rs` | team MCP 作为额外一项 append 到 `build_session_mcp_servers()` 结果里（或在 `session_new` 里直接 merge） | 视实现选择 |

**稳定性保证**：

1. **`Option` + `#[serde(default)]`**：旧 conversation extra 缺这个字段 → 反序列化为 `None` → 不注入 → 单聊完全不受影响
2. **`mcpServers` 是 ACP 标准字段**：ACP CLI 本来就支持（claude / codex 都有 `mcpCapabilities.stdio`），不是自定义协议
3. **bridge 崩溃不影响 agent**：ACP CLI 对 MCP server stdio pipe broken 有容错，tool 调用返回 error，agent 继续工作只是 team_* 工具不可用
4. **全局 MCP 和 team MCP 合并**：`session/new` 的 `mcpServers` 是数组，全局 MCP servers（用户设置里的）和 team MCP server 并列放入即可，互不干扰

### 4.5 stdio config 结构

`crates/aionui-team/src/mcp/bridge.rs`：

```rust
pub struct TeamMcpStdioConfig {
    pub port: u16,
    pub token: String,
    pub slot_id: String,
}
```

### 4.6 stdio bridge 方案：打进主二进制

stdio bridge 作为 `aionui-backend` 的 subcommand 实现，不单独出二进制：

```
aionui-backend mcp-bridge
```

agent CLI spawn 时的 MCP server 配置：
```json
{
  "name": "aionui-team",
  "command": "aionui-backend",
  "args": ["mcp-bridge"],
  "env": [
    { "name": "TEAM_MCP_PORT", "value": "<port>" },
    { "name": "TEAM_MCP_TOKEN", "value": "<token>" },
    { "name": "TEAM_AGENT_SLOT_ID", "value": "<slot_id>" }
  ]
}
```

bridge 职责（代码量极小）：
1. 从 env 读 `TEAM_MCP_PORT` / `TEAM_MCP_TOKEN` / `TEAM_AGENT_SLOT_ID`
2. stdin/stdout 端：接 agent CLI 的 MCP client（JSON-RPC 2.0 over stdio）
3. TCP 端：连 `127.0.0.1:<port>`，帧格式 4 字节大端长度 + JSON
4. 启动后发 `mcp_ready` 通知给 TCP server
5. 双向转发 tool call 请求/响应

⚠️ **尚未实现**，当前只有 `TeamMcpStdioConfig` 数据结构。这是 MCP 通信链路打通的前提。

---

## 5. 后端 vs AionUi GAP 分析

| # | 能力 | AionUi | 后端 | 备注 |
|---|------|:---:|:---:|------|
| 1 | Team 内 MCP TCP server | ✅ | ✅ | `TeamMcpServer` 已就绪 |
| 2 | JSON-RPC 2.0 + initialize 鉴权 | ✅ | ✅ | 协议一致 |
| 3 | `team_send_message` | ✅ | ✅ | 行为一致 |
| 4 | `team_task_*` 4 件套 | ✅ | ✅ | 四个全齐 |
| 5 | `team_members` | ✅ | ✅ | |
| 6 | `team_rename_agent` | ✅ | ✅ | |
| 7 | `team_shutdown_agent` | ✅ | ✅ | |
| 8 | `team_spawn_agent` 工具声明 | ✅ | ✅ | 工具能调 |
| 9 | `team_spawn_agent` 真实执行 | ✅ | ⚠️ **空壳** | `SpawnAgent` action 只打 log，不创 agent |
| 10 | `team_describe_assistant` | ✅ | ❌ | 缺 |
| 11 | `team_list_models` | ✅ | ❌ | 缺 |
| 12 | stdio bridge 二进制 | ✅ | ❌ | 只有数据结构 |
| 13 | MCP 写 mailbox 后主动 wake | ✅ | ❌ | 导致 agent-to-agent 消息可能悬停（见 internals.md bug #2）|

### 5.1 GAP 影响面

- Team 创建必须显式走 Team UI 或 `POST /api/teams`。
- **#9, #12**：后端 MCP server 跑着但没人连得上；且 lead 调 `team_spawn_agent` 无效
- **#13**：teammate 发消息给 lead，lead 可能长时间不醒（除非 teammate 自己触发 `IdleNotification`）

---

## 6. 数据流

### 6.1 Agent 调 MCP 工具的完整路径

```
Agent CLI (claude / codex / ...)
     │   stdin/stdout (MCP JSON-RPC)
     ▼
stdio bridge binary  ⚠️ 未实现
     │   env: TEAM_MCP_PORT / TEAM_MCP_TOKEN / TEAM_AGENT_SLOT_ID
     │   TCP + length-prefixed JSON-RPC
     ▼
TeamMcpServer (accept_loop)
     │   handle_connection: initialize → authenticated
     │   tools/call → dispatch_tool
     ▼
TeammateManager (scheduler)
     │
     ├── team_send_message      → Mailbox.write()           [⚠️ 不 wake]
     ├── team_task_create       → TaskBoard.create_task()
     ├── team_task_update       → TaskBoard.update_task()
     ├── team_task_list         → TaskBoard.list_tasks()
     ├── team_members           → slots 内存读取
     ├── team_spawn_agent       → log-only (空壳)
     ├── team_shutdown_agent    → Mailbox.write(ShutdownRequest)
     └── team_rename_agent      → slots 改名 + 广播 WS 事件
```

### 6.2 与 mailbox / taskboard 的关系

MCP 是 **写入者**，不负责后续调度：

```
[team_send_message tool call]
        │
        ▼
Mailbox (SQLite: mailbox 表)     ← MCP 负责写到这里为止
        │
        │  ⚠️ 无 wake_and_dispatch 调用
        ▼
Scheduler.try_wake(target) ──── 目前只有单聊 API 触发时才会调
```

参考单聊路径（`conversation send_message` → `TeamSession.wake_and_dispatch`）的做法，MCP 写完 mailbox 后应主动 wake 对端。现在缺这一步。

---

## 7. 关键代码索引

| 内容 | 位置 |
|------|------|
| TCP server 启停 | `crates/aionui-team/src/mcp/server.rs :: TeamMcpServer` |
| 握手 + 鉴权 | `crates/aionui-team/src/mcp/server.rs :: handle_initialize` |
| Tool dispatch | `crates/aionui-team/src/mcp/server.rs :: dispatch_tool` |
| Tool descriptor（tools/list 返回） | `crates/aionui-team/src/mcp/tools.rs :: all_tool_descriptors` |
| Backend 白名单 | `crates/aionui-team/src/mcp/tools.rs :: SPAWN_BACKEND_WHITELIST` |
| 帧 + JSON-RPC 类型 | `crates/aionui-team/src/mcp/protocol.rs` |
| Stdio config 结构 | `crates/aionui-team/src/mcp/bridge.rs :: TeamMcpStdioConfig` |
| Session 与 server 生命周期绑定 | `crates/aionui-team/src/session.rs :: TeamSession::start` |
| Scheduler 对 MCP action 的执行 | `crates/aionui-team/src/scheduler.rs :: execute_action` |
| `SpawnAgent` 空壳位置 | `crates/aionui-team/src/scheduler.rs`（搜 `"spawn_agent action — requires TeamSession to complete"`）|
| Mailbox 写入 | `crates/aionui-team/src/scheduler.rs :: handle_send_message` |

---

## 8. 前端该知道什么

1. MCP 是 **后端 ↔ agent 进程** 之间的事，**浏览器前端不直接接触**。
2. 前端不需要知道 port / token / slot_id，这些只塞给 agent 启动环境。
3. 但前端要知道：
   - "单聊升级为团" 这个交互 ⚠️ 后端目前不支持，**必须用户显式走 `POST /api/teams` 建团**
   - Lead 调 `team_spawn_agent` 目前无效（后端 bug #3），前端 UI 看到 lead 说"已创建 agent"但 team 成员数不变 → 是后端 GAP，不是前端 bug
   - Agent 间收发消息 / 任务板变化 → 前端感知靠 WS 事件（`team.agent*`），未来若增加任务板事件会按 `team.task.*` 命名
