# Phase1 接口契约（先冻接口再开发）

> **原则**：所有模块的 Rust 签名在 Wave 1 开工前冻结；后续模块照此实现，Wave 2 串联时不改签名。
>
> **事实来源**：
> - [backend-audit.md](./backend-audit.md) §1.10 / §1.11 / §4.1–§4.8
> - [aionui-audit.md](./aionui-audit.md) §2.1 / §3.1 / §4 / §7
> - [mcp.md](../mcp.md) §4.5 / §4.6
>
> **相关文档**：[README.md](./README.md) · [modules.md](./modules.md) · [milestones.md](./milestones.md)
>
> **范围**：只覆盖 phase1 范围（ACP + 最小闭环）。Wave 1 的契约在 §1/§2/§3/§4；Wave 2 的契约在 §5/§6/§7/§8。

---

## 0. 总览：改动面

| 类型 | 目标 |
|------|------|
| 新增 | `aionui-api-types::team_mcp` 子模块（提升 `TeamMcpStdioConfig`） |
| 新增 | `aionui-team::mcp::bridge` 新 struct `TeamMcpStdioServerSpec`（session/new 注入体） |
| 新增 | `aionui-team::prompts` 4 份常量字符串（leader/teammate/guide/spawn tool desc）+ builder 新签名 |
| 新增 | `aionui-team::mcp::tools` 两个工具 descriptor + handler：`team_list_models` / `team_describe_assistant` |
| 新增 | `aionui-team::session::TeamSession::stdio_spec(slot_id)` 返回 `TeamMcpStdioServerSpec` |
| 新增 | `aionui-team::session::TeamSession::on_agent_finish(conversation_id, is_error)` 供 Wave 2 转发 Finish |
| 新增 | `aionui-app` 子命令 `mcp-bridge`（stdio bridge 入口，无新 trait） |
| 修改 | `aionui-ai-agent::AcpBuildExtra` 加字段 `team_mcp_stdio_config: Option<TeamMcpStdioConfig>` |
| 修改 | `aionui-ai-agent::acp_agent::session_new_and_prompt` 按 config 注入 mcp_servers |
| 修改 | `aionui-team::service::TeamSessionService::new` 加 `task_manager: Arc<dyn IWorkerTaskManager>` 入参 |
| 修改 | `aionui-team::service::ensure_session` 实现 kill+rebuild 闭环（写回 extra → kill → get_or_build_task） |
| 修改 | `aionui-team::session::TeamSession::send_message / send_message_to_agent` 接 wake 路径 |
| 修改 | `aionui-conversation::service::ConversationService::update_extra(conv_id, patch)` 【新增接口】供 `ensure_session` 写 `team_mcp_stdio_config`（extra 是 JSON 字符串列，不需 schema 迁移） |
| 修改 | `aionui-app::state_builders::build_team_state` 传 worker_task_manager |

以下按模块给出签名。**字段命名对齐 aionui-backend Rust 规则（snake_case）；对外 JSON 字段按 backend-audit §1.10 的事实保持 snake_case（rebase 后已经全面去 `rename_all=camelCase`，见 commit `dae96f8`）。**

---

## 1. `aionui-api-types` 新增类型（Wave 1 · 模块 D1）

**文件**：新增 `crates/aionui-api-types/src/team_mcp.rs`；在 `lib.rs` `pub mod team_mcp; pub use team_mcp::*;`

```rust
use serde::{Deserialize, Serialize};

/// team session MCP server 的 stdio 连接三元组
/// (提升自 aionui-team::mcp::bridge，供 aionui-ai-agent 反序列化 AcpBuildExtra 用)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMcpStdioConfig {
    pub port: u16,
    pub token: String,
    pub slot_id: String,
}

impl TeamMcpStdioConfig {
    /// stdio bridge 读到的 env 名（固定常量，不可改；aionui-audit §3.1 列出）
    pub const ENV_PORT: &'static str = "TEAM_MCP_PORT";
    pub const ENV_TOKEN: &'static str = "TEAM_MCP_TOKEN";
    pub const ENV_SLOT_ID: &'static str = "TEAM_AGENT_SLOT_ID";
}
```

**废弃**：`aionui-team::mcp::bridge::TeamMcpStdioConfig` 原定义改成 `pub use aionui_api_types::TeamMcpStdioConfig;`（保持 import path 不断）。

**不引入依赖**：`aionui-api-types` 继续禁 axum/tower（AGENTS.md 硬规则）。

---

## 2. `aionui-ai-agent::types::AcpBuildExtra` 扩展（Wave 1 · 模块 D2）

**文件**：`crates/aionui-ai-agent/src/types.rs`

```rust
use aionui_api_types::TeamMcpStdioConfig;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AcpBuildExtra {
    // ...既有字段原样保留...

    /// 若存在，factory 构造 session/new 时会把它包成 stdio MCP server 注入
    /// 字段名严格是 snake_case，由 ConversationService.build_task_options 或
    /// TeamSessionService.ensure_session 写进 conversation.extra
    #[serde(default)]
    pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
}
```

**兼容约束**：
- `#[serde(default)]` + `Option` 保证旧 extra 无此字段反序列化为 `None`（单聊零影响，backend-audit §4.3 引用）
- **字段命名规则**：新增字段统一 **snake_case**（本字段 `team_mcp_stdio_config` 即如此）；既有 `teamId` 字段（[`service.rs:73`](../../../crates/aionui-team/src/service.rs) 早期写入）**留作历史兼容**不动，但不再新增驼峰字段。conversation.extra 的 JSON 由后端自己写自己读，snake_case 自洽即可（commit `dae96f8` 方向）

---

## 3. `aionui-team::mcp::bridge` 新增 ServerSpec（Wave 1 · 模块 D3）

**文件**：`crates/aionui-team/src/mcp/bridge.rs`

```rust
use aionui_api_types::TeamMcpStdioConfig;

/// session/new 用的 stdio MCP server 完整描述
/// （直接可以被 NewSessionRequest::mcp_servers 消费）
#[derive(Debug, Clone)]
pub struct TeamMcpStdioServerSpec {
    pub name: String,          // 固定 "aionui-team-<team_id>"
    pub command: String,       // aionui-backend 的绝对路径，由调用方传入（见 §7）
    pub args: Vec<String>,     // 固定 vec!["mcp-bridge".into()]
    pub env: Vec<(String, String)>, // 三个 env，见 §1 的常量
}

impl TeamMcpStdioServerSpec {
    pub fn from_config(
        team_id: &str,
        backend_binary_path: &str,
        cfg: &TeamMcpStdioConfig,
    ) -> Self { /* ... */ }
}
```

**注意**：`backend_binary_path` 不从 env 猜，由 `AppServices` 启动时通过 `std::env::current_exe()` 读一次并缓存（phase1 约束：只打进主二进制，不出 standalone bridge binary）。

---

## 4. `aionui-team::mcp::tools` 新增两个工具 descriptor（Wave 1 · 模块 D4）

**文件**：`crates/aionui-team/src/mcp/tools.rs`

**新增 descriptor**（phase1 只要求 descriptor 和 **最小实现** —— 返回 stub 数据也算通过，Wave 2 再接真实数据源）：

```rust
// descriptor 文本必须原样复用 team-prompts.md §5.2 的 team_list_models / team_describe_assistant
// （aionui-audit §8 硬约束：prompt/tool description 原样复用，禁改写）
pub fn team_list_models_descriptor() -> ToolDescriptor { /* ... */ }
pub fn team_describe_assistant_descriptor() -> ToolDescriptor { /* ... */ }
```

**handler 签名**（server.rs dispatch 层调用）：

```rust
// phase1 最小实现：返回固定 backend 列表（claude / codex / gemini / aionrs）
// Wave 2 再接 agent_registry / assistants 配置
pub fn handle_team_list_models(args: &serde_json::Value) -> ToolResult { /* ... */ }

// phase1 最小实现：返回 "Preset assistant not found" —— 后端没有 preset 配置来源
// Wave 2 再加 assistants 配置表
pub fn handle_team_describe_assistant(args: &serde_json::Value) -> ToolResult { /* ... */ }
```

**`all_tool_descriptors()`** 扩展为返回 **10 条**，顺序与 aionui-audit §3.2 表格一致。

---

## 5. `aionui-team::prompts` 大幅扩写（Wave 1 · 模块 D5）

**文件**：`crates/aionui-team/src/prompts.rs`（现有 3 个 builder 重写）

**新增常量**（原样复用 AionUi 英文，**不翻译不改写**，见 team-prompts.md §5 / §3 / §4）：

```rust
/// teamGuidePrompt.ts 的 Rust 端完整等价，面向 solo ACP agent
/// phase1 仅定义；Layer-1 注入由 Wave 2 模块 D7 的 send 路径接到 AcpBuildExtra.preset_context（或 wake payload）
pub const TEAM_GUIDE_PROMPT_TEMPLATE: &str = r#"..."#; // 108 行 AionUi 原文
pub const LEAD_PROMPT_TEMPLATE: &str = r#"..."#;        // 188 行 AionUi 原文
pub const TEAMMATE_PROMPT_TEMPLATE: &str = r#"..."#;    // 114 行 AionUi 原文
pub const TEAM_SPAWN_AGENT_DESCRIPTION: &str = r#"..."#; // toolDescriptions.ts 19 行
```

**builder 签名（替换现有）**：

```rust
pub struct LeadPromptParams<'a> {
    pub team_name: &'a str,
    pub teammates: &'a [TeamAgent],
    pub available_agent_types: &'a [AvailableAgentType], // phase1 可传空 vec
    pub available_assistants: &'a [AvailableAssistant],   // phase1 可传空 vec
    pub renamed_agents: &'a HashMap<String, String>,      // phase1 可传空 map
    pub team_workspace: Option<&'a str>,
}

pub struct TeammatePromptParams<'a> {
    pub agent: &'a TeamAgent,
    pub team_name: &'a str,
    pub leader: &'a TeamAgent,
    pub teammates: &'a [TeamAgent],
    pub renamed_agents: &'a HashMap<String, String>,
    pub team_workspace: Option<&'a str>,
}

pub struct TeamGuidePromptParams<'a> {
    pub backend: &'a str,          // "claude" / "codex" / "gemini" / ...
    pub leader_label: Option<&'a str>, // preset assistant 显示名，phase1 传 None
}

pub fn build_lead_prompt(params: &LeadPromptParams<'_>) -> String;
pub fn build_teammate_prompt(params: &TeammatePromptParams<'_>) -> String;
pub fn build_team_guide_prompt(params: &TeamGuidePromptParams<'_>) -> String;

/// wake 时作为"首个 send_message content"的 payload（不走 preset_context）
/// 由 Wave 2 模块 D8 的 wake 逻辑调用
pub fn build_wake_payload(
    agent: &TeamAgent,
    tasks: &[TeamTask],
    unread_messages: &[MailboxMessage],
    sender_name_lookup: &HashMap<String, String>, // slot_id -> name, 用于 formatMessages
) -> String;
```

**新类型**（phase1 最小字段，供 builder 用；Wave 2 再填值）：

```rust
pub struct AvailableAgentType { pub agent_type: String, pub display_name: String }
pub struct AvailableAssistant {
    pub custom_agent_id: String,
    pub name: String,
    pub backend: String,
    pub description: String,
    pub skills: Vec<String>,
}
```

**硬约束**（aionui-audit §8 #5、team-prompts.md §5）：模板常量一旦定义，Wave 2 只能填 param，**不得改模板文本**。

---

## 6. `aionui-team::session::TeamSession` 新方法（Wave 2 · 模块 D7）

**文件**：`crates/aionui-team/src/session.rs`

```rust
impl TeamSession {
    /// 返回供 ACP session/new 用的 stdio server spec
    /// 由 TeamSessionService.ensure_session 调用，写进每个 agent 的 conversation.extra
    pub fn stdio_spec(&self, slot_id: &str) -> TeamMcpStdioServerSpec;

    /// Wave 2 入口：Finish/Error 事件到达时调这里
    /// 签名选择 async，内部做 finalize_turn + maybeWakeLeaderWhenAllIdle
    pub async fn on_agent_finish(
        &self,
        conversation_id: &str,
        is_error: bool,
    ) -> Result<(), TeamError>;

    /// 触发 wake 的内部入口（由 send_message / send_message_to_agent 调用）
    /// 返回的 (prompt, content) 会被 TeamSessionService 喂给 task_manager
    pub async fn compute_wake_input(
        &self,
        slot_id: &str,
    ) -> Result<Option<WakeInput>, TeamError>;
}

/// Wave 2 D8 的输出类型
pub struct WakeInput {
    pub conversation_id: String,
    /// 若 agent.status 是 Pending/Failed，这里是完整 role prompt + wake payload
    /// 否则仅 wake payload（mailbox formatted messages）
    pub first_message: String,
    /// None 时说明 mailbox 为空，调用方应 skip wake + mark_idle
    pub should_send: bool,
}
```

---

## 6.5 `aionui-team::scheduler::TeammateManager` 签名扩展（Wave 2 · 模块 D8）

**文件**：`crates/aionui-team/src/scheduler.rs`

D8 负责把 scheduler 的已有方法**接上生产路径**。以下签名 AionUi 参考实现已全部实现（`TeammateManager.ts`），后端代码里方法存在但行为不完整。

```rust
impl TeammateManager {
    // ── 现有方法，行为扩展 ──

    /// mark_idle 扩展：
    /// 1. set_status(slot_id, Idle)（现有）
    /// 2. 【新增】写一条 idle_notification 到 mailbox（from=slot_id, to=lead, summary=完成摘要）
    /// 3. 【新增】广播 team.agentStatusChanged WS 事件
    /// 4. 调 maybe_wake_leader_when_all_idle（现有，但 settled 判定要扩展）
    pub async fn mark_idle(
        &self,
        slot_id: &str,
        summary: Option<String>,   // 新增参数：idle_notification 的 summary
    ) -> Result<Option<String>, TeamError>;
    // 返回值不变：Some(lead_slot_id) 表示需要 wake leader

    /// maybe_wake_leader_when_all_idle 扩展：
    /// 现有只看 Idle，需扩展为 settled = {Idle, Completed, Failed, Pending}
    /// （AionUi TeammateManager.ts:440-452 的判定逻辑）
    pub fn maybe_wake_leader_when_all_idle(&self) -> Option<String>;
    // 签名不变，内部逻辑改

    /// finalize_turn 扩展：
    /// 现有接收 Vec<SchedulerAction> 并执行。
    /// 【新增行为】：执行完 actions 后调 mark_idle(slot_id, summary)
    /// summary 从 actions 里的 IdleNotification variant 提取
    pub async fn finalize_turn(
        &self,
        slot_id: &str,
        actions: Vec<SchedulerAction>,
    ) -> Result<(), TeamError>;
    // 签名不变，内部逻辑改

    // ── 新增方法 ──

    /// activeWakes 去重：防止同一 agent 被并发 wake 两次
    /// （AionUi TeammateManager.ts:94-100 的 activeWakes Map）
    /// 返回 true = 获得锁可以 wake；false = 已有 wake 在跑，skip
    pub async fn acquire_wake_lock(&self, slot_id: &str) -> bool;

    /// 释放 wake 锁（wake 完成或失败时调用）
    pub async fn release_wake_lock(&self, slot_id: &str);
}
```

**settled 判定扩展细节**：

```
// 现有（scheduler.rs:495）：
if slot.status != TeammateStatus::Idle { return false; }

// phase1 改为：
fn is_settled(status: &TeammateStatus) -> bool {
    matches!(status,
        TeammateStatus::Idle
        | TeammateStatus::Completed
        | TeammateStatus::Error      // 对应 AionUi 的 Failed
        | TeammateStatus::Pending    // 注意：后端 Pending 目前 serde alias 映射到 Idle，
                                     // phase1 需要恢复独立 Pending variant
    )
}
```

**activeWakes 数据结构**：

```rust
// TeammateManager 新增字段
active_wakes: DashSet<String>,  // slot_id 集合
```

**与 D7 的交互**：
- D7 的 `on_agent_finish` 调 `finalize_turn`（D8 提供）
- D7 的 `compute_wake_input` 调 `acquire_wake_lock` + `try_wake` + `release_wake_lock`（D8 提供）
- D8 不直接依赖 D7

**测试策略**：
- `mark_idle` 写 idle_notification 到 mailbox → 用真实内存 DB 断言
- `maybe_wake_leader_when_all_idle` 对 settled 集合的 4 种状态组合 → 单元测试
- `acquire_wake_lock` / `release_wake_lock` 并发安全 → tokio::spawn 两个并发 wake 断言只有一个成功

---

## 7. `aionui-ai-agent::acp_agent::session_new_and_prompt` 注入（Wave 2 · 模块 D10）

**文件**：`crates/aionui-ai-agent/src/acp_agent.rs:448-458`

**现状**：
```rust
let session_response = self.protocol
    .new_session(NewSessionRequest::new(&self.workspace))
    .await?;
```

**phase1 改为**：
```rust
let mut req = NewSessionRequest::new(&self.workspace);
if let Some(cfg) = &self.config.team_mcp_stdio_config {
    let spec = TeamMcpStdioServerSpec::from_config(
        /* team_id 从 cfg 不直接含，改从 env 或 cfg.slot_id 的前缀取；phase1 简化：
           name 直接写 "aionui-team" 足以区分 */
        "",
        &self.backend_binary_path,   // 新增字段：AcpAgentManager 构造时从 AppServices 接
        cfg,
    );
    req = req.mcp_servers(vec![spec.into_sdk()]);
}
let session_response = self.protocol.new_session(req).await?;
```

**`TeamMcpStdioServerSpec::into_sdk()`** 返回 `agent_client_protocol_schema::McpServer`（phase1 前置：模块 D2 要先读 `McpServer` 的 variant 形状，确定 stdio 入口；backend-audit §9 明确列为"实施前确认项"）。

**新字段 `backend_binary_path`**：`AcpAgentManager::new(...)` 构造签名扩展（非 breaking，只加一个 `backend_binary_path: Arc<PathBuf>`），由 `build_agent_state` 从 `std::env::current_exe()` 缓存后传入。

---

## 8. `aionui-app` 新增子命令 `mcp-bridge`（Wave 1 · 模块 D6）

**文件**：`crates/aionui-app/src/lib.rs`（或新子模块 `bridge.rs`）

**CLI 入口**：
```rust
// main.rs
if args.get(1).map(|s| s.as_str()) == Some("mcp-bridge") {
    aionui_app::bridge::run_mcp_bridge().await;
    return;
}
```

**bridge 函数签名**：
```rust
/// 由 ACP CLI 通过 session/new.mcp_servers 拉起的 stdio bridge 子进程
///
/// 职责（对应 mcp.md §4.6 的 4 步）：
/// 1. 从 env 读 TEAM_MCP_PORT / TEAM_MCP_TOKEN / TEAM_AGENT_SLOT_ID
/// 2. 接 stdin/stdout：rmcp ServerHandler（初始化 + tools/list + tools/call 透传）
/// 3. TCP 连 127.0.0.1:<port>：aionui_team::mcp::protocol 的 4 字节长度帧 + JSON-RPC
/// 4. 启动后发 notifications/initialized 和（phase1 可选）mcp_ready 通知
pub async fn run_mcp_bridge() -> !;
```

**关键决策**：
- bridge **不重复**定义工具 descriptor：tools/list 直接转发到 TCP server 返回。
- bridge **不做 caller 身份判定**：只负责透传 + 在每条 TCP 请求里附 `auth_token` + `slot_id`（或 `from_slot_id`）。
- bridge 错误即退出（exit code 非零），ACP CLI 会把 MCP server 标为 broken，agent 继续跑只是 team_* 不可用（mcp.md §4.4 "稳定性保证 #3"）。

**phase1 范围**：mcp_ready 握手"简化"为"tcp 连接建立成功即认为 ready"，不强制 phase1 做完整握手 —— AionUi 侧 waitForMcpReady 超时 graceful resolve（aionui-audit §8 #11），所以后端 phase1 先不接 server 端等待，后续 P1 再补。

---

## 9. `aionui-team::service::TeamSessionService` 签名扩展（Wave 2 · 模块 D9）

**文件**：`crates/aionui-team/src/service.rs`

```rust
pub struct TeamSessionService {
    repo: Arc<dyn ITeamRepository>,
    conversation_service: ConversationService,
    broadcaster: Arc<dyn EventBroadcaster>,
    // 新增
    task_manager: Arc<dyn IWorkerTaskManager>,
    backend_binary_path: Arc<PathBuf>,
    sessions: DashMap<String, TeamSession>,
}

impl TeamSessionService {
    pub fn new(
        repo: Arc<dyn ITeamRepository>,
        conversation_service: ConversationService,
        broadcaster: Arc<dyn EventBroadcaster>,
        task_manager: Arc<dyn IWorkerTaskManager>,         // 新增
        backend_binary_path: Arc<PathBuf>,                 // 新增
    ) -> Self;

    /// 改动：
    /// 1. MCP server 启动后，遍历 agents 把 stdio_spec 落到 conversation.extra.team_mcp_stdio_config
    ///    （通过调用 `conversation_service.update_extra(conv_id, patch)` —— **【新增接口】**，
    ///    ConversationService 需为此新增一个 `update_extra` 公开方法；extra 列是 JSON 字符串，不需 schema 迁移）
    /// 2. task_manager.kill(conv_id, Some(TeamSessionRefresh)) 然后 get_or_build_task 重建
    /// 3. 全部成功才 sessions.insert；失败时 session.stop() 且不 insert
    pub async fn ensure_session(&self, team_id: &str) -> Result<(), TeamError>;

    /// 改动：写完 mailbox 后调 session.compute_wake_input → task_manager.send_message
    pub async fn send_message(&self, team_id: &str, content: &str) -> Result<(), TeamError>;
    pub async fn send_message_to_agent(&self, team_id: &str, slot_id: &str, content: &str)
        -> Result<(), TeamError>;
}
```

**Finish 事件订阅**：`ensure_session` 成功后启动一个后台 task（或共享的事件聚合器），订阅 `task_manager.get_task(conv_id).subscribe()`，过滤 `AgentStreamEvent::Finish` → 调 `session.on_agent_finish(conv_id, is_error)`。

**订阅任务生命周期**：随 `session` 进入 `sessions`，由 `stop_session` 负责 abort。

---

## 10. `aionui-app::state_builders::build_team_state` 扩展（Wave 2 · 模块 D11）

**文件**：`crates/aionui-app/src/state_builders.rs:304`

```rust
pub fn build_team_state(
    services: &AppServices,
    cron_service: Option<Arc<aionui_cron::service::CronService>>,
    backend_binary_path: Arc<PathBuf>,   // 新增
) -> TeamRouterState {
    // ...
    let service = Arc::new(TeamSessionService::new(
        team_repo,
        conv_service,
        services.event_bus.clone(),
        services.worker_task_manager.clone(),   // 新增
        backend_binary_path,                     // 新增
    ));
    TeamRouterState { service }
}
```

`backend_binary_path` 在 `aionui_app::lib::build_router` 一次性 `Arc::new(std::env::current_exe()?)` 后 clone 到各子 builder。

---

## 11. ConversationService 不改（关键决策）

**phase1 决定不改 `aionui-conversation::service::build_task_options`**。

**理由**（backend-audit §4.5 "备选"）：
- team 有自己的 send 路径（`TeamSessionService.send_message_to_agent`），**phase1 先不复用 `POST /api/conversations/{id}/messages`**
- `ensure_session` 已经把 `team_mcp_stdio_config` 落到 `conversation.extra`，factory 反序列化 `AcpBuildExtra` 自然带上该字段 —— conversation service 无需感知 team
- 避免下游 crate 依赖上游

**影响**：phase1 的 smoke test 只通过 `POST /api/teams/{id}/messages` 入口（见 [README.md §3](./README.md)）。`POST /api/conversations/{id}/messages` 对 team 成员 conversation 的行为 phase1 **未定义**（Wave 2 之后再决策）。

---

## 12.5 `remove_team` 级联 kill agent 进程（Wave 2 · 模块 D11.5）

**文件**：`crates/aionui-team/src/service.rs`

```rust
impl TeamSessionService {
    pub async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), TeamError> {
        // ...existing: stop_session + delete conversations + delete mailbox/tasks/team...

        // 【新增】在 stop_session 之后、delete conversations 之前：
        // 遍历 team.agents，对每个 agent 调 task_manager.kill
        for agent in &team.agents {
            let _ = self.task_manager.kill(
                &agent.conversation_id,
                Some(AgentKillReason::TeamDeleted),
            );
        }
    }
}
```

**前置**：`TeamSessionService` 已持有 `task_manager`（由 D9 注入）。

**测试**：1 条集成测试——建 team → ensure_session → remove_team → 断言 `task_manager.active_count()` 减少对应数量。

---

## 12. 冻结的跨模块调用矩阵

| Wave | 模块 | 读（依赖） | 写（调用） |
|------|------|-----------|-----------|
| 1 | D1 team_mcp types | — | — |
| 1 | D2 AcpBuildExtra | D1 | — |
| 1 | D3 bridge spec | D1 | — |
| 1 | D4 两个新工具 | — | — |
| 1 | D5 prompts 模板 | types | — |
| 1 | D6 mcp-bridge 子命令 | D1 常量 | — |
| 2 | D7 TeamSession 新方法 + send 路径接 wake | D3/D5/D8 | task_manager.send_message |
| 2 | D8 scheduler 签名扩展（§6.5） | D5/D7 | mailbox.write (idle_notification) |
| 2 | D9 TeamSessionService.ensure_session | D7 + IWorkerTaskManager + D3 | conversation_service.update_extra **【新增接口】** |
| 2 | D10 session_new 注入 | D2/D3 | — |
| 2 | D11 app 装配 | 全部 | — |
| 3 | W3-D12 user-scope 过滤 | auth middleware | repo 查询附加 user_id 过滤 |
| 3 | W3-D13 get_team agent 修复 | conversation_repo | service.update（回写 agents） |
| 3 | W3-D14 rename 规范化 | scheduler 内存 | scheduler.rename_agent 改造 + prompt builder 读 renamed_agents |
| 3 | W3-D15 conversation 复用 | conversation_service | conversation_service.update_extra |
| 3 | W3-D16 send_message 识别 team_id | ConversationRow.extra + ITeamMessageRouter | team_router.route_agent_message（新 trait） |
| 3 | W3-D17 MCP 帧 + 超时 | common 常量 | tokio::time::timeout 外层包裹 |
| 4 | W4-D25 stream chunk 底座 | AcpAgentManager 内部 stream | broadcast::Sender 覆盖全部 chunk |
| 4 | W4-D18 active_wakes + wake_timeouts | W4-D25 订阅 | scheduler 内部 state |
| 4 | W4-D19 finalized_turns | 无新依赖 | scheduler 内部 state |
| 4 | W4-D20 crash recovery | W4-D25 订阅 + W4-D18 锁 | task_manager.kill + mailbox.write + wake |
| 4 | W4-D21 429 识别 | W4-D25 订阅 | set_status(Failed) |
| 4 | W4-D22 inactivity watchdog | W4-D18 timer | mailbox.write(idle_notification) + wake |
| 4 | W4-D23 add_agent_locks | 无 | service.add_agent 外层 lock |
| 4 | W4-D24 mcp_ready 握手 | D6 bridge + mcp.protocol | server.notify_mcp_ready + wait_for_mcp_ready |
| 5 | W5-D26 Guide MCP server | W3-D15 conversation 复用 + D4 tool descriptors | TeamSessionService.create_team |
| 5 | W5-D27 Guide bridge 分支 | D6 主 bridge | 无 |
| 5 | W5-D28 Guide prompt 注入 | D5a + D2 + W5-D26 | AcpAgentManager instructions |
| 5 | W5-D29 真实 spawn | W3-D14/D15 + W4-D18/D23 + D3 bridge spec | TeamSessionService.add_agent + task_manager.kill/get_or_build_task + wake |
| 5 | W5-D30 真 kill + shutdown 协议 | W5-D29 + W4-D18/D19 | task_manager.kill + 清 scheduler 内部 state |
| 5 | W5-D31 WS 事件 | 全 Wave 5 生命周期点 | broadcaster.broadcast |

**冻结规则**：签名在对应 Wave 开工前 merge；后续 Wave 任何模块想改前一 Wave 已冻签名必须先开 issue 让 leader 裁决。

---

## 13. User-scope 过滤（Wave 3 · 模块 W3-D12）

**文件**：`crates/aionui-team/src/service.rs`

```rust
impl TeamSessionService {
    /// 改动：入参增加 user_id；repo 侧查询 WHERE user_id = ?
    pub async fn list_teams(&self, user_id: &str) -> Result<Vec<Team>, TeamError>;

    /// 改动：不属于 user_id 的 team 返回 AppError::NotFound（不区分"存在但无权"）
    pub async fn get_team(&self, user_id: &str, team_id: &str) -> Result<Team, TeamError>;

    /// 改动：归属校验失败 NotFound
    pub async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), TeamError>;
}
```

**Repository trait 扩展**：

```rust
// crates/aionui-db/src/repository/team.rs
pub trait ITeamRepository {
    // 改动：入参加 user_id（老签名改为 user_id 过滤，无默认 null 分支）
    fn list_by_user(&self, user_id: &str) -> Result<Vec<Team>, AppError>;
    fn find_by_id_and_user(&self, team_id: &str, user_id: &str) -> Result<Option<Team>, AppError>;
    // ...delete / update 同理
}
```

**Routes 改动**：`crates/aionui-team/src/routes.rs` 所有 handler 从 `Extension<AuthUser>` 取 `user_id` 传给 service。

**错误语义**：phase1 不暴露"存在但无权"差异（避免枚举型泄漏），越权访问一律 NotFound。

---

## 14. `get_team` agent 修复（Wave 3 · 模块 W3-D13）

**文件**：`crates/aionui-team/src/service.rs`

```rust
impl TeamSessionService {
    pub async fn get_team(&self, user_id: &str, team_id: &str) -> Result<Team, TeamError> {
        let mut team = self.repo.find_by_id_and_user(team_id, user_id).await?
            .ok_or(TeamError::NotFound)?;
        if team.agents.is_empty() {
            self.repair_team_agents_if_missing(&mut team, user_id).await?;
        }
        Ok(team)
    }

    /// 按 conversation.extra.team_id 反推 agents 并回写
    async fn repair_team_agents_if_missing(
        &self,
        team: &mut Team,
        user_id: &str,
    ) -> Result<(), TeamError>;
}
```

**Conversation 侧 trait 要求**（若已有则复用，否则 Wave 3 顺带加）：

```rust
pub trait IConversationRepository {
    /// 按 extra.team_id JSON 字段查询属于指定 team 的 conversations
    /// 实现用 JSON1 扩展：WHERE json_extract(extra, '$.team_id') = ?
    async fn list_by_team_id(&self, team_id: &str, user_id: &str) -> Result<Vec<ConversationRow>, AppError>;
}
```

**反推规则**：
- `slot_id` 取 `extra.team_mcp_stdio_config.slot_id`（W2 已写入）；无则取 `extra.slot_id`（兜底）
- `role` 第一个命中的 conversation 假定为 Lead（按 created_at asc 排序），其余 Teammate
- 反推后 `repo.update(team, {agents, updated_at})` 持久化

---

## 15. `rename_agent` 规范化（Wave 3 · 模块 W3-D14）

**文件**：`crates/aionui-team/src/scheduler.rs`

```rust
impl TeammateManager {
    pub async fn rename_agent(
        &self,
        slot_id: &str,
        new_name: &str,
    ) -> Result<RenameResult, TeamError>;
}

pub struct RenameResult {
    pub old_name: String,
    pub new_name: String,
    pub normalized: String,  // 给日志/事件使用
}
```

**规范化函数**（内部）：

```rust
fn normalize_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .to_lowercase()
}
```

**冲突检查**：如果对除自己外任何 agent 的 `normalize_name(agent.name) == normalize_name(new_name)` → `Err(TeamError::NameConflict)`。

**`renamed_agents` 内存结构**（TeammateManager 新增字段）：

```rust
/// slot_id -> 最早的 original name（首次改名前的名字）
renamed_agents: Mutex<HashMap<String, String>>,
```

**原则**：仅当 `renamed_agents.get(slot_id).is_none()` 时写入（只存首次 original name）。

**Prompt builder 对齐**（D5b-2 / D5c 已经预留参数）：Lead / Teammate prompt 在渲染 `## Your Teammates` 时读 `renamed_agents.get(slot_id)`，存在即追加 ` [formerly: <original>]`。

---

## 16. Conversation 复用（Wave 3 · 模块 W3-D15）

**文件**：`crates/aionui-api-types/src/team.rs`

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    pub role: TeammateRole,
    pub backend: String,
    pub model: String,
    // ...既有字段...

    /// 新增：复用已有单聊会话作为 agent 的 conversation
    #[serde(default)]
    pub conversation_id: Option<String>,
}
```

**文件**：`crates/aionui-team/src/service.rs`

```rust
impl TeamSessionService {
    pub async fn create_team(
        &self,
        user_id: &str,
        req: CreateTeamRequest,
    ) -> Result<Team, TeamError> {
        // ...既有校验...
        for (i, agent_req) in req.agents.iter().enumerate() {
            let conv_id = match &agent_req.conversation_id {
                Some(existing_id) => {
                    let conv = self.conversation_service
                        .get(existing_id, user_id).await?
                        .ok_or(TeamError::NotFound)?;
                    // 归属校验：conv.user_id == user_id（get 内部做）
                    // 冲突校验：conv.extra.team_id 不存在或等于当前 team_id
                    self.conversation_service.update_extra(
                        existing_id,
                        json!({"team_id": team_id}),
                    ).await?;
                    existing_id.clone()
                }
                None => {
                    self.conversation_service.create(
                        /* 原有 build_conversation_params 产出 */
                    ).await?.id
                }
            };
            // ...把 (slot_id, conv_id) 挂到 team.agents...
        }
    }
}
```

**错误语义**：
- `conversation_id` 不存在 → `NotFound`
- `conversation_id` 属于别的 user → `NotFound`（越权不暴露）
- `conversation_id` 的 `extra.team_id` 已经等于别的 team_id → `BadRequest("conversation already belongs to another team")`

---

## 17. `ConversationService.send_message` 识别 `team_id`（Wave 3 · 模块 W3-D16）

**新 trait**：放 `aionui-conversation` 里（避免 `aionui-conversation` 反向依赖 `aionui-team`）：

```rust
// crates/aionui-conversation/src/service.rs（或新 traits.rs）
#[async_trait]
pub trait ITeamMessageRouter: Send + Sync {
    /// 当 ConversationService 发现 conv 的 extra.team_id 非空时调用
    /// 实现方（TeamSessionService）负责：按 conv_id 反查 slot_id → 调 session.send_message_to_agent
    async fn route_agent_message(
        &self,
        conversation_id: &str,
        content: &str,
        silent: bool,
    ) -> Result<(), AppError>;
}
```

**ConversationService 改动**：

```rust
pub struct ConversationService {
    // ...既有字段...
    /// 可选：注入后才启用 team 路由
    team_router: Option<Arc<dyn ITeamMessageRouter>>,
}

impl ConversationService {
    pub async fn send_message(&self, req: SendMessageRequest) -> Result<..., AppError> {
        let row = self.repo.get(&req.conversation_id).await?;
        let extra: serde_json::Value = serde_json::from_str(&row.extra)?;
        if let Some(team_id) = extra.get("team_id").and_then(|v| v.as_str()) {
            if let Some(router) = &self.team_router {
                return router.route_agent_message(&row.id, &req.content, false).await
                    .map_err(Into::into);
            } else {
                tracing::warn!(
                    "conversation {} has team_id={} but team_router not injected; falling back to solo path",
                    row.id, team_id
                );
            }
        }
        // ...原有单聊路径...
    }
}
```

**TeamSessionService 实现**：

```rust
#[async_trait]
impl ITeamMessageRouter for TeamSessionService {
    async fn route_agent_message(&self, conv_id: &str, content: &str, silent: bool) -> Result<(), AppError> {
        let session = /* 按 conv_id 找 session，即读 conv.extra.team_id → self.sessions.get(team_id) */;
        let slot_id = session.slot_id_of(conv_id).ok_or(...)?;
        session.send_message_to_agent(&slot_id, content, silent).await?;
        Ok(())
    }
}
```

**`TeamSession::slot_id_of`**：W2 D7 已经预留占位；W3-D16 实现（简单 `self.team.agents.iter().find(|a| a.conversation_id == conv_id)`）。

**装配**：`build_app_services` 构造 `ConversationService` 时注入 `team_router = Some(team_session_service.clone() as Arc<dyn ITeamMessageRouter>)`。

---

## 18. MCP 帧大小 + 300s 超时（Wave 3 · 模块 W3-D17）

**文件**：`crates/aionui-common/src/lib.rs`

```rust
pub const TEAM_MCP_REQUEST_TIMEOUT_MS: u64 = 300_000;
pub const TEAM_MCP_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
```

**文件**：`crates/aionui-team/src/mcp/protocol.rs`

```rust
// 原：const MAX_MCP_MESSAGE_SIZE: usize = 10 * 1024 * 1024;
// 改：
pub const MAX_MCP_MESSAGE_SIZE: usize = aionui_common::TEAM_MCP_MAX_FRAME_BYTES;
```

**文件**：`crates/aionui-team/src/mcp/server.rs`

```rust
// dispatch_tool 外层：
let result = tokio::time::timeout(
    std::time::Duration::from_millis(aionui_common::TEAM_MCP_REQUEST_TIMEOUT_MS),
    dispatch_tool(...),
).await;

match result {
    Ok(r) => r,
    Err(_) => Err(JsonRpcError::internal("Request timeout")),
}
```

---

## 19. AgentStream chunk 订阅底座（Wave 4 · 模块 W4-D25）

**文件**：`crates/aionui-ai-agent/src/types.rs`

```rust
#[derive(Debug, Clone)]
pub enum AgentStreamChunk {
    Text { text: String },
    ToolUse { tool_name: String, input: serde_json::Value },
    Thought { content: String },
    Finish { agent_crash: bool, stop_reason: Option<String> },
    Error { message: String },
}
```

**文件**：`crates/aionui-ai-agent/src/task_manager.rs`

```rust
pub trait AgentManagerHandle: Send + Sync {
    // ...既有...

    /// 订阅当前 session 的全部 stream chunk（新起点才能收）
    /// broadcast 通道：lagged 的订阅者会 skip 旧消息，不影响其他订阅者
    fn subscribe_stream(&self) -> tokio::sync::broadcast::Receiver<AgentStreamChunk>;
}
```

**文件**：`crates/aionui-ai-agent/src/acp_agent.rs`

```rust
pub struct AcpAgentManager {
    // ...既有...
    stream_tx: tokio::sync::broadcast::Sender<AgentStreamChunk>,
}

impl AcpAgentManager {
    pub fn new(...) -> Self {
        let (stream_tx, _) = tokio::sync::broadcast::channel(256);
        // ...
    }
}

impl AgentManagerHandle for AcpAgentManager {
    fn subscribe_stream(&self) -> broadcast::Receiver<AgentStreamChunk> {
        self.stream_tx.subscribe()
    }
}

// 在处理 ACP ChatUpdate / Finish / Error 的地方统一：
// 原来只 emit Finish，现在全部 emit
let _ = self.stream_tx.send(AgentStreamChunk::Text { text });
let _ = self.stream_tx.send(AgentStreamChunk::ToolUse { ... });
let _ = self.stream_tx.send(AgentStreamChunk::Finish { agent_crash: false, stop_reason: ... });
```

**broadcast channel 大小**：256 足够 agent stream 高峰（AionUi 参考按 in-process 事件 bus 无上限，后端用 broadcast 限定避免 OOM）。

---

## 20. `active_wakes` + `wake_timeouts`（Wave 4 · 模块 W4-D18）

**文件**：`crates/aionui-team/src/scheduler.rs`

```rust
pub struct TeammateManager {
    // ...既有...
    active_wakes: DashSet<String>,             // slot_id 正在 wake
    wake_timeouts: DashMap<String, JoinHandle<()>>,  // slot_id -> inactivity timer
}

impl TeammateManager {
    /// 返回 true 表示成功获得锁；false 表示已有并发 wake 在跑
    pub fn try_acquire_wake_lock(&self, slot_id: &str) -> bool {
        self.active_wakes.insert(slot_id.to_string())
    }

    /// 消息发出成功后立即调用（不等 finish）—— aionui-audit §8 #2
    pub fn release_wake_lock(&self, slot_id: &str) {
        self.active_wakes.remove(slot_id);
    }

    /// wake 消息发出后启动 60s inactivity timer
    /// 订阅 W4-D25 的 stream，任何 chunk 都 reset；Finish 清除
    pub fn arm_wake_timeout(
        self: Arc<Self>,
        slot_id: String,
        stream_rx: broadcast::Receiver<AgentStreamChunk>,
    );

    pub fn clear_wake_timeout(&self, slot_id: &str) {
        if let Some((_, handle)) = self.wake_timeouts.remove(slot_id) {
            handle.abort();
        }
    }
}
```

**`arm_wake_timeout` 伪码**：

```rust
let handle = tokio::spawn(async move {
    let mut deadline = Instant::now() + Duration::from_secs(60);
    loop {
        tokio::select! {
            chunk = stream_rx.recv() => {
                match chunk {
                    Ok(AgentStreamChunk::Finish { .. }) => return,
                    Ok(_) => { deadline = Instant::now() + Duration::from_secs(60); }
                    Err(_) => return,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                scheduler.handle_inactivity_timeout(&slot_id).await;
                return;
            }
        }
    }
});
self.wake_timeouts.insert(slot_id, handle);
```

**与 W3-D14 等 Wave 3 rename / remove 模块协同**：agent remove 时必须 `active_wakes.remove()` + `wake_timeouts.remove()`（由 W5-D30 的 `remove_agent` 改造负责）。

---

## 21. `finalized_turns` 5s dedup（Wave 4 · 模块 W4-D19）

**文件**：`crates/aionui-team/src/scheduler.rs`

```rust
pub struct TeammateManager {
    // ...
    finalized_turns: DashMap<String, Instant>,  // conversation_id -> 最后 finalize 时间
}

impl TeammateManager {
    /// 返回 true 表示可以继续处理；false 表示被 dedup 拦截
    pub fn begin_finalize(&self, conversation_id: &str) -> bool {
        let now = Instant::now();
        let should_proceed = match self.finalized_turns.get(conversation_id) {
            Some(entry) if now.duration_since(*entry) < Duration::from_secs(5) => false,
            _ => true,
        };
        if should_proceed {
            self.finalized_turns.insert(conversation_id.to_string(), now);
            // 5s 后清理（避免 DashMap 无限增长）
            let map = self.finalized_turns.clone();
            let key = conversation_id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                map.remove(&key);
            });
        }
        should_proceed
    }

    /// re-wake 前调用（W4-D18 的 try_acquire_wake_lock 成功后立即）
    /// aionui-audit §8 #3：不清这个 dedup 会吞掉新 turn 的 finish
    pub fn clear_finalized_turn(&self, conversation_id: &str) {
        self.finalized_turns.remove(conversation_id);
    }
}
```

**集成点**：
- `TeamSession::on_agent_finish(conv_id, is_error)` 第一步 `if !scheduler.begin_finalize(conv_id) { return; }`
- W4-D18 的 wake 成功发出消息后 `scheduler.clear_finalized_turn(conv_id)`

---

## 22. Crash recovery（Wave 4 · 模块 W4-D20）

**文件**：`crates/aionui-team/src/session.rs`

```rust
impl TeamSession {
    pub async fn on_agent_finish(
        &self,
        conversation_id: &str,
        chunk: AgentStreamChunk,  // 改为接受 chunk 而非仅 is_error（W4-D25 提供全部 chunk）
    ) -> Result<(), TeamError> {
        let slot_id = self.slot_id_of(conversation_id).ok_or(...)?;

        let crash_reason = detect_crash(&chunk);
        if let Some(reason) = crash_reason {
            return self.scheduler.handle_agent_crash(&slot_id, conversation_id, reason).await;
        }

        if let AgentStreamChunk::Error { message } = &chunk {
            if aionui_common::RATE_LIMIT_REGEX.is_match(message) {
                self.scheduler.set_status(&slot_id, TeammateStatus::Failed).await;
                return Ok(());
            }
        }

        // 正常 finish
        if !self.scheduler.begin_finalize(conversation_id) { return Ok(()); }
        self.scheduler.finalize_turn(&slot_id, vec![]).await?;
        Ok(())
    }
}

fn detect_crash(chunk: &AgentStreamChunk) -> Option<CrashReason> {
    match chunk {
        AgentStreamChunk::Finish { agent_crash: true, .. } => Some(CrashReason::AgentCrash),
        AgentStreamChunk::Error { message }
            if message.contains("process exited unexpectedly")
            || message.contains("Session not found") => Some(CrashReason::ProcessExited),
        _ => None,
    }
}
```

**文件**：`crates/aionui-team/src/scheduler.rs`

```rust
impl TeammateManager {
    pub async fn handle_agent_crash(
        &self,
        slot_id: &str,
        conversation_id: &str,
        reason: CrashReason,
    ) -> Result<(), TeamError> {
        let agent = self.get_agent(slot_id).ok_or(...)?;

        if agent.role == TeammateRole::Lead {
            // leader crash：只 failed，不 remove，不 wake 其他
            self.set_status(slot_id, TeammateStatus::Failed).await;
            return Ok(());
        }

        // 非 leader crash：testament + kill + failed + wake leader
        let lead_slot_id = self.find_lead_slot_id().ok_or(...)?;
        let testament = format!(
            "Teammate '{}' crashed during task (reason: {:?}). Please investigate.",
            agent.name, reason,
        );
        self.mailbox.write(
            &self.team_id, &lead_slot_id, slot_id,
            MailboxMessageType::Message, &testament, None,
        ).await?;
        self.task_manager.kill(conversation_id, Some(AgentKillReason::Crash))?;
        self.set_status(slot_id, TeammateStatus::Failed).await;
        self.clear_wake_timeout(slot_id);
        self.release_wake_lock(slot_id);
        self.wake(&lead_slot_id).await?;
        Ok(())
    }
}
```

**新 enum**：

```rust
#[derive(Debug, Clone, Copy)]
pub enum CrashReason { AgentCrash, ProcessExited, SessionNotFound }
```

---

## 23. 429 / rate-limit 识别（Wave 4 · 模块 W4-D21）

**文件**：`crates/aionui-common/src/lib.rs`

```rust
use once_cell::sync::Lazy;
use regex::Regex;

pub static RATE_LIMIT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)429|rate.?limit|quota|too many requests").unwrap()
});
```

**集成点**：见 §22 `on_agent_finish` 的 Error chunk 分支。429 不走 crash recovery（不 kill、不 testament），只 `set_status(Failed)`。

---

## 24. Inactivity watchdog（Wave 4 · 模块 W4-D22）

**文件**：`crates/aionui-team/src/scheduler.rs`

```rust
impl TeammateManager {
    /// 由 W4-D18 的 arm_wake_timeout 在 60s 无 chunk 时调用
    pub async fn handle_inactivity_timeout(&self, slot_id: &str) -> Result<(), TeamError> {
        let agent = self.get_agent(slot_id).ok_or(...)?;
        self.set_status(slot_id, TeammateStatus::Failed).await;
        self.release_wake_lock(slot_id);

        if agent.role == TeammateRole::Lead {
            // leader 自己 stuck：只 failed，不递归自通知
            return Ok(());
        }

        let lead_slot_id = self.find_lead_slot_id().ok_or(...)?;
        let content = format!(
            "Teammate '{}' is stuck (no stream activity for 60s). \
             Possible reasons: LLM stream stalled / Standing By loop / external tool hung. \
             Consider re-dispatching the task or removing this teammate.",
            agent.name,
        );
        self.mailbox.write(
            &self.team_id, &lead_slot_id, slot_id,
            MailboxMessageType::IdleNotification, &content, None,
        ).await?;
        self.wake(&lead_slot_id).await?;
        Ok(())
    }
}
```

---

## 25. `add_agent_locks` 串行化（Wave 4 · 模块 W4-D23）

**文件**：`crates/aionui-team/src/service.rs`

```rust
pub struct TeamSessionService {
    // ...
    add_agent_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl TeamSessionService {
    pub async fn add_agent(
        &self,
        user_id: &str,
        team_id: &str,
        agent_req: CreateAgentRequest,
    ) -> Result<TeamAgent, TeamError> {
        let lock = self.add_agent_locks
            .entry(team_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        // ...原有 read-modify-write...
    }

    pub async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), TeamError> {
        // ...原有逻辑...
        self.add_agent_locks.remove(team_id);  // 避免 lock leak
        Ok(())
    }
}
```

---

## 26. MCP `mcp_ready` 握手（Wave 4 · 模块 W4-D24）

**文件**：`crates/aionui-team/src/mcp/protocol.rs`

```rust
/// 新增 notification 类型（不是 JSON-RPC request，没有 id）
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum McpNotification {
    #[serde(rename = "mcp_ready")]
    McpReady {
        slot_id: String,
        auth_token: String,
    },
}
```

**文件**：`crates/aionui-team/src/mcp/server.rs`

```rust
pub struct TeamMcpServer {
    // ...
    ready_latch: DashSet<String>,              // slot_id 已发过 mcp_ready
    ready_notify: DashMap<String, Arc<tokio::sync::Notify>>,
}

impl TeamMcpServer {
    /// bridge 发来 mcp_ready 时调用
    fn notify_mcp_ready(&self, slot_id: &str) {
        self.ready_latch.insert(slot_id.to_string());
        if let Some(notify) = self.ready_notify.get(slot_id) {
            notify.notify_waiters();
        }
    }

    /// 供外部（TeamSessionService.ensure_session）等待 bridge 就绪
    /// Graceful：timeout 也返回 Ok(()) 不 Err
    pub async fn wait_for_mcp_ready(
        &self,
        slot_id: &str,
        timeout: std::time::Duration,
    ) -> Result<(), AppError> {
        if self.ready_latch.contains(slot_id) { return Ok(()); }

        let notify = self.ready_notify
            .entry(slot_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone();

        tokio::select! {
            _ = notify.notified() => Ok(()),
            _ = tokio::time::sleep(timeout) => {
                tracing::warn!("mcp_ready timeout for slot_id={}, degrading gracefully", slot_id);
                Ok(())  // aionui-audit §8 #11
            }
        }
    }
}
```

**文件**：`crates/aionui-app/src/bridge.rs`（D6 已实现 bridge 主逻辑；W4-D24 在 TCP connect 成功 + initialize ok 后追加）

```rust
// 成功 initialize 后 fire-and-forget
let notification = json!({
    "jsonrpc": "2.0",
    "method": "mcp_ready",
    "params": { "slot_id": slot_id, "auth_token": auth_token }
});
// 直接写一帧 notification（无 id）
let _ = write_frame(&mut tcp_stream, &notification.to_string()).await;
```

**集成点**：`TeamSessionService::ensure_session` 在 `kill + get_or_build_task` 之后调 `mcp_server.wait_for_mcp_ready(slot_id, 30s)`（必须在 insert 到 sessions 之前）。

---

## 27. Team Guide MCP server（Wave 5 · 模块 W5-D26）

**文件**：`crates/aionui-team/src/guide/server.rs`（新增）

```rust
pub struct GuideMcpServer {
    addr: SocketAddr,
    auth_token: String,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl GuideMcpServer {
    /// 单例启动入口：在 AppServices 构造时调用一次
    pub async fn start_singleton(
        team_session_service: Arc<TeamSessionService>,
        broadcaster: Arc<dyn EventBroadcaster>,
    ) -> Result<Arc<Self>, AppError>;

    /// 供 ACP agent 注入时读取
    pub fn stdio_config(&self, backend: &str, conversation_id: &str) -> GuideStdioConfig;

    /// app shutdown 时调用
    pub fn stop(self);
}

pub struct GuideStdioConfig {
    pub port: u16,
    pub token: String,
    pub backend: String,
    pub conversation_id: String,
}

impl GuideStdioConfig {
    pub const ENV_PORT: &'static str = "AION_MCP_PORT";
    pub const ENV_TOKEN: &'static str = "AION_MCP_TOKEN";
    pub const ENV_BACKEND: &'static str = "AION_MCP_BACKEND";
    pub const ENV_CONVERSATION_ID: &'static str = "AION_MCP_CONVERSATION_ID";
}
```

**文件**：`crates/aionui-team/src/guide/handlers.rs`

```rust
pub async fn handle_aion_create_team(
    service: &TeamSessionService,
    broadcaster: &dyn EventBroadcaster,
    args: &serde_json::Value,
    backend: &str,
    caller_conversation_id: &str,
) -> Result<ToolResult, AppError> {
    // 1. 解析 args: summary (required), name (optional), workspace (optional)
    // 2. 补全 workspace：若无，从 caller_conversation 的 extra.workspace 继承
    // 3. name 缺省：summary 前 5 词
    // 4. 调用 service.create_team，user_id="system_default_user"，agents=[{role:Lead, backend, conversation_id: Some(caller_conversation_id)}]
    // 5. 成功后 emit 三个 WS 事件：team.listChanged, conversation.listChanged, deepLink.received
    // 6. async 调用 service.ensure_session(team.id)；完成后 session.send_message_to_agent(lead_slot_id, summary, silent=true)
    // 7. 返回 {team_id, name, route:"/team/<id>", lead_agent, status:"team_created", next_step:"..."}
}

pub async fn handle_aion_list_models(
    args: &serde_json::Value,
) -> Result<ToolResult, AppError> {
    // 复用 Wave 1 D4 的 team_list_models handler（硬编码 backend × model 表）
}
```

**用户常量**：`const MCP_SPAWN_USER_ID: &str = "system_default_user";`（aionui-audit §8 #15，后端 multi-tenant 时替换）。
**MCP spawn 默认值**：`workspace_mode = "shared"`, `session_mode = "yolo"`（aionui-audit §8 #16）。

---

## 28. Guide stdio bridge 分支（Wave 5 · 模块 W5-D27）

**文件**：`crates/aionui-app/src/bridge.rs`（在 D6 已有的 `run_mcp_bridge()` 内部分叉）

```rust
pub async fn run_mcp_bridge() -> ! {
    if std::env::var(aionui_team::guide::GuideStdioConfig::ENV_BACKEND).is_ok() {
        run_guide_bridge().await
    } else {
        run_team_bridge().await
    }
}

async fn run_guide_bridge() -> ! {
    let port = env::var(ENV_PORT)?;
    let token = env::var(ENV_TOKEN)?;
    let backend = env::var(ENV_BACKEND)?;
    let conversation_id = env::var(ENV_CONVERSATION_ID)?;

    // 每条 tools/call 请求往 params 里追加 backend + conversation_id
    // 然后走 TCP 发给 GuideMcpServer
    // （Guide server 用这两个字段做业务判断，比如 aion_create_team 的 caller 复用）
}
```

**区别点**：team bridge 附 `slot_id`；guide bridge 附 `backend` + `conversation_id`。协议（4 字节长度 + JSON-RPC）完全一致。

---

## 29. Guide prompt 注入 + capability 判定（Wave 5 · 模块 W5-D28）

**文件**：`crates/aionui-team/src/guide/capability.rs`（新增）

```rust
const TEAM_CAPABLE_BACKENDS: &[&str] = &["claude", "codex", "gemini", "aionrs"];

pub fn is_team_capable_backend(backend: &str, mcp_stdio_capable: bool) -> bool {
    TEAM_CAPABLE_BACKENDS.contains(&backend) || mcp_stdio_capable
}
```

**文件**：`crates/aionui-ai-agent/src/acp_agent.rs`（instructions 构造点）

```rust
// 注入顺序：既有 preset_context + 新 Guide prompt + user messages
fn build_instructions(&self, config: &AcpBuildExtra, backend: &str) -> String {
    let mut parts = Vec::new();
    if let Some(preset) = &config.preset_context { parts.push(preset.clone()); }

    // 关键互斥：已在 team 里 → 不注入 Guide
    if config.team_mcp_stdio_config.is_none()
        && is_team_capable_backend(backend, self.backend_supports_mcp_stdio())
    {
        let guide_prompt = build_team_guide_prompt(&TeamGuidePromptParams {
            backend,
            leader_label: None,  // phase1 无 preset assistant
        });
        parts.push(guide_prompt);
    }
    parts.join("\n\n")
}

// mcp_servers 注入处同样 guard：
if config.team_mcp_stdio_config.is_none()
    && is_team_capable_backend(backend, ...)
{
    let guide_cfg = guide_server.stdio_config(backend, &conversation_id);
    mcp_servers.push(build_guide_server_spec(&guide_cfg).into_sdk());
}
```

---

## 30. `team_spawn_agent` 真实落地（Wave 5 · 模块 W5-D29）

**文件**：`crates/aionui-team/src/session.rs`

```rust
impl TeamSession {
    /// MCP spawn 闭环（AionUi TeamSessionService.ts:763-787 等价）
    pub async fn spawn_agent(
        &self,
        caller_slot_id: &str,
        req: SpawnAgentRequest,
    ) -> Result<TeamAgent, TeamError>;
}

#[derive(Debug, Clone)]
pub struct SpawnAgentRequest {
    pub name: String,
    pub agent_type: Option<String>,       // backend
    pub custom_agent_id: Option<String>,  // phase1 忽略（无 preset 体系）
    pub model: Option<String>,
}
```

**实现步骤**（对应 modules.md §9 W5-D29 职责 a..g）：
1. 校验 caller 是 Lead（scheduler `TeamMcpServer::handle_spawn_agent` 已有）
2. `normalize_name(req.name)` 不冲突（复用 W3-D14）
3. `agent_type` 默认继承 caller_agent.backend；校验在 `SPAWN_BACKEND_WHITELIST`
4. 构造 `CreateAgentRequest`（含 `conversation_id: None` 让 service 新建）
5. 调 `self.service.add_agent(user_id, team_id, req)`（持 W4-D23 的 add_agent_locks） → 返回 new `TeamAgent` 含 slot_id + conv_id
6. `session.register_agent(new_agent)` 更新内存 slots
7. `stdio_spec = session.stdio_spec(new_slot_id)` → `update_extra(new_conv_id, {team_mcp_stdio_config})`
8. `task_manager.kill(new_conv_id, Some(TeamSpawn))` + `task_manager.get_or_build_task(new_conv_id, opts)`
9. mailbox.write(from=caller, to=new_slot_id, Message, "You have been spawned as <name>...")
10. `wake(new_slot_id)` 触发首次 role prompt 注入（经 W2 D7 的 compute_wake_input → W4-D18 锁 → send_message）
11. emit WS `team.listChanged{action: 'agent_added'}` + `team.agentSpawned`

**错误回滚**：任一步失败需回滚前面的副作用（agent remove + conversation delete + task kill）；phase1 最小实现：若步 9/10 失败只 log + set_status(Failed) 不回滚 agents 数组（AionUi 参考实现也不回滚）。

---

## 31. `team_shutdown_agent` 闭环（Wave 5 · 模块 W5-D30）

**文件**：`crates/aionui-team/src/mcp/server.rs`

```rust
fn handle_send_message(
    &self,
    caller_slot_id: &str,
    args: &serde_json::Value,
) -> Result<ToolResult, AppError> {
    let to = /*...*/;
    let message = /*...*/;

    // shutdown 协议拦截（aionui-audit §2.1 shutdown）
    if message.trim() == "shutdown_approved" {
        return self.handle_shutdown_approved(caller_slot_id).await;
    }
    if let Some(reason) = message.strip_prefix("shutdown_rejected:") {
        return self.handle_shutdown_rejected(caller_slot_id, reason.trim()).await;
    }

    // 普通消息走原有路径
    /* ...既有... */
}
```

**文件**：`crates/aionui-team/src/scheduler.rs`

```rust
impl TeammateManager {
    /// Wave 5 改造：真 kill + 清内部 state
    pub async fn remove_agent(&self, slot_id: &str) -> Result<(), TeamError> {
        let agent = self.get_agent(slot_id).ok_or(TeamError::NotFound)?;

        if agent.role == TeammateRole::Lead {
            tracing::warn!("refusing to remove leader (slot_id={})", slot_id);
            return Err(TeamError::LeaderImmutable);
        }

        // 真 kill agent 进程
        self.task_manager.kill(&agent.conversation_id, Some(AgentKillReason::Shutdown))?;

        // 清内部 state（W4-D18 / W4-D19）
        self.active_wakes.remove(slot_id);
        self.clear_wake_timeout(slot_id);
        self.finalized_turns.remove(&agent.conversation_id);

        // 移除 slot
        self.slots.lock().remove(slot_id);

        // 广播事件
        self.events.broadcast(WsEvent::TeamAgentRemoved { team_id: self.team_id.clone(), slot_id: slot_id.to_string() });
        Ok(())
    }

    /// Wave 5 改造：target role 校验（aionui-audit §2.1 "Leader 不可 shutdown"）
    pub async fn shutdown_agent(&self, caller_slot_id: &str, target: &str) -> Result<(), TeamError> {
        let caller = self.get_agent(caller_slot_id).ok_or(...)?;
        if caller.role != TeammateRole::Lead { return Err(TeamError::LeaderOnly); }

        let target_agent = /* resolve name or slot_id */;
        if target_agent.role == TeammateRole::Lead {
            return Err(TeamError::CannotShutdownLeader);
        }

        // 写 shutdown_request 到 target mailbox
        self.mailbox.write(
            &self.team_id, &target_agent.slot_id, caller_slot_id,
            MailboxMessageType::ShutdownRequest,
            "...Reply \"shutdown_approved\" to confirm, or \"shutdown_rejected: <reason>\" to decline.",
            None,
        ).await?;
        self.wake(&target_agent.slot_id).await?;
        Ok(())
    }
}
```

**`handle_shutdown_approved`**：
```rust
// caller 就是 teammate 自己（from_slot_id）
scheduler.remove_agent(caller_slot_id).await?;
let lead = scheduler.find_lead_slot_id().ok_or(...)?;
mailbox.write(to=lead, content=format!("Teammate '{}' has been removed (shutdown approved).", name));
scheduler.wake(&lead);
```

**`handle_shutdown_rejected`**：
```rust
let lead = scheduler.find_lead_slot_id().ok_or(...)?;
mailbox.write(to=lead, content=format!("Teammate '{}' declined shutdown: {}", name, reason));
scheduler.wake(&lead);
```

---

## 32. `team.mcpStatus` + `teammate_message` WS（Wave 5 · 模块 W5-D31）

**文件**：`crates/aionui-api-types/src/team.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMcpPhase {
    TcpReady,
    TcpError,
    SessionInjecting,
    SessionReady,
    SessionError,
    LoadFailed,
    Degraded,
    ConfigWriteFailed,
    McpToolsWaiting,
    McpToolsReady,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMcpStatusPayload {
    pub team_id: String,
    pub slot_id: Option<String>,
    pub phase: TeamMcpPhase,
    pub port: Option<u16>,
    pub server_count: Option<usize>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeammateMessagePayload {
    pub conversation_id: String,
    pub content: String,
    pub from_slot_id: String,
    pub from_name: String,
}
```

**事件发出点分布**（phase 生命周期）：

| Phase | 发出点 | 模块 |
|-------|--------|------|
| `tcp_ready` | `TeamMcpServer::start` 成功 bind | W5-D31 |
| `tcp_error` | `TeamMcpServer::start` 绑定失败 | W5-D31 |
| `session_injecting` | `ensure_session` 开始遍历 agents | W5-D31 |
| `session_ready` | `ensure_session` 成功 insert sessions | W5-D31 |
| `session_error` | `ensure_session` 失败 stop MCP | W5-D31 |
| `load_failed` | `get_or_build_task` 失败 | W5-D31 |
| `degraded` | `wait_for_mcp_ready` timeout | W5-D31（hook W4-D24） |
| `config_write_failed` | `update_extra` 失败 | W5-D31 |
| `mcp_tools_waiting` | bridge connect 前 | W5-D31 |
| `mcp_tools_ready` | bridge 发送 `mcp_ready` 成功 | W5-D31 |

**`teammate_message` 发出点**：

```rust
// TeamSession::compute_wake_input 内，teammate 场景下
// （Lead 不 emit，因为消息已在 prompt 里）
if agent.role != TeammateRole::Lead {
    for msg in &unread_messages {
        let from_name = agents.iter()
            .find(|a| a.slot_id == msg.from_agent_id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "User".to_string());
        self.broadcaster.broadcast(WsEvent::ConversationResponseStream {
            event_type: "teammate_message".to_string(),
            conversation_id: agent.conversation_id.clone(),
            content: msg.content.clone(),
            from_slot_id: msg.from_agent_id.clone(),
            from_name,
        });
    }
}
```

---

## 33. Wave 3/4/5 接口冻结规则

- Wave 3 的 §13–§18 在 Wave 2 merge（M4）后立刻冻结；各子模块拿自己对应 section 开工
- Wave 4 的 §19–§26 在 Wave 3 merge（M5）前同步冻结，允许提前参考
- Wave 5 的 §27–§32 在 Wave 4 完成核心 D18/D25（可靠性 + chunk 订阅）后冻结
- 任何 Wave 的模块想改前一 Wave 已冻签名 → 先开 issue → leader 裁决 → 更新本文档 → 通知所有受影响模块的开发者

**冻结状态**：phase1 规划产出 = 本文档完整写成 = Wave 1/2 + Wave 3/4/5 契约全量冻结（2026-04-29）。后续若发现签名问题视为 Phase1 规划 bug，走 leader 重审流程。
