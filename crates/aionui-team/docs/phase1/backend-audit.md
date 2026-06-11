# 后端 Team 模块 GAP 调研（rebase 后最新代码）

> **调研范围**：仓库 `/Users/zhuqingyu/.superset/worktrees/aionui-backend/repeated-algebra/`，分支 `docs/api-for-frontend`，HEAD `21abc46`。
> **调研对象**：只看后端；只考虑 **ACP agent type**（Gemini 走 ACP 路径，aionrs 是 stub，不涉及）。
> **目标**：让 backend 包掉所有 team 逻辑，上层调用方（参考 AionUi 实现）只管渲染。
>
> 相关已有文档（本文档的断言若与其冲突，以本文档和源码为准）：
> - [`docs/teams/phase1/aionui-audit.md`](./aionui-audit.md) — **AionUi 侧权威事实来源**（搭档 aionui-audit 交付，锚定 AionUi main @ `ed8a6bcd3`）
> - [`docs/teams/README.md`](../README.md) — 模块总览和 ASCII 架构图
> - [`docs/teams/api.md`](../api.md) — HTTP API 清单
> - [`docs/teams/internals.md`](../internals.md) — scheduler 状态机与时序图
> - [`docs/teams/mcp.md`](../mcp.md) — MCP 协议说明
> - [`docs/teams/team-prompts.md`](../team-prompts.md) — prompt 模板
> - [`docs/teams/frontend-guide.md`](../frontend-guide.md) — 调用方对接指南（AionUi 参考实现）

> **版本说明**：本文档第 3–9 节已基于 aionui-audit.md（AionUi commit `ed8a6bcd3`）对齐；之前凭推断得出的结论已按源码事实修正。所有 "AionUi 有 ✅" 的列都能在 aionui-audit.md 里锚定到行号。

---

## 0. 快速结论

| 领域 | 结论 |
|------|------|
| **核心数据面**（HTTP CRUD、邮箱、任务板、MCP TCP server、scheduler 工具执行） | 已完整实现且带 unit/integration test |
| **控制面 —— agent 运行时与 team 的耦合** | **完全未接通**。`TeammateManager::try_wake`、`TeamMcpStdioConfig`、`build_lead_prompt` 等关键能力均无生产调用 |
| **Team 内部 MCP 工具** | **10 个工具少了 2 个且 8 个中有 1 个是 Lead-only 错判**：现有 8 个（见第 1.5.2 节），AionUi 参考要 10 个（缺 `team_describe_assistant` / `team_list_models`）；`team_rename_agent` AionUi 允许所有角色，本后端实现也是所有角色（一致） |
| **Team Guide MCP（单聊→建团）** | **完全未实现**。AionUi 有 `aion_create_team` + `aion_list_models` 两个工具 + 独立单例 TCP server + stdio bridge + Layer-1 Prompt 注入，后端此模块不存在 |
| **ACP session_new 对 mcp_servers 的注入** | **完全没接通**。`AcpAgentManager::session_new_and_prompt` 直接 `NewSessionRequest::new(workspace)`，ACP agent 看不到任何 `team_*` 工具 |
| **AcpBuildExtra 字段** | 没有 `team_id` / `slot_id` / `team_mcp`，参数传不下去 |
| **ConversationService.send_message** | 不识别 `extra.teamId`，team conversation 的消息走不到 scheduler |
| **Prompt 三层体系** | Layer-2/3 后端有裸函数 `build_lead_prompt` / `build_teammate_prompt`，但**模板内容严重不足**（AionUi leader 有 15 步 Workflow + Model Selection + Preset Assistant + Sequencing Dependent Work；teammate 有 Standing By + Shutdown 协议原文）；Layer-1 Team Guide Prompt 完全不存在 |
| **MCP 可靠性机制** | `mcp_ready` 握手、`waitForMcpReady(30s) graceful`、`MAX_MCP_MESSAGE_SIZE=64MB`、300s 请求超时 —— **全部没有**（后端帧大小上限 10MB，无 ready 握手，无请求超时） |
| **Scheduler 可靠性机制** | `activeWakes` 重入锁、`wakeTimeouts` 60s 看门狗、`finalizedTurns` 5s dedup、crash recovery、429/inactivity watchdog、`addAgentLocks` 串行化 —— **全部没有** |
| **Agent 生命周期事件** | AionUi `team.*` WebSocket 事件集有 6 组（mcpStatus 含 10 个 phase），后端只有 4 组基础事件（status/spawned/removed/renamed），缺 `team.listChanged` / `team.mcpStatus` |
| **Role 不变式**（leader 不可 shutdown/remove） | scheduler 层**部分**：shutdown_agent 只对角色做了 Lead-only 检查（[scheduler.rs:451-455](../../../crates/aionui-team/src/scheduler.rs)），`remove_agent` 不检查 role |

**P0 gap 的本质**（对照 aionui-audit.md §7）：aionui-backend 目前停在"AionUi 架构的数据结构层"，agent 运行时、MCP 注入链、Prompt 内容、可靠性机制全部缺失。只有 team 数据面先跑通 MCP 工具注入 + 订阅 ACP Finish 事件回调 `finalize_turn`，用户才可能第一次看到 "lead 在对话里调度 teammate" 的真实行为。

**二轮对照（§3.5 节补漏）**：交叉审阅 aionui-audit §7/§8 后又发现 11 条漏项（其中 4 条 P0）：`files` 附件参数、wake 的"log-not-throw"语义、`delete_team` 级联 agent kill、工具描述长文本原样复用、`resolveLeaderAssistantLabel`/`formatMessages` helper、MCP-spawn 硬编码默认值、Guide ↔ 内部 MCP 互斥、stream chunk 全订阅。总 GAP 数 **55 条**（P0=16，P1=31，P2=8）。

---

## 1. 模块盘点（逐文件）

### 1.1 `crates/aionui-team/src/session.rs` — TeamSession

结构体（[session.rs:14-20](../../../crates/aionui-team/src/session.rs)）：

```rust
pub struct TeamSession {
    team: Team,
    scheduler: Arc<TeammateManager>,
    mailbox: Arc<Mailbox>,
    task_board: Arc<TaskBoard>,
    mcp_server: TeamMcpServer,
}
```

| 方法 | 位置 | 作用 |
|------|------|------|
| `start(team, repo, broadcaster)` | [session.rs:23](../../../crates/aionui-team/src/session.rs) | 构造 mailbox / task_board / scheduler / mcp_server，返回 session |
| `team_id()` / `scheduler()` / `mailbox()` / `task_board()` | [session.rs:57-146](../../../crates/aionui-team/src/session.rs) | getter |
| `mcp_stdio_config(slot_id) -> TeamMcpStdioConfig` | [session.rs:65](../../../crates/aionui-team/src/session.rs) | **返回一个 `{port, token, slot_id}` 结构，但从来没有被生产代码调用**（见第 3 节） |
| `send_message(content)` | [session.rs:73](../../../crates/aionui-team/src/session.rs) | 用户→lead mailbox 写一条 `Message`，并把 lead 状态置 `Working` |
| `send_message_to_agent(slot_id, content)` | [session.rs:98](../../../crates/aionui-team/src/session.rs) | 用户→指定 agent mailbox，**不验证 agent 是否在线** |
| `add_agent` / `remove_agent` / `rename_agent` | [session.rs:123-133](../../../crates/aionui-team/src/session.rs) | 转发给 scheduler |
| `stop()` | [session.rs:135](../../../crates/aionui-team/src/session.rs) | 停 mcp_server（**不 kill 对应的 ACP agent 进程**） |

> **Gap 特征**：`TeamSession` 里没有任何 agent 运行时引用（没有 `WorkerTaskManager`、没有 `AgentManagerHandle`）。它只管数据，不管 agent 进程。

### 1.2 `crates/aionui-team/src/service.rs` — TeamSessionService

构造 ([service.rs:26-37](../../../crates/aionui-team/src/service.rs))：

```rust
pub fn new(
    repo: Arc<dyn ITeamRepository>,
    conversation_service: ConversationService,
    broadcaster: Arc<dyn EventBroadcaster>,
) -> Self
```

**依赖的是 `ConversationService`（通过 `create_conversation` 造会话行），不是 `IWorkerTaskManager`。** 这意味着 service 层不负责启动 agent 运行时。

全部 public 方法：

| 方法 | 行 | 作用 |
|------|----|------|
| `create_team(user_id, CreateTeamRequest)` | 39 | 为每个 agent 建 conversation（`extra={"teamId": team_id}`，[service.rs:73](../../../crates/aionui-team/src/service.rs)），首个 agent = Lead |
| `list_teams()` / `get_team(id)` | 127, 137 | DB 读取 |
| `remove_team(user_id, team_id)` | 147 | stop_session + 删每个 conversation + 删 mailbox / tasks / team |
| `rename_team` | 172 | 改名 |
| `add_agent` / `remove_agent` / `rename_agent` | 191, 262, 307 | 增删改 agent，同步到 session（若存在） |
| `ensure_session(team_id)` | 346 | 幂等启动 TeamSession |
| `stop_session(team_id)` | 364 | 停掉 session（dashmap 删） |
| `send_message(team_id, content)` | 370 | 转发 `TeamSession::send_message` |
| `send_message_to_agent(team_id, slot_id, content)` | 378 | 同上 |
| `dispose_all()` | 391 | 停所有 session |

**关键事实**：
1. `CreateTeamRequest` 校验 `agents.is_empty()` 抛错（[service.rs:44](../../../crates/aionui-team/src/service.rs)），**但不校验 user_id 归属 —— `get_team`/`list_teams`/`remove_team` 都不做 user 过滤**（这条在 [api.md](../api.md) 有提到是 bug #5）。
2. 首个 agent 强制 `role=Lead`（`i==0`，[service.rs:56-60](../../../crates/aionui-team/src/service.rs)），忽略输入的 role 字段。
3. 每个 agent 独立 conversation，`extra.teamId = team_id`（[service.rs:73](../../../crates/aionui-team/src/service.rs) / [service.rs:218](../../../crates/aionui-team/src/service.rs)）。**这是目前唯一把 team 和 conversation 绑定的线索，但 `ConversationService::send_message` 不读它**（见第 4 节）。

### 1.3 `crates/aionui-team/src/routes.rs` — HTTP 路由

端点（[routes.rs:24-48](../../../crates/aionui-team/src/routes.rs)）：

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/teams` | 创建 |
| GET | `/api/teams` | 列表（**未按 user 过滤**） |
| GET | `/api/teams/{id}` | 详情 |
| DELETE | `/api/teams/{id}` | 删除 |
| PATCH | `/api/teams/{id}/name` | 改名 |
| POST | `/api/teams/{id}/agents` | 加 agent |
| DELETE | `/api/teams/{id}/agents/{slot_id}` | 删 agent |
| PATCH | `/api/teams/{id}/agents/{slot_id}/name` | 改 agent 名 |
| POST | `/api/teams/{id}/messages` | **发消息给 team（写 lead 邮箱）** |
| POST | `/api/teams/{id}/agents/{slot_id}/messages` | **发消息给指定 agent（写其邮箱）** |
| POST | `/api/teams/{id}/session` | 启动/确保 session |
| DELETE | `/api/teams/{id}/session` | 停 session |

**注意**：没有 `GET /tasks`、`GET /mailbox`、`GET /members` 端点。客户端要看任务板 / 邮箱只能通过 WebSocket 事件或者等 agent 在 prompt 里回显（AionUi 参考实现同样无 HTTP 拉取端点）。

### 1.4 `crates/aionui-team/src/scheduler.rs` — 核心调度

核心类型：

- `SchedulerAction` enum ([scheduler.rs:22-57](../../../crates/aionui-team/src/scheduler.rs))：`SendMessage` / `TaskCreate` / `TaskUpdate` / `SpawnAgent` / `IdleNotification` / `ShutdownAgent` / `RenameAgent`
- `WakePayload` struct ([scheduler.rs:64](../../../crates/aionui-team/src/scheduler.rs))：`{ agent, tasks, unread_messages }`
- `AgentSlot` ([scheduler.rs:75](../../../crates/aionui-team/src/scheduler.rs))：私有，`{ agent, status }`
- `TeammateManager` ([scheduler.rs:84](../../../crates/aionui-team/src/scheduler.rs))：`{ team_id, slots: Mutex<HashMap>, mailbox, task_board, events }`

`TeammateManager` 关键方法：

| 方法 | 行 | 作用 |
|------|----|------|
| `set_status(slot_id, status)` | 120 | 改状态 + `team.agentStatusChanged` WS 事件 |
| `get_status(slot_id)` / `get_agent(slot_id)` | 134, 142 | getter |
| `build_wake_payload(slot_id)` | 150 | 组装 `{agent, tasks, unread_messages}` |
| `try_wake(slot_id)` | 164 | **原子 Idle→Working，返回 payload；非 idle 返回 None** |
| `mark_idle(slot_id)` | 182 | 置 idle；若非 lead 则尝试 `maybe_wake_leader_when_all_idle` |
| `execute_action(from_slot_id, action)` | 201 | 执行单个 SchedulerAction |
| `finalize_turn(slot_id, actions)` | 281 | 批量 execute + mark_idle（若无 IdleNotification） |
| `add_agent` / `remove_agent` / `rename_agent` | 305, 324, 336 | 动态增删改 + 广播事件 |
| `list_agents()` / `list_tasks()` / `find_lead_slot_id()` | 348, 353, 357 | getter |
| `maybe_wake_leader_when_all_idle()` | 482 | 防死循环：全员 idle 且 lead idle 时返回 lead slot_id（**调用方自己负责 wake**） |

反死循环规则（tested in [scheduler.rs:701-776](../../../crates/aionui-team/src/scheduler.rs)）：
- Lead 变 idle 时：立刻 `Ok(None)`
- Non-lead 变 idle 时：全员 idle 且 lead idle → 返回 `Some(lead_slot_id)`；否则 `None`
- `try_wake` 发现非 idle → `Ok(None)`（跳过重复唤醒）

**特殊 action**：
- `SpawnAgent` ([scheduler.rs:254-266](../../../crates/aionui-team/src/scheduler.rs))：**只 log，不执行**，注释写 `spawn_agent action — requires TeamSession to complete`。实际 spawn 逻辑根本没实现。
- `ShutdownAgent` ([scheduler.rs:267](../../../crates/aionui-team/src/scheduler.rs))：Lead-only，写 `ShutdownRequest` 消息到目标邮箱，不 kill 进程。
- `IdleNotification` ([scheduler.rs:250](../../../crates/aionui-team/src/scheduler.rs))：非 lead 发送时写到 lead 邮箱；随后 `mark_idle(from_slot_id)`。
- 广播消息 (`to == "*"`)：按 `slots.keys()` 过滤自己，全部发一遍 ([scheduler.rs:375-395](../../../crates/aionui-team/src/scheduler.rs))。

`WAKE_TIMEOUT_MS` 常量 ([scheduler.rs:16](../../../crates/aionui-team/src/scheduler.rs))：`60_000` —— **定义但未使用**（没有任何定时器读它）。

### 1.5 `crates/aionui-team/src/mcp/` — TCP MCP 服务器

目录（[mcp/mod.rs](../../../crates/aionui-team/src/mcp/mod.rs)）：`bridge.rs` / `protocol.rs` / `server.rs` / `tools.rs`。

#### 1.5.1 `server.rs` — TeamMcpServer

结构 ([server.rs:26-30](../../../crates/aionui-team/src/mcp/server.rs))：`{ addr, auth_token, shutdown_tx }`。

启动流程：
1. `TcpListener::bind("127.0.0.1:0")` — **localhost 任意端口**（[server.rs:37](../../../crates/aionui-team/src/mcp/server.rs)）
2. `tokio::spawn(accept_loop)`
3. 每个 `TcpStream` 走 `handle_connection`

`handle_connection` ([server.rs:117](../../../crates/aionui-team/src/mcp/server.rs)) 握手：
1. 第一条 request 必须是 `initialize`，带 `auth_token` + `slot_id`（同时接受 `authToken`/`slotId` 驼峰别名）
2. 认证过后存 `caller_slot_id`
3. 支持 `notifications/initialized` / `tools/list` / `tools/call`

**鉴权模型**：token 在 `TeamMcpServer::start` 调用处由 `session.rs:39` 生成（`aionui_common::generate_id()`），每个 TeamSession 一个 token。**该 token 从未写入 ACP 的 `NewSessionRequest`，所以 ACP agent 拿不到**。

传输格式：4 字节大端长度前缀 + JSON payload，单帧最大 10 MB（[protocol.rs:86-107](../../../crates/aionui-team/src/mcp/protocol.rs)）。

#### 1.5.2 `tools.rs` — 8 个工具描述 + 输入校验

`all_tool_descriptors()` ([tools.rs:18-115](../../../crates/aionui-team/src/mcp/tools.rs)) 返回 **8 个** tool：

| 工具 | 作用 | 权限 |
|------|------|------|
| `team_send_message` | 发消息给指定 slot 或 `*` 广播 | 所有角色 |
| `team_spawn_agent` | 动态创建 teammate（白名单 `claude`, `codex`） | **Lead only** |
| `team_task_create` | 建任务 | 所有 |
| `team_task_update` | 改任务（status/description/owner/blocked_by） | 所有 |
| `team_task_list` | 列任务 | 所有 |
| `team_members` | 列成员 | 所有 |
| `team_rename_agent` | 改 agent 名 | 所有 |
| `team_shutdown_agent` | 发送 shutdown_request | **Lead only** |

**Backend 白名单**：`["claude", "codex"]`（[tools.rs:167](../../../crates/aionui-team/src/mcp/tools.rs)）。硬编码，注意和 `AcpBackend` 实际支持的 17 个 backend（[enums.rs:102](../../../crates/aionui-common/src/enums.rs)）不一致 —— 这里列白名单是为了防 prompt injection 乱 spawn。

**Dead code**：`parse_tool_call()` ([tools.rs:177](../../../crates/aionui-team/src/mcp/tools.rs)) 公开 API，但生产路径从 `server.rs::dispatch_tool` 直接 dispatch，未调用 `parse_tool_call`。仅在单元测试被引用。

#### 1.5.3 `bridge.rs` — TeamMcpStdioConfig

```rust
pub struct TeamMcpStdioConfig {
    pub port: u16,
    pub token: String,
    pub slot_id: String,
}
```

`to_env_map()` ([bridge.rs:25-32](../../../crates/aionui-team/src/mcp/bridge.rs)) 返回 `TEAM_MCP_PORT` / `TEAM_MCP_TOKEN` / `TEAM_AGENT_SLOT_ID`。

**关键事实**：这些环境变量**从未**被写入任何 agent 进程：`factory.rs` 里 `CommandSpec.env` 只从 agent registry 读取（[factory.rs:110](../../../crates/aionui-ai-agent/src/factory.rs)）；`TeamSession::mcp_stdio_config` 也没有生产调用。

### 1.6 `crates/aionui-team/src/mailbox.rs` — Mailbox

封装 `ITeamRepository` 的四个方法（[mailbox.rs:11-90](../../../crates/aionui-team/src/mailbox.rs)）：

| 方法 | 行 | 后端调用 |
|------|----|--------|
| `write(team_id, to_agent_id, from_agent_id, msg_type, content, summary)` | 20 | `repo.write_message()` |
| `read_unread(team_id, agent_id)` | 56 | `repo.read_unread_and_mark()` — 读取 + 原子标记已读 |
| `get_history(team_id, agent_id, limit)` | 74 | `repo.get_history()` |
| `delete_by_team(team_id)` | 85 | `repo.delete_mailbox_by_team()` |

`MailboxMessageType` 三种：`Message`、`IdleNotification`、`ShutdownRequest`（[types.rs:146-150](../../../crates/aionui-team/src/types.rs)）。

### 1.7 `crates/aionui-team/src/task_board.rs` — TaskBoard

CRUD + 依赖管理：

| 方法 | 行 | 说明 |
|------|----|------|
| `create_task(team_id, subject, description?, owner?, blocked_by[])` | 31 | 创建时验证依赖 task 存在，并回写依赖 task 的 `blocks` 字段 |
| `update_task(team_id, task_id, TaskUpdate)` | 75 | TaskUpdate: `{status, description, owner, blocked_by, metadata}`；若状态变 `Completed` 则调 `check_unblocks` |
| `list_tasks(team_id)` | 120 | 全部任务 |
| `check_unblocks` (私有) | 129 | 任务完成 → 把下游任务的 `blocked_by` 中去掉当前 id |

Task 状态 ([types.rs:198-203](../../../crates/aionui-team/src/types.rs))：`Pending` / `InProgress` / `Completed` / `Deleted`。

### 1.8 `crates/aionui-team/src/prompts.rs` — prompt 模板

三个函数（[prompts.rs:5-157](../../../crates/aionui-team/src/prompts.rs)）：

| 函数 | 产物 |
|------|------|
| `build_lead_prompt(team_name, members)` | Lead 的 system prompt：团队成员列表 + 8 个 team_* 工具说明 + Workflow Guidelines |
| `build_teammate_prompt(agent, team_name)` | Teammate 的 system prompt：身份 + 通信协议（`team_send_message` / `team_task_update` / idle/shutdown） |
| `build_wake_payload(agent, tasks, unread_messages)` | wake 时注入给 agent 的 markdown：未读消息列表 + 任务板表格 |

**用法缺失**：这三个函数**仅在 unit test 调用**，生产代码无人用（grep 证实）。即使 team 接通了，目前也没有代码把 `build_lead_prompt()` 作为 ACP agent 的 system prompt 注入 —— ACP 的 system prompt 走 `AcpBuildExtra.preset_context`（[types.rs:58-60](../../../crates/aionui-ai-agent/src/types.rs)），而 `TeamSessionService::create_team` 在构造 `CreateConversationRequest` 时**没有设置 preset_context**（[service.rs:63-74](../../../crates/aionui-team/src/service.rs)）。

### 1.9 `crates/aionui-team/src/types.rs` — 类型定义

| 类型 | 要点 |
|------|------|
| `TeammateRole` ([types.rs:11-36](../../../crates/aionui-team/src/types.rs)) | `Lead` / `Teammate`；接受 `"leader"` 别名 |
| `TeammateStatus` ([types.rs:42-82](../../../crates/aionui-team/src/types.rs)) | `Idle` / `Working` / `Thinking` / `ToolUse` / `Completed` / `Error`；接受 AionUi 别名（pending/active/failed） |
| `TeamAgent` ([types.rs:88-108](../../../crates/aionui-team/src/types.rs)) | `{slot_id, name, role, conversation_id, backend, model, custom_agent_id, status, conversation_type, cli_path}`；`backend` 接受 `agentType` 别名 |
| `Team` ([types.rs:129-138](../../../crates/aionui-team/src/types.rs)) | `{id, name, agents, lead_agent_id, created_at, updated_at}`（**没有 workspace 字段**，`TeamRow` 里有但 `Team` 没导出） |
| `MailboxMessageType` / `MailboxMessage` | 邮件类型 + 邮件结构（[types.rs:144-190](../../../crates/aionui-team/src/types.rs)） |
| `TaskStatus` / `TeamTask` | 任务状态 + 任务结构（带 `blocked_by` / `blocks`） |

### 1.10 `crates/aionui-ai-agent/src/types.rs` — AcpBuildExtra 当前字段

[types.rs:39-74](../../../crates/aionui-ai-agent/src/types.rs)：

```rust
pub struct AcpBuildExtra {
    pub agent_id: Option<String>,
    pub backend: Option<AcpBackend>,
    pub cli_path: Option<String>,
    pub agent_name: Option<String>,
    pub custom_agent_id: Option<String>,
    pub preset_context: Option<String>,
    pub skills: Vec<String>,
    pub preset_assistant_id: Option<String>,
    pub session_mode: Option<String>,
    pub cron_job_id: Option<String>,
}
```

**对 team 的支持**：**完全没有**。没有 `team_id`、`slot_id`、`team_mcp` 任一字段。

### 1.11 `crates/aionui-ai-agent/src/acp_agent.rs` — ACP session_new 调用点

[acp_agent.rs:454-458](../../../crates/aionui-ai-agent/src/acp_agent.rs)：

```rust
let session_response = self
    .protocol
    .new_session(NewSessionRequest::new(&self.workspace))
    .await
    .map_err(AppError::from)?;
```

**关键事实**：只传 `cwd=workspace`，`mcp_servers` 字段用默认值（空 vec，[agent.rs:954-964](https://docs.rs/agent-client-protocol-schema/0.12.0/src/agent.rs)）。SDK 的 `NewSessionRequest` 支持 `.mcp_servers(vec![...])` builder 方法（[schema/agent.rs:978-983](../../../../../../.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/agent-client-protocol-schema-0.12.0/src/agent.rs)），但没有被调用。

**结论**：ACP agent **看不到** team 模块的 8 个 MCP 工具。现在就算手工让 agent 跑，它在 prompt 里调 `team_send_message` 只会被 ACP 当成未知工具拒掉。

### 1.12 `crates/aionui-ai-agent/src/task_manager.rs` — IWorkerTaskManager

Trait 签名（[task_manager.rs:23-49](../../../crates/aionui-ai-agent/src/task_manager.rs)）：

```rust
pub trait IWorkerTaskManager: Send + Sync {
    fn get_task(&self, conversation_id: &str) -> Option<AgentManagerHandle>;
    fn get_or_build_task(
        &self,
        conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<AgentManagerHandle, AppError>;
    fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AppError>;
    fn clear(&self);
    fn active_count(&self) -> usize;
    fn collect_idle(&self, idle_threshold_ms: TimestampMs) -> Vec<String>;
}
```

Key by `conversation_id`（[task_manager.rs:25](../../../crates/aionui-ai-agent/src/task_manager.rs)）。**每个 agent 对应一个 conversation**，team 里的每个 agent 都应该单独调 `get_or_build_task(agent.conversation_id, ...)` 启动，但目前没有任何代码这么做。

### 1.13 `crates/aionui-ai-agent/src/factory.rs` — agent 工厂

ACP 分支（[factory.rs:88-154](../../../crates/aionui-ai-agent/src/factory.rs)）：
1. 反序列化 `extra` 为 `AcpBuildExtra`
2. 从 `agent_registry` 解析 backend / cli_path / env
3. `CommandSpec { command, args, env, cwd: Some(workspace) }`
4. 构造 `AcpAgentManager::new(conversation_id, workspace, is_custom_workspace, command_spec, config, skill_manager)`

**没有任何地方注入 team 相关参数**（port/token/slot_id）到 `AcpBuildExtra` 或 `CommandSpec.env`。

### 1.14 `crates/aionui-conversation/src/service.rs` — send_message / build_task_options

`build_task_options` ([service.rs:1029-1065](../../../crates/aionui-conversation/src/service.rs))：
```rust
fn build_task_options(&self, row: &ConversationRow) -> Result<BuildTaskOptions, AppError> {
    // ... 解析 row.type → agent_type
    // ... 解析 row.model → ProviderWithModel
    let extra: serde_json::Value = serde_json::from_str(&row.extra)?;
    let workspace = extra.get("workspace").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    Ok(BuildTaskOptions {
        agent_type,
        workspace,
        model,
        conversation_id: row.id.clone(),
        extra,
    })
}
```

**`extra` 里虽然可能有 `teamId`（由 `TeamSessionService` 写入），但 `build_task_options` 原样透传给 factory，factory 的 ACP 分支反序列化为 `AcpBuildExtra` 时会完全忽略它**（`AcpBuildExtra` 没有 `teamId` 字段，serde 默认丢弃未知字段）。

`send_message` ([service.rs:809-920](../../../crates/aionui-conversation/src/service.rs))：
- 主流程：校验 → 存 user message → `build_task_options` → `task_manager.get_or_build_task` → 改 status Running → 订阅 agent 事件 → send
- **不识别 team conversation**。agent 回复后不回调 `TeamSessionService::finalize_turn` 或任何 team scheduler 方法。

[service.rs:289](../../../crates/aionui-conversation/src/service.rs) 提到 `exclude_team_conversations: true`，说明列表端点会过滤 team 的 conversation，但这不代表有 team-aware 行为。

### 1.15 `crates/aionui-app/src/lib.rs` 和 `state_builders.rs` — 组装

[state_builders.rs:304-333](../../../crates/aionui-app/src/state_builders.rs)：

```rust
pub fn build_team_state(
    services: &AppServices,
    cron_service: Option<Arc<aionui_cron::service::CronService>>,
) -> TeamRouterState {
    // 建 team_repo / conv_repo / skill_resolver / conv_service
    let service = Arc::new(TeamSessionService::new(
        team_repo,
        conv_service,              // ← 只要 ConversationService
        services.event_bus.clone(),
    ));
    TeamRouterState { service }
}
```

**`worker_task_manager` 不传**。`TeamSessionService` 完全不知道 agent 怎么跑。

---

## 2. 数据流实际情况 vs 期望情况

### 实际：

```
用户                            HTTP Team Routes
 │                                   │
 │ POST /api/teams ──────────────▶ create_team
 │                                   │ 为每个 agent 建 conversation（extra.teamId=team_id）
 │                                   │ 写 team 行
 │                                   │
 │ POST /api/teams/{id}/session ──▶ ensure_session
 │                                   │ 启动 TeamMcpServer（随机端口+token）← 但 token 没人拿
 │                                   │ TeammateManager 建好 slots
 │                                   │
 │ POST /api/teams/{id}/messages ─▶ send_message
 │                                   │ 写 lead mailbox
 │                                   │ set_status(lead, Working)     ← 没人真的触发 agent
 │                                   ▼
 │                             ◀ 200 OK
 │
 │ 期待：lead agent 被唤醒、读邮箱、执行 team 工具、报告
 │ 实际：什么都没发生 —— 没有 scheduler loop、没有 wake → prompt 的桥
 ▼
```

### 期望（AionUi 参考）：

```
用户 → HTTP → TeamSession → scheduler.try_wake(lead_slot) → WakePayload
                                        │
                                        ▼
            WorkerTaskManager.get_or_build_task(lead_conv_id, BuildTaskOptions {
                extra.team_mcp = {port, token, slot_id},     ← 目前缺失
                extra.preset_context = build_lead_prompt()    ← 目前缺失
            })
                                        │
                                        ▼
             ACP AgentManager.new_session(NewSessionRequest::new(workspace)
                                            .mcp_servers(vec![team_mcp]))
                                            ↑ 目前直接 ::new(workspace)，mcp_servers 永远空
                                        │
                                        ▼
               ACP 进程拿到 team_* 工具，调工具 → MCP server → scheduler.execute_action
                                        │
                                        ▼
               ACP 进程返回 turn-complete → 后端应该调 scheduler.finalize_turn
                                            ↑ 目前没任何代码订阅这个事件
```

---

## 3. GAP 分析（对照 AionUi 参考实现）

> **数据来源**：下表每一行的"AionUi 有 ✅" 均能锚定到 [aionui-audit.md](./aionui-audit.md) 的具体章节（主 = 该文档行号）。本轮对照后，先前凭推断填的列已全部删除，事实不明的行标"未在源码找到"。
>
> GAP 按 AionUi 分类重排：**MCP 注入链 → Scheduler 可靠性 → Prompt 体系 → Agent 生命周期 → 数据层 → 事件层 → 协议层**。

### 3.1 P0（没它 team 跑不起来）

| # | 能力 | 后端有 | AionUi | GAP | aionui-audit 锚点 |
|---|------|:------:|:------:|-----|------|
| 1 | `AcpBuildExtra` 携带 team 上下文 | ❌ | ✅ | 无 `team_id` / `slot_id` / `teamMcpStdioConfig` 字段；factory 反序列化 `extra.teamMcpStdioConfig` 被直接丢弃 | §2.1 "启动 agent（MCP 注入）"、§3.1 "Team 内部 MCP 启动" |
| 2 | ACP `session/new` 携带 team MCP | ❌ | ✅ | `NewSessionRequest::new(workspace)` 不调 `.mcp_servers(...)`；AionUi 在 `acp/index.ts:1605-1656` 把 `teamMcpStdioConfig` 包成 `AcpSessionMcpServer` 塞进 `session/new` | §2.1 |
| 3 | `getOrStartSession` 语义 | ❌ | ✅ | 后端 `ensure_session` 只启动 MCP server + 填 `slots`；**不回写** `extra.teamMcpStdioConfig`，**不调** `task_manager.get_or_build_task({skipCache:true})` 重建 agent。AionUi 的 "全部成功才 `sessions.set`" 避免坏 session 缓存也没做 | §1.1 启动 session、§1.3 序列图 |
| 4 | `sendMessage(teamId, content)` → wake leader | ❌ | ✅ | 后端 `send_message` 写完 lead mailbox + set_status Working 就返回；AionUi 还会 `wakeAfterAcceptedDelivery(leadSlotId, 'team')` 真正拉一次流 | §4.1 wake 来源（user 对 team 说话） |
| 5 | `sendMessageToAgent(teamId, slotId, content, silent?, files?)` → wake target | ❌ | ✅ | 后端只写 mailbox；AionUi 写完后 `safeWake(targetSlotId)`；`silent=true` 还需要 **不** 写 user bubble 到目标 conversation | §4.1 "user 对某 agent 说话" |
| 6 | Wake 流程：首次→完整 role prompt + unread messages；后续→仅 unread messages；unread 为空→释放锁 idle 掉 | ❌ | ✅ | `try_wake` 返回 `WakePayload` 后无人消费；`build_lead_prompt` / `build_teammate_prompt` / `build_wake_payload` 生产链路为空 | §2.1 "启动 agent（prompt 注入）"、§2.1 "pending → idle 转换" |
| 7 | ACP turn 完成事件订阅 → `finalize_turn` | ❌ | ✅ | `AgentStreamEvent::Finish` 无订阅者；AionUi 通过 `teamEventBus.on('responseStream', …)` 监听 finish/error → `finalizeTurn(conversationId)` | §4.4 finalize_turn 流程 |
| 8 | Layer-2 Leader prompt 全内容注入 | ⚠️ | ✅ | `build_lead_prompt`（[prompts.rs:5-60](../../../crates/aionui-team/src/prompts.rs)）仅有"团队成员列表 + 8 个工具列表 + 7 条 Workflow"。AionUi `leadPrompt.ts:111-166` 有 Workflow 15 条、Model Selection Guidelines、Sequencing Dependent Work、Preset Assistant Selection 等**上千字节**的产品语义；还接受 `availableAgentTypes` / `availableAssistants` / `renamedAgents` / `teamWorkspace` 四个动态参数 | §5.1 三层结构、§5.2 动态 context |
| 9 | Layer-3 Teammate prompt 全内容注入 | ⚠️ | ✅ | `build_teammate_prompt`（[prompts.rs:62-89](../../../crates/aionui-team/src/prompts.rs)）三段概要；AionUi `teammatePrompt.ts:85-97` 显式 "Standing By (300s 超时防护)" + "Shutdown 协议原文" | §5.1 |
| 10 | 10 个 team 内部 MCP 工具 | ⚠️ 8/10 | ✅ | 缺 `team_describe_assistant`、`team_list_models`；`team_members` 输出格式和 `team_task_list` ID 截断（[server.rs:427-443](../../../crates/aionui-team/src/mcp/server.rs)）与 AionUi 对齐但需要验证 | §3.2 Team 内部 10 个工具 |
| 11 | 认证 token 机制 | ✅ | ✅ | 后端已实现（session 生成 token + handshake 校验），但目前 token 没有分发给任何 agent（见 #2） | §3.1 auth |
| 12 | MCP `tools/call` arg 上下文 | ❌ | ✅ | 后端 `handle_tools_call` 把 `caller_slot_id` 作为参数传给 handler；AionUi bridge 在每次 request 里附 `from_slot_id`，便于 `team_send_message` 取 `fromAgentId`。后端虽然有 `caller_slot_id`，但 "shutdown_approved/rejected" 消息**识别**逻辑缺失（AionUi `TeamMcpServer.ts:244-277` 在 `handleSendMessage` 里识别这两种消息） | §3.2 `team_send_message` 行为说明 |

### 3.2 P1（能跑但体验差 / 关键硬约束）

| # | 能力 | 后端有 | AionUi | GAP | aionui-audit 锚点 |
|---|------|:------:|:------:|-----|------|
| 13 | `activeWakes` 重入锁（正在 wake 时再次 wake 跳过） | ❌ | ✅ | scheduler 没有这个 set。无锁双 wake 会导致 mailbox 被双读 | §4.3 wake 重入与幂等、§8 #1/#2 |
| 14 | `wakeTimeouts` 60s 看门狗（流中 chunk reset、finish clear；超时→failed + idle_notification） | ❌ | ✅ | `WAKE_TIMEOUT_MS=60_000` 已定义（[scheduler.rs:16](../../../crates/aionui-team/src/scheduler.rs)）**但无 timer 读**。Stuck agent 永远不会被标 failed | §2.1 inactivity watchdog、§8 |
| 15 | `finalizedTurns` 5s dedup 窗口（避免同 turn 多次 finalize；re-wake 前显式 delete） | ❌ | ✅ | 无任何 dedup 机制 | §4.3、§8 #3 |
| 16 | `maybeWakeLeaderWhenAllIdle` 仅在 `{idle,completed,failed,pending}` 时 wake leader | ⚠️ | ✅ | 后端只检查 `TeammateStatus::Idle`（[scheduler.rs:495](../../../crates/aionui-team/src/scheduler.rs)），不把 `failed`/`completed` 当 settled，导致一个 failed teammate 会**永久阻塞** leader | §4.4、§8 #4 |
| 17 | Crash recovery：`finish.agentCrash` / error 含 `process exited unexpectedly`/`Session not found` → testament + kill + failed + wake leader；**leader crash 只 failed 不 remove** | ❌ | ✅ | 没有任何 crash 识别代码 | §2.1 crash recovery、§8 #6 |
| 18 | 429/Rate-limit 识别：error 正则 `/429\|rate.?limit\|quota\|too many requests/i` → failed | ❌ | ✅ | 无识别；429 会被当普通 error 冒泡 | §2.1 "429 / 限流识别" |
| 19 | `addAgentLocks` per-team 串行化并发 addAgent | ❌ | ✅ | `TeamSessionService::add_agent` 用 read-modify-write 更新 `team.agents`（[service.rs:197-253](../../../crates/aionui-team/src/service.rs)），并发下会丢 agent | §8 #14 |
| 20 | Leader 不可 shutdown / remove 的硬约束 | ⚠️ 部分 | ✅ | `shutdown_agent` 检查了 Lead-only caller（[scheduler.rs:451-455](../../../crates/aionui-team/src/scheduler.rs)），但目标 agent role 未检查；`remove_agent` 完全不检查 role。AionUi 两处都拒 | §8 #6 |
| 21 | `team_send_message` 识别 `"shutdown_approved"` / `"shutdown_rejected: <reason>"` | ❌ | ✅ | 后端把所有消息当普通 message 入箱。AionUi 拦截这两种消息：approved → `removeAgent(fromSlotId)` 并通知 leader；rejected → 写 reason 给 leader | §2.1 shutdown 协议、§3.2 |
| 22 | `team_spawn_agent` 真正 spawn teammate conversation + agent task | ❌ | ✅ | scheduler 只 log 不执行（[scheduler.rs:254-266](../../../crates/aionui-team/src/scheduler.rs)）；AionUi 走 `spawnAgent` 闭包 → `addAgent` → 写 `teamMcpStdioConfig` 回 extra → wake 新 agent | §2.1 MCP spawn、§1.3 getOrStartSession 闭包 |
| 23 | `team_shutdown_agent` 真正 kill 进程（发完 shutdown_request 等 teammate 回复） | ⚠️ | ✅ | 后端仅写 `ShutdownRequest` 到邮箱；AionUi 是同一套（也只写邮件），但**后续 teammate 回 `shutdown_approved` 时**要 kill 进程 + `removeAgent` —— 这一步后端未实现（见 #21） | §2.1 |
| 24 | `team_rename_agent` 规范化（trim + 去不可见字符 + 小写唯一性校验） + 保留 originalName 供 prompt | ❌ | ✅ | 后端直接写新 name（[scheduler.rs:336-346](../../../crates/aionui-team/src/scheduler.rs)）；AionUi 额外做规范化 + 唯一性 + `renamedAgents` map（供 prompt 显示 `[formerly: X]`） | §2.1 renameAgent |
| 25 | `TeammateStatus` 枚举 `pending` / `idle` / `active` / `completed` / `failed` | ⚠️ | ✅ | 后端是 `Idle` / `Working` / `Thinking` / `ToolUse` / `Completed` / `Error`（[types.rs:42-55](../../../crates/aionui-team/src/types.rs)）。语义差异：AionUi 的 `pending` = 首次 wake 前的 "未启动"；后端没有这个状态。并且接受 AionUi 别名（`pending`→Idle, `active`→Working, `failed`→Error）**已经做了**，但少了 `pending` 触发首次 role prompt 注入的区分 | §2.2 状态机 |
| 26 | 首次 wake vs 非首次 wake 区分 | ❌ | ✅ | 后端 `try_wake` 对所有 wake 一视同仁；AionUi 只在 `status in {pending, failed}` 才注入 role prompt，否则只发 unread messages | §2.1 |
| 27 | Teammate 收到的 mailbox message 额外 emit 为 UI 左气泡（leader 不 emit） | ❌ | ✅ | 后端没有 `user_content` / `teammate_message` 事件；AionUi `TeammateManager.ts:127-161` 逐条 emit 到 `acpConversation.responseStream` | §2.1 "active 期间输入字节流" |
| 28 | Team Guide MCP 单例（`aion_create_team` / `aion_list_models`） | ❌ | ✅ | 后端**没有**这个 server、stdio bridge、注入逻辑。AionUi 是 app 启动时建单例；solo agent 不在 team 时注入 | §3.1 Team Guide MCP 生命周期 |
| 29 | Layer-1 Team Guide Prompt 注入 | ❌ | ✅ | 后端 `AcpAgentManager` 的 system instructions 构造（`preset_context` 拼接路径）没有 `getTeamGuidePrompt({backend, leaderLabel})` 等价物 | §5.1 |
| 30 | `isTeamCapableBackend(backend, caps)` 白名单 + ACP stdio 能力判断 | ❌ | ✅ | 后端硬编码 `["claude", "codex"]`（[tools.rs:167](../../../crates/aionui-team/src/mcp/tools.rs)），仅用于 `team_spawn_agent` 白名单。AionUi 白名单是 `{gemini, claude, codex, aionrs}` + 其他 backend 按 `cachedInitResults.capabilities.mcpCapabilities.stdio===true` 动态判定 | §3.1 "Team Guide 能力判断" |
| 31 | User-scope 过滤（list_teams / get_team / remove_team） | ❌ | ✅ | [service.rs:127-170](../../../crates/aionui-team/src/service.rs) 未按 user_id 过滤。AionUi `listTeams(userId)` 直接按 user 查 | §1.1 列 team |
| 32 | `ConversationService.send_message` 识别 `extra.teamId` → 路由到 team 发送路径 | ❌ | ✅ | 当前两个路径（`conversation.send_message` 和 `team.send_message_to_agent`）互不感知 | §7.1 "对 agent 发话" |
| 33 | `TeamGuideMcpServer.handleCreateTeam` 建成后发的 IPC 事件 | ❌ | ✅ | AionUi 发 `team.listChanged` + `conversation.listChanged` + `deepLink.received { route:/team/<id> }`；后端这些 WS 事件都没有 | §1.2 建团流程、§7.6 |
| 34 | MCP `mcp_ready` 握手（bridge `server.connect` 后发 `{type:'mcp_ready', slot_id, auth_token}`；service `waitForMcpReady(slotId, 30s) graceful` 超时 resolve 不 reject） | ❌ | ✅ | 后端只有 `initialize` 握手，无 `mcp_ready` 握手 | §3.1 "MCP ready 握手"、§8 #11 |
| 35 | MCP 帧大小上限 64MB（`MAX_MCP_MESSAGE_SIZE`） | ⚠️ 10MB | ⚠️ 64MB | 后端 `10 * 1024 * 1024`（[protocol.rs:88](../../../crates/aionui-team/src/mcp/protocol.rs)）—— 工具返回大 JSON 会炸 | §3.1 TCP 传输协议 |
| 36 | MCP 请求默认超时 300s | ❌ | ✅ | 后端无请求级超时 | §3.1 |
| 37 | `getTeam` 的 agent 修复逻辑（`repairTeamAgentsIfMissing`，从 conversation.extra.teamId 反推） | ❌ | ✅ | 后端 `get_team` 直接读 repo，不做修复 | §1.1 |

### 3.3 P2（锦上添花）

| # | 能力 | 后端有 | AionUi | GAP |
|---|------|:------:|:------:|-----|
| 38 | `Team.workspace` / `workspace_mode` / `session_mode` 字段暴露给调用方 | ⚠️ | ✅ | `TeamRow` 有但 `Team`（[types.rs:129](../../../crates/aionui-team/src/types.rs)）不导出 |
| 39 | `CreateTeamRequest.agents[].role` 字段尊重 | ❌ | ✅ | 后端强制 i==0 为 Lead（[service.rs:56-60](../../../crates/aionui-team/src/service.rs)）；AionUi 允许输入指定 |
| 40 | `updateWorkspace(teamId, newWorkspace)` 级联更新每个 agent conversation 的 extra | ❌ | ✅ | 后端没有这个接口 |
| 41 | `setSessionMode(teamId, mode)` 供 spawn 继承 | ❌ | ✅ | 后端没有 |
| 42 | `team.mcpStatus` WS 事件（10 个 phase：`tcp_ready/tcp_error/session_injecting/session_ready/session_error/load_failed/degraded/config_write_failed/mcp_tools_waiting/mcp_tools_ready`） | ❌ | ✅ | 后端一个也没有 |
| 43 | HTTP 端点拉任务板 / 邮箱历史 | ❌ | ⚠️ | 调用方可以靠 WS 事件拼（AionUi 参考实现亦如此），但后端落地时可选加 |
| 44 | `team_describe_assistant` 的 locale 解析 + i18n fallback | ❌ | ✅ | 依赖后端 `assistants` 配置，当前后端无此配置来源 |

### 3.4 P0 gap 连锁关系（修正版）

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ P0 #1 + #2  AcpBuildExtra 加字段 + session/new 传 mcp_servers  │
  │ (agent 看得见 team_* 工具的先决条件)                            │
  └──────────────────────────┬──────────────────────────────────────┘
                             │
  ┌──────────────────────────▼──────────────────────────────────────┐
  │ P0 #8 + #9  Leader / Teammate Prompt 真内容注入                  │
  │ (agent 知道怎么用工具的先决条件 —— 要原样复用 AionUi 的         │
  │  leadPrompt.ts / teammatePrompt.ts 文本，不能自己造)             │
  └──────────────────────────┬──────────────────────────────────────┘
                             │
  ┌──────────────────────────▼──────────────────────────────────────┐
  │ P0 #3 + #4 + #5  ensure_session / sendMessage / sendMessageTo   │
  │ 真正触发 wake → agent 运行时（task_manager.get_or_build_task）   │
  │ (agent 真的跑起来的先决条件)                                    │
  └──────────────────────────┬──────────────────────────────────────┘
                             │
  ┌──────────────────────────▼──────────────────────────────────────┐
  │ P0 #6 + #7  wake 时注入 wake_payload / finish 时回 finalize_turn │
  │ (agent 之间形成"你一轮我一轮"闭环的先决条件)                    │
  └──────────────────────────┬──────────────────────────────────────┘
                             │
                             ▼
                        ...P1 可靠性机制 ...P2 工具完善
```

**P0 只解决 "agent 在对话里真能用 team 工具"。P1 的那些硬约束（activeWakes/finalizedTurns/crash recovery/leader 不可 shutdown）是"多 agent 协作不出乱子"的基础。aionui-audit §8 列了 17 条硬约束，P1 阶段必须全部落地，否则 agent 行为会漂移到什么都可能的状态**。

### 3.5 交叉审阅补漏（二轮对照 aionui-audit §7/§8 后新发现）

以下条目先前漏掉，现补进清单。每一条都锚定到 [aionui-audit.md](./aionui-audit.md) 的原文位置。

| # | 能力 | 后端有 | AionUi | GAP | 优先级 | aionui-audit 锚点 |
|---|------|:------:|:------:|-----|:------:|------|
| 45 | `send_message` / `send_message_to_agent` 支持附件 `files` | ❌ | ✅ | 后端 [`routes.rs:136-144`](../../../crates/aionui-team/src/routes.rs) 的 `SendTeamMessageRequest` / `SendAgentMessageRequest` 只有 `content` 字段；AionUi 签名 `{teamId, slotId, content, silent?, files?}` | **P0** | §7.1 "对 team 发话" / "对 agent 发话" |
| 46 | `wake_after_accepted_delivery` 的"log-not-throw"语义：wake 失败只 log 不 throw，**避免已入箱的消息被重复发送** | ❌ | ✅ | 后端当前 `send_message` 直接返回 mailbox 写结果；若 P0#4 接通 wake 后，wake 错误不能 propagate 给调用方，否则 retry 会双写 mailbox | **P0** | §4.1 表格备注 |
| 47 | `delete_team` 级联要 kill agent 进程 | ❌ | ✅ | 后端 [`service.rs:147-170`](../../../crates/aionui-team/src/service.rs) 的 `remove_team` 只调 `stop_session` + 删 conversation，**不调** `task_manager.kill(conversation_id, 'team_deleted')`；agent 进程会变成孤儿 | **P0** | §1.1 "删除 team（级联）"、§1.4 删除时序图 |
| 48 | 工具描述原文常量：`TEAM_SPAWN_AGENT_DESCRIPTION` 常量 + `getCreateTeamToolDescription()` 动态生成器 | ❌ | ✅ | 后端 [`tools.rs:30-45`](../../../crates/aionui-team/src/mcp/tools.rs) 是极简自造描述（"Dynamically create a new teammate agent (Lead only)."）；AionUi 对应描述是"3 PRECONDITIONS + STRICT 流程"的长文本，影响 agent 是否会在 spawn 前征求用户同意 | **P0** | §5.3、§7.4 |
| 49 | `resolveLeaderAssistantLabel(presetAssistantId)` 按 locale 解析 preset 显示名（给 Team Guide prompt 的 `leaderLabel`） | ❌ | ✅ | 后端没有 preset assistant 体系；Team Guide Prompt 注入时 `leaderLabel` 取不到 | **P1** | §5.2 、§7.4 |
| 50 | `formatMessages(messages, agents)` 邮件格式化：`[From <senderName\|User>] <content>\nFiles: <joined>` | ❌ | ✅ | 后端 [`prompts.rs:98-117`](../../../crates/aionui-team/src/prompts.rs) 的 `build_wake_payload` 用 "- From \`slot_id\` [type]: content" 格式，和 AionUi 不一致；会影响 LLM 对消息来源的解析 | **P1** | §7.4、§2.1 "消息格式" |
| 51 | MCP spawn 路径硬编码 `sessionMode='yolo'` / `workspaceMode='shared'` / `userId='system_default_user'` | ❌ | ✅ | 后端 `create_team` 接受 userId 参数（[`service.rs:39`](../../../crates/aionui-team/src/service.rs)），但本地开发模式用 `system_default_user` 的约定未写入；`sessionMode` 字段后端 `TeamSessionService` 根本没传给新建的 conversation（[`service.rs:63-74`](../../../crates/aionui-team/src/service.rs) 只传 `teamId`）。AionUi 这三个是 MCP-spawn 建团的硬默认值 | **P1** | §8 #15/#16、§1.2 建团流程 |
| 52 | Guide MCP 与 team 内部 MCP 互斥注入：`!extra.teamMcpStdioConfig` 时才注入 Guide；进了 team 只注入内部 MCP | ❌ | ✅ | 后端 Guide 完全不存在（#28），实现时必须保证：agent 只要 `extra.teamMcpStdioConfig` 非空就**不再**注入 Guide，否则 team 成员会把 `aion_create_team` 当工具再次建团 | **P1** | §3.3 图示下方备注、§8 #17 |
| 53 | 订阅 ACP stream 所有 chunk（text/tool/thought）用于 reset wakeTimeout（#14 的必要条件） | ❌ | ✅ | #14 的实现先决条件；后端订阅 `AgentStreamEvent` 需要覆盖 `Text`/`ToolUse`/`Thought` 等**所有** chunk，而不只是 `Finish` | **P1** | §2.1 inactivity watchdog |
| 54 | `TeammateManager.removeAgent` 要清理的内部表 | ❌ | ✅ | 后端 [`scheduler.rs:324-333`](../../../crates/aionui-team/src/scheduler.rs) 只从 slots 移除；AionUi 还要清 `wakeTimeouts / activeWakes / ownedConversationIds / finalizedTurns`（#13-15 的实现先决条件） | **P1** | §2.1 TeammateManager.removeAgent |
| 55 | `TeamTask.owner` 要允许 slotId 以外的 sentinel（比如 `null` / empty 表示 unassigned，AionUi 工具输出显示 "unassigned"） | ✅ | ✅ | 后端 `TeamTask.owner` 是 `Option<String>`（已支持），**但 `team_task_list` 输出格式**（[`mcp/server.rs:436-437`](../../../crates/aionui-team/src/mcp/server.rs)）要对齐 AionUi 的 `- [<id8>] <subject> (<status>, owner: <X> \| unassigned)` 格式 | **P2** | §3.2 `team_task_list` |

**小计**：新增 P0 4 条（45/46/47/48）、P1 6 条（49/50/51/52/53/54）、P2 1 条（55），总 GAP 数由 44 升到 **55**。

---

## 4. 跨 crate 改动分析

### 4.1 `aionui-api-types`（P0/P1）

- 新增 `team_mcp.rs` 类型：`TeamMcpStdioConfig { port, token, slot_id }` 公共类型（`aionui-team` 的 bridge.rs 已经有等价类型，考虑提升到 api-types 层供 ai-agent 复用；见 §4.3）。
- 新增事件 payload：`TeamListChangedPayload`、`TeamMcpStatusPayload`（用于 #33, #42）。`TeamAgentStatusPayload` 等已存在。

### 4.2 `aionui-common`（P1）

- 新增正则/常量：`RATE_LIMIT_PATTERN: &str = r"(?i)429|rate.?limit|quota|too many requests"`（给 scheduler crash detection 复用）。
- 新增超时常量：`TEAM_MCP_REQUEST_TIMEOUT_MS`（300_000）、`TEAM_MCP_READY_TIMEOUT_MS`（30_000）、`TEAM_MCP_MAX_FRAME_BYTES`（64 * 1024 * 1024）。
- 新增枚举：`TeamMcpPhase { TcpReady, TcpError, SessionInjecting, SessionReady, SessionError, LoadFailed, Degraded, ConfigWriteFailed, McpToolsWaiting, McpToolsReady }`（对应 #42）。

### 4.3 `aionui-ai-agent`（P0 重点）

1. `types.rs::AcpBuildExtra` 新增（P0#1）：
   ```rust
   #[serde(default)]
   pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
   ```
   命名对齐 AionUi `extra.teamMcpStdioConfig`。
2. `acp_agent.rs::session_new_and_prompt`（P0#2）：
   改为
   ```rust
   let mut req = NewSessionRequest::new(&self.workspace);
   if let Some(cfg) = &self.config.team_mcp_stdio_config {
       req = req.mcp_servers(vec![build_team_mcp_server(cfg)?]);
   }
   let session_response = self.protocol.new_session(req).await?;
   ```
   `build_team_mcp_server` 产出 `McpServer::Stdio { command: "node" 或 bridge binary, args, env: {TEAM_MCP_PORT, TEAM_MCP_TOKEN, TEAM_AGENT_SLOT_ID} }`。**需要决策**：后端是用 Rust 写一个 team-mcp-bridge 二进制（替代 AionUi 的 `teamMcpStdio.ts`），还是让 ACP 直接走 HTTP/SSE transport（`McpServer::Http`）。推荐前者，和 AionUi 架构对齐（§3.3 图示）。
3. `acp_agent.rs` 新增（P0#7）：Finish 事件广播 hook。目前 Finish 事件通过 `event_tx` 广播（[acp_agent.rs:514-518](../../../crates/aionui-ai-agent/src/acp_agent.rs)），`TeamSession` 需要订阅某个 conversation_id 的 stream 并在 Finish 时调 `scheduler.finalize_turn`。
4. `task_manager.rs::IWorkerTaskManager` 增强（P1）：新增 `wait_for_mcp_ready(conversation_id, slot_id, timeout) -> Result<()>`，graceful 超时不 reject（#34）。

### 4.4 `aionui-mcp`（P0 配套）

- 新增 `build_team_mcp_server(cfg: &TeamMcpStdioConfig) -> McpServer`（或挂在 team crate 里）。
- `build_session_mcp_servers` 可以扩展成合并 user MCP + team MCP + extension MCP。**先验证** `session_injection.rs:135` 的 `build_session_mcp_servers` 目前是否真的接到 ACP 生产路径；从 grep 看它只在集成测试里用，若是则 P0 还要打通这条链路。

### 4.5 `aionui-conversation`（P0/P1）

- `service.rs::build_task_options`（P0#32）：识别 `row.extra.teamId`，查 `TeamSessionService::get_session(team_id)` 拿 `TeamMcpStdioConfig`（按 conversation_id 反查 slot_id），塞进 `extra.team_mcp_stdio_config`。需要在 `ConversationService` 注入 `Option<Arc<TeamSessionService>>` 或新 trait `ITeamContextProvider`。
  - **备选**：把 team 相关的 send_message 路径完全从 conversation 分离（`TeamSession::send_message_to_agent` 自己调 `task_manager.get_or_build_task`，conversation 不需要知道 team）。推荐这个，避免下游 crate 依赖上游。

### 4.6 `aionui-team`（P0 重点 + P1 大量）

P0：

1. `service.rs::TeamSessionService::new` 加依赖 `task_manager: Arc<dyn IWorkerTaskManager>` + `Option<EncryptionKey>`（后续 credential）。
2. `service.rs::ensure_session`（P0#3 对齐 AionUi `getOrStartSession`）：
   - 启动 MCP server ✅ 已做
   - **新增**：遍历 agents，更新 `conversation.extra.teamMcpStdioConfig = session.mcp_stdio_config(slot_id)`
   - **新增**：`task_manager.get_or_build_task(conversation_id, opts_with_team_mcp)` 强制重建
   - **新增**：全部成功后才 `sessions.insert()`；失败时 stop MCP server + 不 insert
3. `session.rs::TeamSession::send_message` / `send_message_to_agent`（P0#4/#5）：
   - 写完 mailbox → `wake_agent(slot_id, first_prompt_if_pending_or_failed)` → `task_manager.send_message(conv_id, SendMessageData { content, msg_id, silent })`
4. `session.rs::TeamSession::on_agent_finish(conv_id, finish_data)`（P0#7）：
   - 订阅 `AgentManagerHandle::subscribe()` 的 Finish 事件（需要后台 task）
   - 查表 `conv_id → slot_id` → `scheduler.finalize_turn(slot_id, &[])`
   - 处理返回的 `Some(leader_slot_id)` → 重新 wake leader
5. `prompts.rs` 大幅扩写（P0#8/#9）：
   - 按 aionui-audit §5.2 动态节：新增参数 `availableAgentTypes`、`availableAssistants`、`renamedAgents`、`teamWorkspace`
   - **文本必须原样复用** AionUi `leadPrompt.ts` / `teammatePrompt.ts` / `teamGuidePrompt.ts`（aionui-audit §8 #5 强调这一点）；现有的极简中文模板要全部替换
6. `mcp/tools.rs` 新增 2 个工具（P0#10）：`team_describe_assistant`、`team_list_models`
7. `mcp/server.rs::handle_send_message`（P0#12/P1#21）：
   - 识别 `"shutdown_approved"` → `scheduler.remove_agent(from_slot_id)` + 写反馈给 leader
   - 识别 `"shutdown_rejected: <reason>"` → 写 reason 给 leader

P1：

8. `scheduler.rs::TeammateManager` 新增字段：`active_wakes: Mutex<HashSet<String>>`、`wake_timeouts: Mutex<HashMap<String, JoinHandle<()>>>`、`finalized_turns: Mutex<HashMap<String, Instant>>`（对应 #13/#14/#15）
9. `scheduler.rs::maybe_wake_leader_when_all_idle`（#16）：把 `failed` / `completed` 也当 settled
10. `scheduler.rs` + `session.rs`：crash recovery、429 detection、inactivity watchdog（#17/#18）。需要订阅 AcpStream 的 tool/thought/text chunk 事件 reset wake_timeout
11. `service.rs::add_agent` 加 `Mutex<HashMap<team_id, Mutex<()>>>` 串行化（#19）
12. `scheduler.rs::shutdown_agent` / `remove_agent` 添加 target role=Lead 拒绝检查（#20）
13. `scheduler.rs::rename_agent` 加规范化 + 唯一性 + `renamed_agents` map（#24）
14. `TeammateStatus` 枚举对齐（#25/#26）：要么新增 `Pending` variant 让首次 wake 知道要注入 role prompt；要么用 `renamed_agents` 以外的其他 metadata 表示"首次"。建议前者，和 AionUi 对齐最省事
15. `session.rs::TeamSession` 状态广播：给 teammate 的 mailbox message 额外 emit `team.message.new` WS 事件（#27）
16. `mcp/protocol.rs::MAX_FRAME_SIZE` 从 10MB 升到 64MB（#35）
17. `mcp/server.rs` 加请求级超时（#36）
18. `mcp/protocol.rs` 加 `mcp_ready` handshake type 支持（#34）—— 需要 bridge 端也支持

### 4.7 Team Guide MCP（P1#28/#29，新子系统）

全新模块：

- `crates/aionui-team/src/guide/` 目录（或新 crate `aionui-team-guide`）
- `guide/server.rs`：单例 TCP MCP server，启动时 `aionui_common::generate_id()` 做 auth token
- `guide/tools.rs`：2 个 tool（`aion_create_team`、`aion_list_models`）
- `guide/prompts.rs`：`build_team_guide_prompt({backend, leader_label})` 完整复用 AionUi 文本
- `guide/capability.rs`：`is_team_capable_backend(backend, caps)` 白名单 + `caps.mcp.stdio`
- `guide/stdio_bridge/`：Rust 二进制（替代 AionUi 的 `teamGuideMcpStdio.ts`），被 `McpServer::Stdio.command` 调用
- `aionui-ai-agent`: ACP 在构造 instructions 时如果不在 team 里且 `is_team_capable_backend(backend)` 则注入 `build_team_guide_prompt(...)` 到 `preset_context`，并在 `session/new` 的 `mcp_servers` 里追加 guide 配置

### 4.8 `aionui-app`（P0 配套）

- `state_builders.rs::build_team_state`（[state_builders.rs:304](../../../crates/aionui-app/src/state_builders.rs)）：加 `services.worker_task_manager.clone()` 参数
- `lib.rs`：初始化 Team Guide MCP 单例（P1#28）
- `lib.rs::shutdown`：增加 `services.team_session_service.dispose_all()` + `team_guide_singleton.stop()` 的 clean shutdown 流程

---

## 5. 完整改动一览（P0 必做）

| 文件 | 改动摘要 |
|------|--------|
| `crates/aionui-ai-agent/src/types.rs` | `AcpBuildExtra` 加 `team_mcp_stdio_config: Option<TeamMcpStdioConfig>`（字段名对齐 AionUi `teamMcpStdioConfig`） |
| `crates/aionui-ai-agent/src/acp_agent.rs` | `session_new_and_prompt` 调 `.mcp_servers(vec![build_team_mcp_server(cfg)?])`；Finish 事件需能被外部订阅 |
| `crates/aionui-mcp/src/session_injection.rs` | 新增 `build_team_mcp_server(&TeamMcpStdioConfig) -> McpServer` |
| `crates/aionui-team/src/service.rs` | `TeamSessionService::new` 加 `task_manager` 参数；`ensure_session` 完整实现（回写 extra + 重建 task）；`create_team`/`add_agent` 注入 `preset_context = build_lead_prompt/build_teammate_prompt(...)` |
| `crates/aionui-team/src/session.rs` | `send_message`/`send_message_to_agent` 写完 mailbox 后触发 agent task；新增 on_agent_finish 订阅 Finish → `scheduler.finalize_turn`；维护 `conversation_id → slot_id` 的映射表 |
| `crates/aionui-team/src/prompts.rs` | **整个文件重写**：leader/teammate 模板文本原样复用 AionUi `leadPrompt.ts` / `teammatePrompt.ts`（见 [team-prompts.md §1-§4](../team-prompts.md)）；新增动态参数 `availableAgentTypes/availableAssistants/renamedAgents/teamWorkspace` |
| `crates/aionui-team/src/mcp/tools.rs` | 新增 `team_describe_assistant`、`team_list_models` 两个 tool descriptor 和 handler |
| `crates/aionui-team/src/mcp/server.rs` | `handle_send_message` 识别 `shutdown_approved`/`shutdown_rejected` 拦截消息 |
| `crates/aionui-app/src/state_builders.rs` | `build_team_state` 传 `worker_task_manager` |

P1（strongly recommended，agent 行为硬约束）：详见 §4.6 第 8-18 项，核心是 **activeWakes / wakeTimeouts / finalizedTurns / crash recovery / 429 detection / addAgentLocks / leader 不可 shutdown 的 target 检查 / rename 规范化**。

P1 新子系统（Team Guide MCP）：详见 §4.7，需要独立的 server + stdio bridge binary + Prompt + 白名单逻辑。

---

## 6. 测试覆盖现状

| 测试文件 | 覆盖范围 | 是否使用 Mock |
|---------|--------|----------------|
| [`session.rs` 内联](../../../crates/aionui-team/src/session.rs) 138-337 | start/stop、mcp_stdio_config、send_message、agent lifecycle | 是（MockTeamRepo + NullBroadcaster） |
| [`scheduler.rs` 内联](../../../crates/aionui-team/src/scheduler.rs) 539-1256 | 状态机、反死循环、execute_action 全覆盖、finalize_turn | 是 |
| [`mailbox.rs` 内联](../../../crates/aionui-team/src/mailbox.rs) 93-254 | 读写、delete、history limit | 是 |
| [`task_board.rs` 内联](../../../crates/aionui-team/src/task_board.rs) 149-418 | CRUD、依赖链、unblock 传播 | 是 |
| [`prompts.rs` 内联](../../../crates/aionui-team/src/prompts.rs) 159-410 | 三个 builder 的 snapshot 式检查 | N/A（纯函数） |
| [`mcp/server.rs` / `mcp/protocol.rs` / `mcp/tools.rs`](../../../crates/aionui-team/src/mcp/) | 协议帧 roundtrip、tool 描述、parse_tool_call | 是 |
| [`tests/mcp_server_integration.rs`](../../../crates/aionui-team/tests/mcp_server_integration.rs) | 完整 TCP 握手 + tools/list + tools/call | 是（内存 repo） |
| [`tests/session_service_integration.rs`](../../../crates/aionui-team/tests/session_service_integration.rs) | TeamSessionService CRUD + ensure_session | 是 |
| [`tests/scheduler_integration.rs`](../../../crates/aionui-team/tests/scheduler_integration.rs) | Scheduler 跨组件 | 是 |

**整个 `aionui-team` crate 不依赖真正的 ACP/agent 进程做 e2e 测试。** P0 gap 修好之后需要在 `crates/aionui-app/tests/` 加 e2e 测试跑真的 ACP（起 `claude --experimental-acp` 子进程或其它可用 CLI）来验证。

---

## 7. 参考调用图

```
╔═════════════════════════════════════════════════════════════════╗
║                   用户 ↔ HTTP Team Routes                        ║
║                            │                                    ║
║                   ┌────────▼─────────┐                         ║
║                   │ TeamSessionService│                        ║
║                   │   • repo          │                        ║
║                   │   • conv_service  │← 建 conversation       ║
║                   │   • broadcaster   │                        ║
║                   │   • sessions(dash)│                        ║
║                   └────────┬─────────┘                         ║
║                            │ ensure_session                   ║
║                   ┌────────▼─────────┐                         ║
║                   │   TeamSession    │ (内存，每 team 一个)    ║
║                   │ • scheduler      │                        ║
║                   │ • mailbox        │                        ║
║                   │ • task_board     │                        ║
║                   │ • mcp_server ─────┐ TCP 127.0.0.1:随机端口 ║
║                   └───────┬──────────┘ │                      ║
║                           │            │ JSON-RPC over TCP    ║
║              ┌────────────┼────────────┘                      ║
║              │            │                                   ║
║              │            │                                   ║
║              │  ╔═════════▼══════════════════╗                ║
║              │  ║  GAP: ACP agent 看不到这里 ║  ← P0#1         ║
║              │  ║        的 port/token       ║                ║
║              │  ╚═════════════════════════════╝                ║
║              │                                                 ║
║              │ (应当)                                          ║
║              ▼                                                 ║
║     ┌────────────────┐                                        ║
║     │ WorkerTaskMgr  │ (按 conversation_id 索引)              ║
║     │  → ACP agent   │                                         ║
║     └────────┬───────┘                                         ║
║              │                                                 ║
║  ╔═══════════▼══════════════════════════════════════╗         ║
║  ║ GAP: agent Finish 事件 → TeamSession 没订阅      ║ ← P0#2   ║
║  ║  → finalize_turn 永远不会被触发                  ║          ║
║  ╚═══════════════════════════════════════════════════╝        ║
╚═════════════════════════════════════════════════════════════════╝
```

---

## 8. 对齐 aionui-audit 之后原 7 个待验证点的答案

| 先前问题 | 基于 aionui-audit.md 的答案 |
|---------|---------------------------|
| AionUi 建 team 后，每个 agent 的 ACP `session/new.mcp_servers` 是否带 team MCP？ | **是**。`src/process/agent/acp/index.ts:1605-1656` 用 `loadBuiltinSessionMcpServers()` 把 `extra.teamMcpStdioConfig` 包成 `AcpSessionMcpServer` 注入。见 aionui-audit §2.1 / §3.1。 |
| agent turn 结束的 `finalize_turn` 由谁负责？ | **后端监听 `teamEventBus.on('responseStream')`，type=`finish` 或 `error` 时调 `finalizeTurn(conversationId)`**（`TeammateManager.ts:283-452`）。见 §4.4。 |
| `try_wake` 是注入 prompt 还是别的方式？ | **首次 wake** 或 `status∈{pending,failed}` 时**注入完整 role prompt + `## Unread Messages`**；**非首次 wake** 仅发 mailbox messages；mailbox 为空时直接设 idle 释放锁。见 §2.1。 |
| `SpawnAgent` 怎么完成？ | **TeamSession 层完成**。`TeamMcpServer.handleSpawnAgent` 校验 leader → 调外部 `spawnAgent(name, agent_type, model, custom_agent_id)` 闭包（在 `getOrStartSession` 里定义，调 `teamSessionService.addAgent` + 写回 extra）→ 再 `safeWake(newAgent.slotId)`。见 §2.1 / §1.3。 |
| `ShutdownAgent` 是否 kill ACP 进程？ | **分两步**：1) leader 调 `team_shutdown_agent` → 只写 `shutdown_request` 到 mailbox + `safeWake(target)`；2) teammate 回 `shutdown_approved` 消息 → `team_send_message` 拦截 → `removeAgent(fromSlotId)` → `TeammateManager.removeAgent` 调 `workerTaskManager.kill(conversationId)` 真 kill。见 §2.1。 |
| AionUi 参考实现是否有 HTTP 拉任务板 / 邮箱？ | **没有**。AionUi 全走 `ipcBridge.team.*` 事件推送，没有专门的 REST endpoint。见 §7.6。后端要实现的话是锦上添花（我们自己决定）。 |
| Team Guide MCP 是独立还是合并？ | **独立**。app 启动时 `initTeamGuideService()` 建单例；solo agent 通过 ACP `session/new.mcp_servers` 和 team 内部 MCP 一样方式注入，但是独立的 stdio bridge + 独立的 TCP server + 独立的 auth token。**注意**：同一 agent 一旦进 team，Guide MCP 就不再注入（`!this.extra.teamMcpStdioConfig` 条件），Guide 和 team 内部 MCP 互斥。见 §3.1 / §3.3。 |

## 9. 仍需进一步确认的事项

1. **`agent-client-protocol-schema::McpServer` 的 variant 细节**：目前只确认 `NewSessionRequest.mcp_servers: Vec<McpServer>` 签名存在；`McpServer` 的 stdio / http / sse 各 variant 形状尚未展开读。实施前需要读 [schema/agent.rs](../../../../../../.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/agent-client-protocol-schema-0.12.0/src/agent.rs) 中的 `McpServer` 类型，确认 `Stdio { command, args, env }` 是否就是 AionUi `AcpSessionMcpServer` 的等价形态。
2. **`aionui-mcp::build_session_mcp_servers` 是否已接入 ACP 生产路径**：grep 只在集成测试和 `aionui-mcp::lib.rs` pub use 中看到，`aionui-ai-agent` 没有调用链。**需要实施前确认**：当前 ACP `session/new` 是否已经把 user MCP servers 注入？若否，P0 还要先把 user MCP 注入链接上。
3. **ConversationRow.extra 字段命名一致性**：AionUi 参考实现存的是 `extra.teamMcpStdioConfig` 驼峰，后端 `TeamSessionService::create_team` 存的是 `extra.teamId` 驼峰；但后端 conversation 的 `extra` 存储方式（直接 JSON 字符串 serialize/deserialize）不保证驼峰。实施前需确认接口调用方/IPC 是否要求 snake_case 还是 camelCase，以便 `AcpBuildExtra` 的 serde 别名正确。
4. **接口调用方是否只靠 WebSocket 渲染团队状态**：[frontend-guide.md](../frontend-guide.md) 需要核对；如果调用方要拉任务板/邮箱历史做初始化渲染，#43 就从 P2 提到 P1。
5. **`TeammateStatus::Pending` vs 当前 `Idle` 的迁移策略**：后端已经用 serde alias 让 `"pending"` 反序列化为 `Idle`，但 AionUi 生产代码 `status === 'pending'` 是触发首次 role prompt 注入的关键判断。**方案**：新增 `Pending` variant + 改 alias；或者在 `TeamAgent` 加一个 `has_been_waked: bool` 字段。前者和 AionUi 对齐度更高。
6. **Rust 端 stdio bridge 的打包方式**：AionUi 的 bridge 是 `node + scriptPath`；后端如果要重新实现 bridge，是打成独立 `aionui-team-mcp-bridge` 二进制（app 发布时一起分发）还是用子命令 `aionui-backend team-mcp-bridge`。这决定了 `McpServer::Stdio.command` 的值。
7. **CLAUDE.md 的"只存事实，不存观点"在 P1 Prompt 重写时的尺度**：AionUi 的 prompt 文本里大量是"产品语义判断"（例如 shutdown 前必须用户显式要求、spawn 前必须拿到用户同意），这些并不是 code convention 而是 LLM 行为契约。§8 #5 建议"原样复用"。实施时需要确认后端是否允许直接把 AionUi 的英文 prompt 移植过来作为 Rust 常量。
