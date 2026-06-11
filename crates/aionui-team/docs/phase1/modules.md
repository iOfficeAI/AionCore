# Phase1 模块拆解（全量 5 波）

> **范围**：phase1 = **完整调研 + 全量开发计划**。本文档覆盖 5 波（Wave 1 → Wave 5）共 **87 个模块**。Wave 1/2 = 最小闭环；Wave 3/4/5 = 规范化 / 鲁棒性 / 业务闭环补全。
>
> **硬约束**（team-lead 指令）：
> 1. 一人一模块，每模块不超 200 行代码（例外模块见 [README.md §1](./README.md#1-硬约束不可违反)）
> 2. Wave 先拆纯净非业务（Wave 1），再拆业务串接（Wave 2/5）；可靠性模块（Wave 4）必须 hook 已有接口不新造抽象
> 3. 底层能拆就拆，不允许"超级模块"
> 4. 所有模块的接口签名在 [interface-contracts.md](./interface-contracts.md) 冻结
>
> **相关文档**：[README.md](./README.md) · [interface-contracts.md](./interface-contracts.md) · [milestones.md](./milestones.md)
>
> **事实来源**：[backend-audit.md](./backend-audit.md) §1–§5 · [aionui-audit.md](./aionui-audit.md) §1–§4 · [mcp.md](../mcp.md) §4 · [team-prompts.md](../team-prompts.md) §2–§4

---

## 0. 术语

- **Wave 1**：纯内容 / 纯结构 / 单文件可交付的"原料"模块。彼此无依赖，可以并行交付。
- **Wave 2**：把 Wave 1 的原料"串起来"让 team 最小闭环能跑的业务模块。依赖 Wave 1 全部完成。
- **Wave 3**：规范化与轻量修复——多用户过滤、agent 修复、rename 规范化、conversation 复用、MCP 帧/超时等与 scheduler 内核无关的增强。依赖 Wave 2 merge。
- **Wave 4**：scheduler 可靠性加固——activeWakes / wakeTimeouts / finalizedTurns / crash / 429 / inactivity / addAgentLocks / mcp_ready。依赖 Wave 2 merge（Wave 3 并行进行不阻塞）。
- **Wave 5**：业务闭环补全——Team Guide MCP、真实 spawn、真 kill + shutdown 协议、teammate_message 左气泡、team.mcpStatus 10-phase。依赖 Wave 3 + Wave 4。
- **Dn / W3-Dn / W4-Dn / W5-Dn**：开发者编号。Wave 1/2 用 `D1..D11` 延续历史；Wave 3/4/5 前缀标注 Wave 避免跨 Wave 编号冲突。
- **LoC**：新增或修改的 Rust 代码行（不含测试）。

---

## 1. 依赖拓扑

```
Wave 1（并行 8 人，无依赖 —— D1 是类型种子；D2/D3/D6 基于 D1 stub 起手）
 ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐
 │ D1  │  │ D2  │  │ D3  │  │ D4  │  │ D5a │  │ D5b │  │ D5c │  │ D6  │
 │api- │  │Acp- │  │Stdio│  │两个 │  │Team │  │Lead │  │Team-│  │mcp- │
 │types│  │Build│  │Serv-│  │MCP  │  │Guide│  │Prom-│  │mate │  │brid-│
 │team │  │Extra│  │er   │  │工具 │  │Prom-│  │pt   │  │Prom-│  │ge   │
 │_mcp │  │扩字 │  │Spec │  │     │  │pt   │  │(拆  │  │pt + │  │sub- │
 │(种  │  │段   │  │     │  │     │  │     │  │D5b-1│  │wake │  │cmd  │
 │子)  │  │(stub│  │(stub│  │     │  │     │  │/D5b-│  │pay- │  │(stub│
 │     │  │ D1) │  │ D1) │  │     │  │     │  │2)   │  │load │  │ D1) │
 └──┬──┘  └──┬──┘  └──┬──┘  └──┬──┘  └──┬──┘  └──┬──┘  └──┬──┘  └──┬──┘
    │        │        │        │        │        │        │        │
    ▼        ▼        ▼        ▼        ▼        ▼        ▼        ▼
   ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    【Wave 1 完工门禁】全部 merge + cargo test --workspace 通过
   ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
                             │
                             ▼
Wave 2（5 人，有序，部分可并行）
   D7 TeamSession 新方法 + send 路径  ◀─── D3, D5          (可单独起)
   D8 scheduler 首次wake              ◀─── D5, D7          (等 D7)
   D9 ensure_session 闭环             ◀─── D3, D7          (等 D7)
   D10 acp_agent 注入                 ◀─── D2, D3          (可和 D9 并行)
   D11 app 装配                       ◀─── D7, D9, D10     (最后)
```

**说明**：D1 是"类型种子"（最早 merge）；**D2 / D3 / D6 依赖 D1 的 `TeamMcpStdioConfig` 类型或 ENV 常量**，开工时先用 stub struct/const 占位起手（见各模块 `预估 LoC` 下的 stub 策略），D1 落地后再把 `pub use` 切过来，不阻塞并行。D4 / D5a / D5b-1 / D5b-2 / D5c 不依赖 D1，可独立起手。

**关键路径**：D7 → D9 → D11 是 Wave 2 的关键路径；D8 可在 D9 完成后随时并行；D10 独立于 D9，和 D9 同时开工。

### Wave 3 / Wave 4 / Wave 5 依赖拓扑

```
           【Wave 2 完工门禁】M4 Wave 2 merge + smoke 通过
                                  │
        ┌─────────────────────────┼─────────────────────────┐
        │                         │                         │
        ▼                         ▼                         ▼
Wave 3（6 人并行，无内部依赖）   Wave 4（scheduler 内核加固）
  W3-D12 user-scope              W4-D25 stream chunk 底座  ← 先做底座
  W3-D13 agent 修复                       │
  W3-D14 rename 规范化             ┌──────┼──────┬──────┬──────┐
  W3-D15 conversation 复用         ▼      ▼      ▼      ▼      ▼
  W3-D16 send_message 识别 team  W4-D18 W4-D19 W4-D20 W4-D21 W4-D22
  W3-D17 MCP 帧/超时            active wake finalize crash 429  inactivity
                                  │      dedup  recovery        watchdog
                                  │      │
                                  └──┬───┘
                                     │ (W4-D18/D19 先 merge)
                                     ▼
                                  W4-D23 add_agent_locks（独立）
                                  W4-D24 mcp_ready 握手（独立，依赖 D6）
                                  │
                                  ▼
                         【Wave 3 + Wave 4 完工门禁】
                         M5 Wave 3 merge · M6 Wave 4 merge + 可靠性 smoke
                                  │
                                  ▼
Wave 5（3 人关键路径 + 可并行点）
  W5-D26 Guide MCP server     ◀─── 需要 W3-D15 conversation 复用
                                        │
                                        ▼
  W5-D27 Guide stdio bridge 分支 ◀─── 依赖 W5-D26（tools 已可 list） + D6 subcommand
                                        │
                                        ▼
  W5-D28 Guide prompt + 互斥注入 ◀─── 依赖 W5-D26 + D5a
                                        │
         ┌──────────────────────────────┘
         ▼
  W5-D29 team_spawn_agent 真实落地 ◀─── 需要 W3-D15 conversation 复用 + W4-D18 wake 锁 + W4-D23 add_agent_locks
         │
         ▼
  W5-D30 team_shutdown_agent 真 kill + shutdown 协议 ◀─── 需要 W5-D29 能 spawn 才能 shutdown
         │
         ▼
  W5-D31 team.mcpStatus + teammate_message WS 事件 ◀─── 可在 W5-D26 启动后任意时刻并行
                                        │
                                        ▼
                         【Wave 5 完工门禁】M7 全量 e2e
```

**Wave 3 说明**：六个子模块完全彼此独立，**同时并行开工**；各自只修改对应文件且签名已冻（见 [interface-contracts.md §13–§18](./interface-contracts.md)）。

**Wave 4 关键点**：W4-D25（chunk 订阅底座）必须**最先**完成，否则 W4-D18/D20/D21/D22 全部无法订阅 stream reset 看门狗。W4-D23 + W4-D24 独立可并行。

**Wave 5 关键路径**：W5-D26 → W5-D27 → W5-D28（Guide MCP 三连）；**W5-D29 必须在 W5-D27 之前可以开工**（spawn 不依赖 Guide），两条路线在 W5-D30 汇合。

---

## 2. Wave 1 模块详单（每人 ≤ 200 行）

### D1 — `aionui-api-types::team_mcp` 子模块

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-api-types/src/team_mcp.rs`（新增） + `lib.rs` 导出（修改） |
| 职责 | 定义 `TeamMcpStdioConfig { port, token, slot_id }` 和三个 env key 常量 |
| 输入 | 无（纯数据类型） |
| 输出 | `pub struct TeamMcpStdioConfig` + `pub const ENV_*` |
| 依赖 | 只依赖 `serde` |
| 测试策略 | 2 条单元测试：JSON roundtrip、serde_json 无 unknown field 报错 |
| 预估 LoC | 40 行 |
| 预估人天 | 0.5 |
| 接口契约 | [interface-contracts.md §1](./interface-contracts.md#1-aionui-api-types-新增类型wave-1--模块-d1) |

---

### D2 — `aionui-ai-agent::AcpBuildExtra` 字段扩展

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/types.rs`（修改，只加 1 个字段 + import） |
| 职责 | 加 `#[serde(default)] team_mcp_stdio_config: Option<TeamMcpStdioConfig>` |
| 输入 | D1 导出的类型 |
| 输出 | 新字段可反序列化 |
| 依赖 | D1 必须先 merge（Cargo 依赖层面）**— phase1 特例：D2 里先用 `pub use aionui_api_types::TeamMcpStdioConfig;`，单元测试里用 stub struct 跑通 JSON；待 D1 merge 后只需删 stub** |
| 测试策略 | 2 条：旧 JSON（无字段）反序列化为 None；新 JSON 正确解出 config |
| 预估 LoC | 15 行 |
| 预估人天 | 0.3 |
| 接口契约 | [§2](./interface-contracts.md#2-aionui-ai-agenttypesacpbuildextra-扩展wave-1--模块-d2) |

---

### D3 — `aionui-team::mcp::bridge::TeamMcpStdioServerSpec`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/bridge.rs`（修改：替换原 `TeamMcpStdioConfig` 用 `pub use` + 新增 ServerSpec） |
| 职责 | 从 `(team_id, backend_binary_path, TeamMcpStdioConfig)` 构造出 `{name, command, args, env}`；并提供 `into_sdk()` 方法转成 `agent_client_protocol_schema::McpServer`（此前未用过的 variant —— **本模块第一步：读 SDK 源码确定 variant 形状并写进 TODO 注释**） |
| 输入 | D1 类型 + SDK 类型 |
| 输出 | `pub struct TeamMcpStdioServerSpec` + `from_config()` + `into_sdk()` |
| 依赖 | D1、`agent-client-protocol-schema` SDK |
| 测试策略 | 3 条：from_config 字段填充；env 命名对齐 D1 常量；snapshot SDK 序列化结果（tools/list 场景反序列化保持稳定） |
| 预估 LoC | 80 行 |
| 预估人天 | 1.0（含确认 SDK variant 形状的 0.3 天） |
| 接口契约 | [§3](./interface-contracts.md#3-aionui-teammcpbridge-新增serverspecwave-1--模块-d3) |

---

### D4 — 两个 MCP 工具 `team_list_models` / `team_describe_assistant`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/tools.rs`（修改）+ `server.rs` dispatch 分支（修改） |
| 职责 | 加 descriptor（文本原样复用 [team-prompts.md §5.2](../team-prompts.md#52-team-内部-mcp10-个工具)）+ phase1 最小 handler |
| phase1 最小实现 | `team_list_models`：返回固定 JSON `{agent_types:[{type:"claude",models:["claude-sonnet-4","claude-opus-4"]},{type:"codex",models:["gpt-5"]},...]}`（不读真实 registry）；`team_describe_assistant`：统一返回 `"Preset assistant not found"` 文本（backend 尚无 assistants 配置，aionui-audit §7.1 "workspace" 一致未实现） |
| 输入 | 无（phase1 用 hardcoded backend 表） |
| 输出 | 两个 descriptor + 两个 handler |
| 依赖 | 无 |
| 测试策略 | 4 条：2 个 descriptor 的文本匹配 team-prompts.md 原文；2 个 handler 返回 ToolResult.isError=false |
| 预估 LoC | 150 行（含两段 description 文本 + handler + 测试 fixture） |
| 预估人天 | 1.0 |
| 接口契约 | [§4](./interface-contracts.md#4-aionui-teammcptools-新增两个工具-descriptorwave-1--模块-d4) |
| ⚠️ 硬约束 | descriptor 文本**原样**来自 AionUi（aionui-audit §8 #5）；Wave 2 才补真实数据源 |

---

### D4b — `team_spawn_agent` 描述常量 `TEAM_SPAWN_AGENT_DESCRIPTION`（原样复用）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/tools.rs`（新增 `pub const TEAM_SPAWN_AGENT_DESCRIPTION: &str = r#"..."#;`） + 替换现有 `team_spawn_agent` descriptor 中的 description 字段引用常量 |
| 职责 | 只做一件事：把 AionUi `toolDescriptions.ts:1-18` 原文（"3 PRECONDITIONS + STRICT 流程"）**逐字节**复制到 Rust 常量替换后端原有的极简自造描述（"Dynamically create a new teammate agent (Lead only)."）；现有 D4 模块只管新加的 2 个工具，这条是改已有工具 |
| 依赖 | 无（纯文本常量） |
| 测试 | 2 条：常量与 team-prompts.md §5.2 `team_spawn_agent` 原文 `diff -w` 零差异；`tools/list` 返回的该工具 description 等于常量 |
| 预估 LoC | 40（常量 + 替换 + 测试） |
| 预估人天 | 0.3 |
| 事实来源 | [backend-audit §3.5 #48](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现) 标 **P0** · [aionui-audit §8 #5](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点)（原文复用硬约束） · [team-prompts.md §5.2 team_spawn_agent](../team-prompts.md#team_spawn_agent) |

---

### D5 — `aionui-team::prompts` 三层模板 + builder

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/prompts.rs`（**重写**：保留 `build_wake_payload` 概念，替换三个 builder） |
| 职责 | 定义四份常量（Guide / Lead / Teammate 模板 + spawn 工具描述），实现新签名 builder |
| 模板来源 | [team-prompts.md §2/§3/§4/§5](../team-prompts.md) —— **原样复用 AionUi 英文，禁翻译、禁改写** |
| builder 任务 | 按 [interface-contracts.md §5](./interface-contracts.md#5-aionui-teamprompts-大幅扩写wave-1--模块-d5) 的 params 产出字符串；`## Your Teammates` / `## Available Agent Types` / `## Available Preset Assistants` / `## Team Workspace` 四个动态 section 按条件开关 |
| 输入 | TeamAgent / MailboxMessage / TeamTask / HashMap<slot_id,name> |
| 输出 | 5 个 pub fn（3 个 role builder + team_guide + wake_payload） |
| 依赖 | `aionui-team::types`（已存在） |
| 测试策略 | 6 条快照测试：lead 最小参数；lead 带 preset assistants；teammate 最小；teammate 带 renamed；wake_payload 空邮件箱；wake_payload 有任务和邮件 |
| 预估 LoC | 需要承载 500+ 行 AionUi 原文文本 → **虽超 200 行约束，但都是 `r#"..."#` 常量，实际"代码"逻辑 < 150 行。此处申请例外**：模板原文是"原料"不是"逻辑"，leader 已默许（phase1 README 会重申） |
| 预估人天 | 1.5（含模板逐行从 AionUi 源码拷贝校对的 0.5 天） |
| 接口契约 | [§5](./interface-contracts.md#5-aionui-teamprompts-大幅扩写wave-1--模块-d5) |

**例外说明**：因 AionUi 三份 prompt 加起来 410 行原文必须原样搬运，模板文本视作"原料"而非"逻辑"（aionui-audit §8 #5 硬约束）。默认方案已按 team lead 要求把 D5 拆成 **4 个子模块 D5a / D5b-1 / D5b-2 / D5c**（即 8 人 Wave 1 里的 D5 系列），每人代码 < 200 行：

- **D5a**：Team Guide 模板常量 + `build_team_guide_prompt()`（~120 行）
- **D5b-1**：Lead prompt 常量，用 `include_str!("prompt_templates/lead.txt")` 引用 AionUi 原文件。目标文件 `crates/aionui-team/src/prompts/lead.rs`（代码 < 50 行）+ `crates/aionui-team/src/prompts/prompt_templates/lead.txt`（逐字节复制 AionUi `leadPrompt.ts` 模板原文）
- **D5b-2**：`build_lead_prompt()` builder 实现（依赖 D5b-1 的常量）。目标文件 `crates/aionui-team/src/prompts/lead.rs` 的 builder 部分（~30 行 Rust）
- **D5c**：Teammate 模板常量 + `build_teammate_prompt()` + `build_wake_payload()`（~150 行）

> **D5b-1 / D5b-2 依赖关系**：D5b-2 语义上依赖 D5b-1 提供的 `pub const LEAD_PROMPT_TEMPLATE: &str = include_str!(...)`。phase1 并行策略：D5b-2 开工前先在本地 stub 一个 `LEAD_PROMPT_TEMPLATE = ""`；D5b-1 merge 后只需删 stub。与 D1→D2/D3/D6 同样的"stub 起手"模式。

---

### D6 — `aionui-app mcp-bridge` 子命令

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-app/src/bridge.rs`（新增） + `main.rs`（新增 argv 分支） |
| 职责 | 实现 stdio↔TCP 透传（mcp.md §4.6 的 4 步） |
| 依赖 | `rmcp` 或手写最小 JSON-RPC 2.0 over stdio；`aionui-team::mcp::protocol::{read_frame, write_frame}`；D1 的 env key |
| 测试策略 | 2 条集成测试：1) spawn bridge 子进程 → 测试代码做 ACP 侧（向 stdin 发 `tools/list`）+ mock TCP server 侧（校验请求里有 `auth_token`、返回 fake tools）；2) TCP 连不上时 bridge 在 1s 内退出（非零 exit code） |
| 预估 LoC | 180 行（含 argv parse + stdio loop + tcp loop + signal handling） |
| 预估人天 | 1.5 |
| 接口契约 | [§8](./interface-contracts.md#8-aionui-app-新增子命令-mcp-bridgewave-1--模块-d6) |

---

## 3. Wave 2 模块详单（每人 ≤ 200 行）

### D7a — `TeamSession` 三个新方法（compute/spec/finish，不接 wake）

> **拆分说明**（team-lead 2026-04-29 二轮审阅）：原 D7 合并了"新方法"和"send 接 wake"共 280 行，超 200 行例外被驳回。现拆成 D7a（三个纯方法）+ D7b（send 路径接 wake）+ D7c（level-5 专用：`send_message_to_agent(silent=true)` + files 附件 + log-not-throw）；三者共改同文件，按 merge 顺序 D7a → D7b → D7c 串行交付（同文件不并行，避免冲突）。

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（只新增三个 pub 方法，不改 send 路径） |
| 职责 | 只实现三个无副作用方法：<br>a. `stdio_spec(slot_id)`：封 `TeamMcpStdioServerSpec::from_config(team_id, binary_path, mcp_stdio_config(slot_id))` 返回 spec<br>b. `compute_wake_input(slot_id) -> Option<WakeInput>`：读 status + unread + tasks → 按 pending/failed 判断是否注入 role prompt → 用 D5 builder 拼 first_message；mailbox 空时 `should_send = false`<br>c. `on_agent_finish(conv_id, is_error)`：调 `scheduler.finalize_turn(slot_id, &[])` → 返回值交给调用方（本模块不直接 re-wake，re-wake 在 D7b） |
| 依赖 | D3 Spec + D5 builder + D8 scheduler |
| 测试 | 4 条：pending agent 首次 compute_wake_input 返回 WithRolePrompt；working agent compute_wake_input 返回 None；mailbox 空时 should_send=false；on_agent_finish 返回 Some(lead_slot_id) |
| 预估 LoC | 150 |
| 预估人天 | 1.5 |
| 接口契约 | [§6](./interface-contracts.md#6-aionui-teamsessionteamsession-新方法wave-2--模块-d7) |

---

### D7b — `send_message` / `send_message_to_agent` 接 wake + `files` 附件 + log-not-throw

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（只改两个 send 方法）+ `crates/aionui-api-types/src/team.rs`（请求 DTO 加 `files: Option<Vec<String>>` 字段）+ `crates/aionui-team/src/routes.rs`（handler 透传 files） |
| 职责 | 1) `send_message(content, files)` / `send_message_to_agent(slot_id, content, files)` DTO 和签名加 `files: Option<Vec<String>>` 可选参数（backend-audit §3.5 #45 P0）<br>2) 写完 mailbox → 调 D7a 的 `compute_wake_input(slot_id)` → 若 `should_send` 则 `task_manager.send_message(conv_id, SendMessageData { content: first_message, files, ... })`<br>3) **log-not-throw 语义**：wake 失败（task_manager.send_message err）只 `tracing::warn!` **不** propagate 给 HTTP 调用方 → HTTP 返回 200（mailbox 已写入）；调用方重试会双写（backend-audit §3.5 #46 P0）<br>4) teammate 场景（`send_message_to_agent` 给某非 leader agent）同样接 wake |
| 依赖 | D7a（compute_wake_input）+ W2 D10（task_manager.send_message 接 SendMessageData）|
| 测试 | 5 条：leader send 后 task_manager.send_message 收到 files 参数；mailbox 空时 should_send=false 不调 wake；wake 失败返 200 + log warn（不 propagate err）；附件 files 透传到 SendMessageData；teammate send_message_to_agent 触发 target wake |
| 预估 LoC | 120 |
| 预估人天 | 1.2 |
| 接口契约 | [§6.2](./interface-contracts.md#6-aionui-teamsessionteamsession-新方法wave-2--模块-d7) |
| 事实来源 | [backend-audit §3.5 #45/#46](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现) · [aionui-audit §4.1 表格备注](./aionui-audit.md#41-wake-触发源) "log-not-throw" |

---

### D7c — `send_message_to_agent(silent=true)` phase1 占位（MCP-spawn 才用到）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`send_message_to_agent` 加 `silent: bool` 参数 + 占位分支） |
| 职责 | 只做占位：`silent=true` 走和 `silent=false` 几乎一样的流程，**但不写 user bubble** 到目标 conversation（phase1 因为 Wave 2 还没接通 conversation 的 user bubble 写入路径，此模块仅让签名支持参数，实际 silent 行为的**完整测试**在 Wave 5 W5-D26b 的 `aion_create_team` 场景里真跑（那时 leader 复用 conversation 会用到 silent=true） |
| 依赖 | D7b |
| 测试 | 2 条：silent=true 不 panic；Wave 5 e2e 验证实际效果 |
| 预估 LoC | 40 |
| 预估人天 | 0.3 |
| 接口契约 | [§6.3](./interface-contracts.md#6-aionui-teamsessionteamsession-新方法wave-2--模块-d7) |
| 事实来源 | [aionui-audit §4.1](./aionui-audit.md#41-wake-触发源) "silent=true 时 **不** 写 user bubble" |

---

### D8 — Scheduler 首次 wake 区分 + mark_idle 扩展

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（修改） |
| 职责 | 1) 在 `TeammateStatus` 加 `Pending` variant（默认值，取代 `None` 状态的"首次"语义，aionui-audit §2.1）<br>2) `try_wake` 判断 `status in {Pending, Failed}` 时返回 `WakePayload::WithRolePrompt`，否则 `WakePayload::MailboxOnly`<br>3) `maybe_wake_leader_when_all_idle` 扩大 settled 集合到 `{Idle, Completed, Failed, Pending}`（aionui-audit §8 #4） |
| 输入 | 现有 scheduler + D5 builder |
| 输出 | `WakePayload` 多一个 variant；`try_wake` 签名兼容（内部结构变化） |
| 依赖 | D5（需要用 role prompt builder） + D7（调用 D7 的 compute_wake_input 或由 D7 封装） |
| 测试策略 | 3 条：新 agent（Pending）首次 wake 返回 WithRolePrompt；同 agent 二次 wake 返回 MailboxOnly；**有 Failed 的 teammate 全员 settle 时**直接唤 leader（failed 本身就是 settled 成员，不做状态回退） |
| 预估 LoC | 120 行 |
| 预估人天 | 1.5 |

---

### D9 — `TeamSessionService::ensure_session` 打通 kill+rebuild 闭环

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（修改 `new` + `ensure_session`） |
| 职责 | 1) 构造函数加 `task_manager` + `backend_binary_path`<br>2) `ensure_session`：启 MCP server → 对每个 agent 调 `session.stdio_spec(slot_id)` → 调 `conversation_service.update_extra(conv_id, {"team_mcp_stdio_config": spec_config})` → `task_manager.kill(conv_id, TeamSessionRefresh)` → `task_manager.get_or_build_task(conv_id, opts)`<br>3) 全部成功才 insert；失败时 `session.stop()` + 不 insert<br>4) 启动 Finish 事件订阅 task（`task_manager.get_task(conv_id).subscribe()` → 过滤 Finish → 调 `session.on_agent_finish`） |
| 前置 | `ConversationService` 需要 `update_extra(conv_id, patch)` 公开方法；若现有 API 不支持，D9 作者新增（不需 schema 迁移，extra 是 JSON 字符串列） |
| 输入 | D7 |
| 输出 | 扩展后的 service |
| 依赖 | D3 / D7 / IWorkerTaskManager |
| 测试策略 | 3 条集成：`ensure_session` 成功路径（用 Mock IWorkerTaskManager 计数 kill 和 get_or_build_task 各 N 次）；`get_or_build_task` 失败时 sessions 未 insert；二次 `ensure_session` 幂等（只 kill+rebuild 一次） |
| 预估 LoC | 200 行（含测试 fixture 略紧，严格控制） |
| 预估人天 | 2.0 |

---

### D10 — `acp_agent::session_new_and_prompt` 注入 mcp_servers

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/acp_agent.rs`（修改 `session_new_and_prompt` + 构造函数加 `backend_binary_path` 字段） |
| 职责 | 按 [interface-contracts.md §7](./interface-contracts.md#7-aionui-ai-agentacp_agentsession_new_and_prompt-注入wave-2--模块-d10) 改造 `NewSessionRequest` 构造 |
| 输入 | D2（读 config）+ D3（build spec → into_sdk） |
| 输出 | session/new 携带 team MCP |
| 依赖 | D2 + D3 |
| 测试策略 | 2 条单元：无 team_mcp_stdio_config 时 req.mcp_servers 为空；有 config 时包含正确 McpServer variant；集成测试见 D11 的 smoke test |
| 预估 LoC | 60 行 |
| 预估人天 | 1.0 |

---

### D11 — `aionui-app` 装配 + e2e smoke test

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-app/src/state_builders.rs`（修改 `build_team_state` 签名）+ `lib.rs`（传 `backend_binary_path`）+ `crates/aionui-app/tests/team_phase1_smoke.rs`（新增） |
| 职责 | 1) `build_team_state` 按 [interface-contracts.md §10](./interface-contracts.md#10-aionui-appstate_buildersbuild_team_state-扩展wave-2--模块-d11) 加参数<br>2) `lib.rs::build_router` 一次性 `current_exe()` 缓存到 `Arc<PathBuf>`<br>3) e2e smoke：按 [README.md §3](./README.md) 的 8 步脚本实跑，校验 agent 能调 `team_send_message` |
| 输入 | D7 / D9 / D10 |
| 输出 | 装配完整 + 可通过的 smoke test |
| 依赖 | D7 + D9 + D10（Wave 2 最后） |
| 测试策略 | **e2e 真跑**：启动后端 in-memory DB → HTTP 建 team（2 个 agent，都是真实 claude CLI） → session → 发消息 → 等 30s → 校验 task_board 有 leader 创的任务 **或** 校验 WS 事件 `team.agentStatusChanged` 出现 Working 态 |
| 预估 LoC | 180 行 |
| 预估人天 | 2.0 |
| ⚠️ 前置 | 开发机必须装 `claude --experimental-acp` 可用；CI 暂 skip 真 CLI，靠本地手工跑证据（screenshot + 日志） |

---

### D11.5 — `remove_team` 级联 kill agent 进程

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（仅 `remove_team` 方法内在 `stop_session` 前循环遍历 `team.agents` 调 `task_manager.kill(conv_id, Some(AgentKillReason::TeamDeleted))`） |
| 职责 | 只做一件事：`remove_team` 执行链的最前面加一步——对 team 的每个 agent 调 `task_manager.kill(conv_id, TeamDeleted)`；kill 返回 NotFound 视为成功；kill 返回其他 err 只 log 不阻塞（删除不因 agent 进程残留而失败） |
| 依赖 | W2 D9（`TeamSessionService::new` 已拿到 `task_manager`）+ W3-D12c（`remove_team(user_id, id)` 归属校验先做）|
| 测试 | 2 条集成：建 team（2 个 agent）→ 用 MockWorkerTaskManager 监听 kill → `remove_team` 后 kill 被调 2 次参数是两个 agent 的 conv_id；MockWorkerTaskManager kill 全部返 NotFound → remove_team 仍成功删 team 行 |
| 预估 LoC | 40 |
| 预估人天 | 0.3 |
| 接口契约 | [§12.5](./interface-contracts.md#125-remove_team-级联-killwave-2--模块-d115) |
| 事实来源 | [backend-audit §3.5 #47](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现) 标 **P0**（agent 进程会变成孤儿） · [aionui-audit §1.4 删除时序图](./aionui-audit.md#14-删除时序图) |

---

## 7. Wave 3 模块详单（规范化与轻量修复，每人 ≤ 200 行，"拆到不能再拆"）

> **范围**：与 scheduler 内核无关的语义正确性 / 多用户隔离 / 协议硬上限类 GAP。16 人完全并行，无内部依赖（除 D12b/c 等待 D13a 的 repo trait）。
>
> **拆分原则**（team-lead 2026-04-29 确认）：
> - 一个模块一件事：职责描述里不允许出现"且/和/同时/并"；若出现必须继续拆
> - 每模块 LoC 目标 ≤ 80；超 100 必须在"不能再拆"行给理由
> - 一人一模块交付即下线，返工派新人
>
> **子锚点约定**：下文里的 `§XX.Y` 链接（例如 `§13.1`）指向 [interface-contracts.md](./interface-contracts.md) §XX 大章节内对应子模块的描述段；interface-contracts.md 若未细分子标题，则锚点解析会停在 §XX 顶部（可读性不受影响）。
>
> **验收门禁**：[README.md §2 Wave 3 验收标准](./README.md#wave-3规范化与轻量修复16-人并行)。

### W3-D12a — `list_teams(user_id)` 签名 + repo 过滤

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（仅 `list_teams`）+ `crates/aionui-db/src/repository/team.rs`（加 `list_by_user`）+ `crates/aionui-team/src/routes.rs`（仅 list handler） |
| 职责 | 只改 `list_teams`：签名 `(user_id: &str)` + repo 查询 `WHERE user_id = ?` |
| 依赖 | 无 |
| 测试 | 1 条集成：两个 user 各一个 team → A 的 list 只见 A |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§13.1](./interface-contracts.md#131-list_teamsuser_id) |
| 事实来源 | [backend-audit §1.2](./backend-audit.md#12-cratesaionui-teamsrcservicers--teamsessionservice) · [aionui-audit §1.1 listTeams(userId)](./aionui-audit.md#11-能力清单) |

### W3-D12b — `get_team(user_id, id)` 归属校验

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（仅 `get_team` 签名改造）+ `crates/aionui-team/src/routes.rs`（仅 get handler） |
| 职责 | 只改 `get_team`：不归属当前 user 的 team_id 返 `NotFound`（信息隐藏，不暴露"存在但无权"） |
| 依赖 | W3-D13a 的 `find_by_id_and_user` trait 方法 |
| 测试 | 1 条集成：A 调 `get_team(B_team_id)` 返 NotFound |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§13.2](./interface-contracts.md#132-get_teamuser_id-id) |
| 事实来源 | 同 D12a |

### W3-D12c — `remove_team(user_id, id)` 归属校验

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（仅 `remove_team`）+ `crates/aionui-team/src/routes.rs`（仅 delete handler） |
| 职责 | 只改 `remove_team`：不归属返 `NotFound` 且不执行删除 |
| 依赖 | W3-D13a |
| 测试 | 1 条集成：A 调 `remove_team(B_team_id)` 后 B 的 team 仍存在 |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§13.3](./interface-contracts.md#133-remove_teamuser_id-id) |
| 事实来源 | 同 D12a |

---

### W3-D13a — `IConversationRepository::list_by_team_id`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-db/src/repository/conversation.rs`（trait 加方法）+ `crates/aionui-db/src/repository/sqlite_conversation.rs`（实现） |
| 职责 | 只加 repo 方法：`fn list_by_team_id(team_id, user_id) -> Vec<ConversationRow>`；用 `json_extract(extra, '$.team_id') = ? AND user_id = ?` |
| 依赖 | 无（纯数据层） |
| 测试 | 2 条：命中 team + user；属于别的 user 不命中 |
| 预估 LoC | 50 · 预估人天 0.5 |
| 接口契约 | [§14.1](./interface-contracts.md#141-list_by_team_id-trait-方法) |

### W3-D13b — `repair_team_agents_if_missing` 纯函数

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（新增私有 fn，不触碰 `get_team`） |
| 职责 | 只做反推：输入 `Vec<ConversationRow>` → 输出 `Vec<TeamAgent>`（按 `created_at asc`，第一个 = Lead）；不持久化 |
| 依赖 | W3-D13a（取数据需要） |
| 测试 | 2 条：2 个 conversation 反推 2 个 agent；first agent = Lead |
| 预估 LoC | 80 · 预估人天 0.8 |
| 不能再拆理由 | 反推规则是一次映射（slot_id/role/backend/model/conv_id）；若拆成"排序" + "映射"两步，中间临时结构跨模块传无意义 |
| 接口契约 | [§14.2](./interface-contracts.md#142-repair_team_agents_if_missing-纯函数) |

### W3-D13c — `get_team` 串接修复写回

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（仅在 `get_team` 末尾加 if 分支） |
| 职责 | 只做串接：若 `agents.is_empty()` → 调 D13a 拉 conversations → 调 D13b 反推 → `repo.update` 回写 |
| 依赖 | W3-D12b（get_team 签名）+ W3-D13a + W3-D13b |
| 测试 | 1 条集成：agents=[] 的 team → 首次 get 反推 + 回写，二次 get 不再 repair |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§14.3](./interface-contracts.md#143-get_team-串接修复写回) |

---

### W3-D14a — `normalize_name` 纯函数

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs` 新子模块（或 `utils.rs`），暴露 `pub fn normalize_name(&str) -> String` |
| 职责 | 只做字符串归一化：trim + filter `is_control()` + to_lowercase |
| 依赖 | 无（纯函数，零状态） |
| 测试 | 3 条：空格 trim；大小写；控制字符过滤 |
| 预估 LoC | 50 · 预估人天 0.3 |
| 接口契约 | [§15.1](./interface-contracts.md#151-normalize_name-纯函数) |

### W3-D14b — `rename_agent` 冲突 + renamed_agents 写入

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`TeammateManager::rename_agent` 改造 + 内存字段 `renamed_agents: Mutex<HashMap<String, String>>`） |
| 职责 | 只改 `rename_agent`：调 D14a 归一化 → unique 冲突校验 → 首次 rename 写 `renamed_agents[slot_id] = old_name`（非首次不覆盖） |
| 依赖 | W3-D14a |
| 测试 | 3 条：冲突返 Err；首次记录；二次不覆盖 |
| 预估 LoC | 70 · 预估人天 0.5 |
| 接口契约 | [§15.2](./interface-contracts.md#152-rename_agent-冲突--renamed_agents) |
| 事实来源 | [aionui-audit §2.1 renameAgent](./aionui-audit.md#21-能力清单) |

### W3-D14c — Prompt builder 读 renamed_agents 渲染 `[formerly: X]`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/prompts/lead.rs` + `teammate.rs`（只改 `## Your Teammates` 段的渲染逻辑） |
| 职责 | 只改 teammates 列表渲染：对每个 agent 查 `renamed_agents.get(slot_id)`；Some 时追加 ` [formerly: <原名>]` |
| 依赖 | W3-D14b（数据源） + D5b-2 / D5c（builder 签名已预留 `renamed_agents` 参数） |
| 测试 | 2 条快照：有 renamed 渲染；无 renamed 不渲染 |
| 预估 LoC | 50 · 预估人天 0.5 |
| 接口契约 | [§15.3](./interface-contracts.md#153-prompt-builder-读-renamed_agents) |

---

### W3-D15a — `CreateAgentRequest.conversation_id` 字段

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-api-types/src/team.rs`（`CreateAgentRequest` 加字段） |
| 职责 | 只加 1 个字段：`#[serde(default)] pub conversation_id: Option<String>` |
| 依赖 | 无 |
| 测试 | 2 条：旧 JSON → None；新 JSON → Some |
| 预估 LoC | 30 · 预估人天 0.2 |
| 接口契约 | [§16.1](./interface-contracts.md#161-createagentrequestconversation_id) |

### W3-D15b — `create_team` 复用 conversation 分支

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（`create_team` 内 for each agent 循环里加一个 if 分支） |
| 职责 | 只做 `if agent.conversation_id.is_some()` 分支：读 conversation → 校验归属（非本 user NotFound）→ 校验冲突（已属别的 team BadRequest）→ `update_extra(team_id)`；None 时走原新建路径不动 |
| 依赖 | W3-D15a |
| 测试 | 3 条：合法复用；不存在 NotFound；已属别 team BadRequest |
| 预估 LoC | 100 · 预估人天 1.0 |
| 不能再拆理由 | 三个校验（存在 / 归属 / 冲突）+ update_extra 是 early-return 决策链，每一步都依赖上一步的结果；拆开会让错误路径跨模块传状态 |
| 接口契约 | [§16.2](./interface-contracts.md#162-create_team-复用分支) |
| 事实来源 | [aionui-audit §1.1 "单聊→team 的 conversation 复用"](./aionui-audit.md#11-能力清单) |
| ⚠️ 注意 | phase1 只实现 REST 路径复用；MCP 路径 `aion_create_team` 的复用由 Wave 5 W5-D26b 完成 |

---

### W3-D16a — `ITeamMessageRouter` trait 定义 + 注入点

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-conversation/src/traits.rs`（新增 trait）+ `crates/aionui-conversation/src/state.rs`（`ConversationService` 加 `team_router: Option<Arc<dyn ITeamMessageRouter>>`） |
| 职责 | 只做两件纯定义工作：<br>a. 定义 trait（1 个方法 `route_agent_message`）<br>b. ConversationService 构造新增 Option 字段；None 时维持原构造行为 |
| 依赖 | 无（trait 放 conversation crate 内，避免反向依赖 team crate） |
| 测试 | 1 条：默认 None；传入后字段持有正确 |
| 预估 LoC | 50 · 预估人天 0.4 |
| 接口契约 | [§17.1](./interface-contracts.md#171-iteammessagerouter-trait--注入点) |
| 不能再拆理由 | trait 和 field 都是纯声明，拆开会让使用方无法编译（trait 独立无用） |

### W3-D16b — `ConversationService.send_message` 路由分叉

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-conversation/src/service.rs`（仅 `send_message` 入口增加一段 if） |
| 职责 | 只做分叉：读 `row.extra.team_id`；非空 && `team_router.is_some()` → 调 router；team_router None 时 log warn 退化到原路径 |
| 依赖 | W3-D16a |
| 测试 | 3 条：无 team_id 走原路径；有 team_id + router 走 mock；有 team_id 但 router None log warn 退化 |
| 预估 LoC | 80 · 预估人天 0.7 |
| 接口契约 | [§17.2](./interface-contracts.md#172-send_message-路由分叉) |
| 事实来源 | [aionui-audit §7.1 "对 agent 发话"](./aionui-audit.md#71-rest--ipc-等价入口backend-需要暴露的-api) |

### W3-D16c — `TeamSessionService impl ITeamMessageRouter` + 装配

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（`impl ITeamMessageRouter for TeamSessionService`）+ `crates/aionui-app/src/state_builders.rs`（装配时把 team_session_service clone 作为 router 传进 conversation state） |
| 职责 | 1 件事（两个紧密耦合点）：<br>a. impl trait：按 conv_id → `session.slot_id_of` → 委托 `session.send_message_to_agent`（W2 D7 已有）<br>b. build_app_services 里把 `team_session_service.clone() as Arc<dyn ITeamMessageRouter>` 注入 conversation_service |
| 依赖 | W3-D16a + W3-D16b + W2 D7 |
| 测试 | 2 条集成：team 成员 conv 发消息 → session.send_message_to_agent 被调；不存在 conv_id 返 NotFound |
| 预估 LoC | 70 · 预估人天 0.6 |
| 不能再拆理由 | impl 没有装配点无法生效；装配点没有 impl 无从注入；二者互相定义 |
| 接口契约 | [§17.3](./interface-contracts.md#173-teamsessionservice-impl--装配) |

---

### W3-D17a — MCP 帧大小升 64MB

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-common/src/lib.rs`（新增常量 `TEAM_MCP_MAX_FRAME_BYTES`）+ `crates/aionui-team/src/mcp/protocol.rs`（`MAX_MCP_MESSAGE_SIZE` 改为引用） |
| 职责 | 只改帧大小；常量放 common 供未来其他 MCP 复用 |
| 依赖 | 无 |
| 测试 | 2 条：63MB roundtrip 通过；65MB 拒 |
| 预估 LoC | 20 · 预估人天 0.2 |
| 接口契约 | [§18.1](./interface-contracts.md#181-帧大小升-64mb) |

### W3-D17b — tool call 300s 请求超时

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-common/src/lib.rs`（`TEAM_MCP_REQUEST_TIMEOUT_MS`）+ `crates/aionui-team/src/mcp/server.rs`（dispatch_tool 外层 `tokio::time::timeout`） |
| 职责 | 只加 300s 超时；超时返 `JsonRpcError::Internal("Request timeout")` |
| 依赖 | 无 |
| 测试 | 1 条：handler sleep 301s → timeout error |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§18.2](./interface-contracts.md#182-tool-call-300s-超时) |

---

## 8. Wave 4 模块详单（鲁棒性与可靠性，每人 ≤ 200 行，"拆到不能再拆"）

> **范围**：scheduler 可靠性 8 条硬约束全部落地。
>
> **关键依赖**：W4-D25a/b/c（chunk 订阅底座）必须**最先**完成，其他订阅型模块都要它。
>
> **拆分原则**：同 §7。
>
> **验收门禁**：[README.md §2 Wave 4 验收标准](./README.md#wave-4鲁棒性与可靠性17-人w4-d25-先做底座--其余并行)。

### W4-D25a — `AgentStreamChunk` enum 定义

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/types.rs`（新 enum） |
| 职责 | 只定义 enum：`Text { text }` / `ToolUse { tool_name, input }` / `Thought { content }` / `Finish { agent_crash, stop_reason }` / `Error { message }` + `Clone + Debug` 实现 |
| 依赖 | 无（纯类型） |
| 测试 | 1 条：五个 variant 能通过 serde JSON roundtrip |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§19.1](./interface-contracts.md#191-agentstreamchunk-enum) |

### W4-D25b — `AgentManagerHandle::subscribe_stream()` trait 方法

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/task_manager.rs`（trait 加方法） |
| 职责 | 只加 trait 方法 `fn subscribe_stream(&self) -> broadcast::Receiver<AgentStreamChunk>`；不实现 |
| 依赖 | W4-D25a |
| 测试 | 1 条：`fn implements_send_sync<T: Send + Sync>() {} implements_send_sync::<broadcast::Receiver<AgentStreamChunk>>()` |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§19.2](./interface-contracts.md#192-subscribe_stream-trait-方法) |

### W4-D25c-1 — `AcpAgentManager` broadcast channel 字段 + impl `subscribe_stream`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/acp_agent.rs`（只加字段 `stream_tx: broadcast::Sender<AgentStreamChunk>` + 构造函数初始化 capacity 256 + impl `AgentManagerHandle::subscribe_stream`） |
| 职责 | 只做"broadcast 装水管"——加字段、初始化 channel、暴露 subscribe 接口；**不改任何现有 chunk 处理逻辑**（merge 后 broadcast 空闲） |
| 依赖 | W4-D25a（chunk 类型）+ W4-D25b（trait 方法签名） |
| 测试 | 2 条：构造 manager 后 subscribe_stream 返回合法 receiver；无 emit 时 receiver.try_recv 返 Empty |
| 预估 LoC | 50 |
| 预估人天 | 0.5 |
| 接口契约 | [§19.3.1](./interface-contracts.md#193-acpagentmanager-broadcast-注入) |

### W4-D25c-2 — `AcpAgentManager` 全 chunk emit 点注入

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/acp_agent.rs`（现有 5 个 chunk 处理点各插一行 `let _ = self.stream_tx.send(...)`） |
| 职责 | 只做 emit 点插入：Text / ToolUse / Thought / Finish / Error 五种 chunk 处理处各加一行 send；不 propagate send 错（零订阅者 send 返 Err 正常） |
| 依赖 | W4-D25c-1（sender 字段） |
| 测试 | 3 条：订阅后收到 Text；收到 Finish；收到 Error |
| 预估 LoC | 50 |
| 预估人天 | 0.5 |
| 接口契约 | [§19.3.2](./interface-contracts.md#193-acpagentmanager-broadcast-注入) |
| 事实来源 | [backend-audit §3.5 #53](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现) |

---

### W4-D18a — `active_wakes` 重入锁

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`TeammateManager` 加字段 `active_wakes: DashSet<String>` + 两个方法 `try_acquire_wake_lock` / `release_wake_lock`） |
| 职责 | 只做 wake 去重：<br>a. `try_acquire_wake_lock(slot_id) -> bool` 使用 `DashSet::insert` 原子语义<br>b. `release_wake_lock(slot_id)` 调 remove |
| 依赖 | 无（纯内存结构） |
| 测试 | 2 条：并发 100 次 try_acquire → 只有 1 次返 true；release 后 try_acquire 再次成功 |
| 预估 LoC | 60 · 预估人天 0.5 |
| 接口契约 | [§20.1](./interface-contracts.md#201-active_wakes-重入锁) |
| 事实来源 | [aionui-audit §8 #1/#2](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点) |

### W4-D18b-1 — `wake_timeouts` 存储字段 + `clear_wake_timeout`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`TeammateManager` 只加字段 `wake_timeouts: DashMap<String, JoinHandle<()>>` + `clear_wake_timeout(slot_id)` 实现） |
| 职责 | 只做纯存储操作：字段声明 + `clear_wake_timeout` 方法 `remove(slot_id).map(JoinHandle::abort)` |
| 依赖 | 无 |
| 测试 | 2 条：insert + clear 后 map 为空；clear 不存在的 slot_id 不 panic |
| 预估 LoC | 30 |
| 预估人天 | 0.2 |
| 接口契约 | [§20.2.1](./interface-contracts.md#202-wake_timeouts-60s-看门狗) |

### W4-D18b-2 — `arm_wake_timeout` spawn task 实现（select! 主体）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`arm_wake_timeout(slot_id, stream_rx)` 方法 spawn 后台 task） |
| 职责 | 只做 spawn tokio task：`tokio::select!` loop 监听 stream_rx（chunk → 重置 deadline；Finish → 退出）与 `sleep_until(deadline)`（超时 → 调 `handle_inactivity_timeout`，W4-D22 提供）；JoinHandle 存入 D18b-1 的 map |
| 依赖 | W4-D18b-1（存储） + W4-D25c-2（stream_rx 有效） + W4-D22（超时 handler 存在） |
| 测试 | 3 条：chunk 到达 reset deadline（不触发 timeout）；60s 无 chunk 触发 inactivity handler；Finish 到达清 map 条目 |
| 预估 LoC | 90 |
| 预估人天 | 1.0 |
| 不能再拆理由 | `tokio::select!` 三路（chunk recv / sleep / Finish）是原子并发原语；拆开会让取消语义失控 |
| 接口契约 | [§20.2.2](./interface-contracts.md#202-wake_timeouts-60s-看门狗) |
| 事实来源 | [aionui-audit §2.1 inactivity watchdog](./aionui-audit.md#21-能力清单) |

### W4-D18c — `compute_wake_input` / `send_message` 接入 wake lock

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`compute_wake_input` / `send_message` 开头结尾加 acquire / release） |
| 职责 | 只做调用点接入：session 的两处 wake 触发点在 wake 开始前 `try_acquire_wake_lock`，成功发送后立即 `release_wake_lock`（不等 finish） |
| 依赖 | W4-D18a |
| 测试 | 2 条：并发两次 compute_wake_input 同 slot → 只有一个走完全程；send 成功后 active_wakes 立即空 |
| 预估 LoC | 60 · 预估人天 0.5 |
| 接口契约 | [§20.3](./interface-contracts.md#203-session-接入-wake-lock) |

---

### W4-D19a — `finalized_turns` 存储 + API

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`TeammateManager` 加字段 `finalized_turns: DashMap<String, Instant>` + `begin_finalize(conv_id) -> bool` + `clear_finalized_turn(conv_id)`） |
| 职责 | 只做 dedup 存储：`begin_finalize` 若 `now - last < 5s` 返 false；否则写入 + spawn 5s 后清理；`clear_finalized_turn` 即 remove |
| 依赖 | 无（纯内存结构） |
| 测试 | 3 条：100ms 内两次 begin_finalize 同 conv → 第二次 false；5s 后再次 true；clear 后立即 true |
| 预估 LoC | 80 · 预估人天 0.7 |
| 接口契约 | [§21.1](./interface-contracts.md#211-finalized_turns-存储) |
| 事实来源 | [aionui-audit §4.3 + §8 #3](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点) |

### W4-D19b — `on_agent_finish` / re-wake 路径接入 dedup

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`on_agent_finish` 第一行插入 `if !scheduler.begin_finalize(conv_id) { return; }`；wake 成功后调 `clear_finalized_turn`） |
| 职责 | 只做调用点接入：两个固定位置插入 D19a 提供的方法调用 |
| 依赖 | W4-D19a |
| 测试 | 2 条：同 conv 100ms 两次 Finish → 只 finalize 一次；re-wake 后 finalize 立即可执行 |
| 预估 LoC | 50 · 预估人天 0.4 |
| 接口契约 | [§21.2](./interface-contracts.md#212-session-接入-dedup) |

---

### W4-D20a — `detect_crash(chunk) -> Option<CrashReason>` 纯函数

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（新 fn + `CrashReason` enum） |
| 职责 | 只做识别：`Finish { agent_crash: true }` → `AgentCrash`；`Error { message }` 含 `"process exited unexpectedly"` → `ProcessExited`；含 `"Session not found"` → `SessionNotFound`；否则 `None` |
| 依赖 | W4-D25a（chunk 类型） |
| 测试 | 4 条：4 种 variant 每种一条单元测试 |
| 预估 LoC | 60 · 预估人天 0.5 |
| 接口契约 | [§22.1](./interface-contracts.md#221-detect_crash-纯函数) |

### W4-D20b-1 — `handle_agent_crash` 非 leader 流程：写 testament 给 leader

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`handle_agent_crash` 内部"写 testament"私有 helper） |
| 职责 | 只做一件事：按 `reason: CrashReason` 格式化 testament 文本（`"Teammate '<name>' crashed during task (reason: <ProcessExited\|AgentCrash\|SessionNotFound>). Last message: ...". Please investigate."`）并 `mailbox.write(from=slot_id, to=lead_slot_id, Message, content=testament)` |
| 依赖 | W4-D20a（CrashReason enum） |
| 测试 | 2 条：三种 reason 的 testament 文本包含对应关键词；mailbox.write 参数 `to=lead_slot_id` |
| 预估 LoC | 40 |
| 预估人天 | 0.4 |
| 接口契约 | [§22.2.1](./interface-contracts.md#222-handle_agent_crash-非-leader-流程) |

### W4-D20b-2 — `handle_agent_crash` 非 leader 流程：kill + 清 state + wake leader

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`handle_agent_crash` 主体：串接 D20b-1 的 testament + 下列"进程卫生"步骤） |
| 职责 | 只做清理+唤醒流水线：<br>a. 调 D20b-1 写 testament<br>b. `task_manager.kill(conv_id, AgentKillReason::Crash)`<br>c. `set_status(slot_id, Failed)`<br>d. `release_wake_lock(slot_id)` + `clear_wake_timeout(slot_id)`<br>e. `wake(lead_slot_id)` |
| 依赖 | W4-D20b-1 + W4-D18a（release_wake_lock） + W4-D18b-1（clear_wake_timeout） |
| 测试 | 3 条：kill 被调；set_status(Failed) 生效；wake(leader) 被调 |
| 预估 LoC | 60 |
| 预估人天 | 0.6 |
| 不能再拆理由 | b..e 5 步是原子"安全降级"流水线：先 kill 再清锁 timer 保证新 wake 可以起；先 Failed 再 wake leader 保证 leader 看到的状态正确；拆 5 步成 5 模块会让"顺序错一处 → agent 进程或 leader 视图漂移"的责任无人承担 |
| 接口契约 | [§22.2.2](./interface-contracts.md#222-handle_agent_crash-非-leader-流程) |

### W4-D20c — `handle_agent_crash` leader 分支

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（在 D20b 的方法里加 `if role == Lead` 分支） |
| 职责 | 只做 leader crash 的特殊分支：只 `set_status(Failed)`，不 remove、不 wake 其他（aionui-audit §2.1）|
| 依赖 | W4-D20b |
| 测试 | 2 条：leader crash 不触发其他 wake；leader crash 后 agents 数组未变 |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§22.3](./interface-contracts.md#223-handle_agent_crash-leader-分支) |
| 事实来源 | [aionui-audit §2.1 crash recovery](./aionui-audit.md#21-能力清单) |

---

### W4-D21 — 429 / rate-limit 识别

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-common/src/lib.rs`（`RATE_LIMIT_REGEX` once_cell Lazy）+ `crates/aionui-team/src/session.rs`（`on_agent_finish` 里 Error chunk 分支：若 regex 命中 → `set_status(Failed)` 不走 crash） |
| 职责 | 只做 regex 匹配 + 状态设置（不 kill 不 testament） |
| 依赖 | W4-D25c |
| 测试 | 3 条：命中 "HTTP 429"；命中 "rate limit"；不命中 "syntax error" |
| 预估 LoC | 50 · 预估人天 0.3 |
| 接口契约 | [§23](./interface-contracts.md#23-429--rate-limit-识别) |
| 事实来源 | [aionui-audit §2.1 "429 / 限流识别"](./aionui-audit.md#21-能力清单) |

---

### W4-D22 — Inactivity watchdog handler

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`handle_inactivity_timeout(slot_id)` 被 D18b 的 timer 调用） |
| 职责 | 只做 inactivity 决策：`set_status(Failed)` + `release_wake_lock` + 若非 leader 则写 idle_notification 给 leader + wake leader；leader 自己 stuck 只 failed 不递归 |
| 依赖 | W4-D18a/b |
| 测试 | 3 条：teammate stuck → failed + leader 邮箱有通知；leader stuck → 只 failed；wake_timeout handler 调用后不影响其他 slot timer |
| 预估 LoC | 100 · 预估人天 1.0 |
| 不能再拆理由 | 同一个 handler 内 leader / 非 leader 是 if/else 决策树，共用前置（set_status + release_lock）；拆分会让 set_status 调用散落 |
| 接口契约 | [§24](./interface-contracts.md#24-inactivity-watchdog) |
| 事实来源 | [aionui-audit §2.1 inactivity watchdog](./aionui-audit.md#21-能力清单) |

---

### W4-D23 — `add_agent_locks` per-team 串行化

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（`TeamSessionService` 加 `add_agent_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>` + `add_agent` 入口取锁 + `remove_team` 清 lock entry） |
| 职责 | 只做 add_agent 的 per-team 串行化 |
| 依赖 | 无 |
| 测试 | 2 条：并发 10 次 add_agent 同 team → 长度 10；不同 team 并发不互相阻塞 |
| 预估 LoC | 80 · 预估人天 0.7 |
| 不能再拆理由 | lock 的申请 / 使用 / 清理三点必须在同一 service 对象内联动，拆开会让 lock entry 泄漏 |
| 接口契约 | [§25](./interface-contracts.md#25-add_agent_locks-串行化) |
| 事实来源 | [aionui-audit §8 #14](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点) |

---

### W4-D24a — `McpReadyNotification` 协议类型

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/protocol.rs`（新枚举 `McpNotification::McpReady { slot_id, auth_token }`） |
| 职责 | 只定义协议类型（serde tag="type"） |
| 依赖 | 无 |
| 测试 | 1 条：JSON roundtrip |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§26.1](./interface-contracts.md#261-mcpreadynotification-协议类型) |

### W4-D24b-1 — `TeamMcpServer` ready 数据结构字段

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（`TeamMcpServer` 加字段 `ready_latch: DashSet<String>` + `ready_notify: DashMap<String, Arc<Notify>>`） |
| 职责 | 只加字段 + 构造时初始化；不实现任何方法 |
| 依赖 | 无 |
| 测试 | 1 条：构造后两字段均为空 |
| 预估 LoC | 20 |
| 预估人天 | 0.2 |
| 接口契约 | [§26.2.1](./interface-contracts.md#262-server-notify--wait) |

### W4-D24b-2 — `notify_mcp_ready(slot_id)` 方法

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（方法实现：收到 D24a 的通知帧时调） |
| 职责 | 只做一件事：`ready_latch.insert(slot_id)` → 查 `ready_notify` map；存在则 `Notify::notify_waiters()`；不存在说明无 waiter，只存 latch 下一次 wait 立即返回 |
| 依赖 | W4-D24a（帧类型）+ W4-D24b-1（字段） |
| 测试 | 2 条：notify 后 latch 中有对应 slot_id；有 waiter 时 notify 唤醒 waiter |
| 预估 LoC | 30 |
| 预估人天 | 0.3 |
| 接口契约 | [§26.2.2](./interface-contracts.md#262-server-notify--wait) |

### W4-D24b-3 — `wait_for_mcp_ready(slot_id, 30s)` graceful

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（方法实现：外部 service 调用此方法等待 ready） |
| 职责 | 只做一件事：若 `ready_latch.contains(slot_id)` 直接 Ok；否则取 / 创建 `Notify` 放入 `ready_notify`；`tokio::select!` 等 `notify.notified()` 或 `sleep(30s)`；**timeout 分支也 `Ok(())`**（aionui-audit §8 #11 graceful） |
| 依赖 | W4-D24b-1（字段） + W4-D24b-2（notify 写入）|
| 测试 | 3 条：已有 latch 直接返回；无 notify 30s graceful Ok；两个 slot 并发独立计时互不干扰 |
| 预估 LoC | 50 |
| 预估人天 | 0.5 |
| 不能再拆理由 | `tokio::select!` 两路（notify / sleep）是原子并发原语，拆开引入 race |
| 接口契约 | [§26.2.3](./interface-contracts.md#262-server-notify--wait) |
| 事实来源 | [aionui-audit §3.1 "MCP ready 握手"](./aionui-audit.md#31-能力清单) · [aionui-audit §8 #11](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点)（graceful timeout） |

### W4-D24c — Bridge 端发 `mcp_ready` 通知

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-app/src/bridge.rs`（D6 主 bridge 逻辑内 initialize 成功后追加一行 send） |
| 职责 | 只做一件事：TCP connect + initialize ok 后 fire-and-forget 发 `{type:"mcp_ready", slot_id, auth_token}` |
| 依赖 | D6（bridge 主体）+ W4-D24a（协议类型） |
| 测试 | 1 条集成：bridge 启动 → 连上 mock server → server 在 100ms 内收到 `mcp_ready` |
| 预估 LoC | 30 · 预估人天 0.2 |
| 接口契约 | [§26.3](./interface-contracts.md#263-bridge-端发-mcp_ready) |

---

## 9. Wave 5 模块详单（业务闭环补全，每人 ≤ 200 行，"拆到不能再拆"）

> **范围**：让"单聊→建团→真 spawn→真 kill"闭环、事件完整。
>
> **拆分原则**：同 §7。Wave 5 是 phase1 最容易出"超模块"的地方，本次全部拆开。
>
> **验收门禁**：[README.md §2 Wave 5 验收标准](./README.md#wave-5业务闭环补全19-人)。

### W5-D26a — `GuideMcpServer` 结构 + 启停（只 TCP 启停，不含工具）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/guide/server.rs`（新 struct + `start_singleton` + `stop`） |
| 职责 | 只做 TCP server 生命周期：bind 127.0.0.1 随机端口 + auth_token UUID + accept_loop 骨架（dispatch 留占位 TODO 交给 D26b/c） + `stop` 发 shutdown signal |
| 依赖 | 无（纯 TCP 框架） |
| 测试 | 2 条：start 返回成功 addr；stop 后 bind 端口释放 |
| 预估 LoC | 80 · 预估人天 0.8 |
| 接口契约 | [§27.1](./interface-contracts.md#271-guidemcpserver-结构--启停) |
| 事实来源 | [aionui-audit §3.1 Team Guide MCP 生命周期](./aionui-audit.md#31-能力清单) |

### W5-D26b-1 — `aion_create_team` args 解析 + 默认值补全（纯函数）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/guide/handlers.rs`（新 `parse_create_team_args(args, caller_conversation) -> CreateTeamParams` 纯函数） |
| 职责 | 只做"输入变成结构"：<br>a. 解析必需字段 `summary`（缺失 Err）<br>b. 解析可选 `name` / `workspace`<br>c. workspace 缺省 → 从 `caller_conversation.extra.workspace` 继承<br>d. name 缺省 → `summary.split_whitespace().take(5).collect::<Vec<_>>().join(" ")`<br>e. 返回 `CreateTeamParams { summary, name, workspace }` |
| 依赖 | 无（纯数据映射） |
| 测试 | 4 条：summary 缺失 Err；name 缺省用 summary 前 5 词；workspace 缺省继承 caller；全字段自定义优先生效 |
| 预估 LoC | 70 |
| 预估人天 | 0.6 |
| 接口契约 | [§27.2.1](./interface-contracts.md#272-handle_aion_create_team) |

### W5-D26b-2 — `handle_aion_create_team` 调 service + 返回结构化

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/guide/handlers.rs`（handler 主体） |
| 职责 | 只做"拿到 params 后调 service"：<br>a. 调 D26b-1 拿 `CreateTeamParams`<br>b. 构造 `CreateTeamRequest { agents: [Lead { conversation_id: caller_conversation_id 复用 W3-D15b }], workspace_mode: "shared", session_mode: "yolo" }`<br>c. `service.create_team("system_default_user", req).await`<br>d. 返回 `{ team_id, name, route: "/team/<id>", lead_agent, status: "team_created", next_step: "The team page has been opened automatically. End your turn now." }` |
| 依赖 | W5-D26b-1（params）+ W3-D15b（conversation 复用） |
| 测试 | 3 条：正常路径 service.create_team 被调且返回含 next_step；service Err 时返 ToolResult.is_error=true；leader 复用 caller_conversation_id |
| 预估 LoC | 70 |
| 预估人天 | 0.7 |
| 接口契约 | [§27.2.2](./interface-contracts.md#272-handle_aion_create_team) |
| 事实来源 | [aionui-audit §1.2 建团流程](./aionui-audit.md#12-建团流程时序图mcp-spawn) |

### W5-D26c — `handle_aion_list_models` handler

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/guide/handlers.rs`（新 fn，仅 list_models） |
| 职责 | 只做 tool 的 handler：直接复用 D4 的 `team_list_models` handler（硬编码 backend × model 表） |
| 依赖 | W5-D26a + D4 |
| 测试 | 1 条：返回 JSON schema 和 D4 一致 |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§27.3](./interface-contracts.md#273-handle_aion_list_models) |

### W5-D26d — 建团成功后 emit 3 个 WS 事件

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/guide/handlers.rs`（D26b 的末尾插入事件广播） |
| 职责 | 只在 `handle_aion_create_team` 成功路径尾部 emit 3 个 WS 事件：`team.listChanged` + `conversation.listChanged` + `deepLink.received { route:/team/<id> }` |
| 依赖 | W5-D26b（handler 已出结果）+ W5-D31a（事件类型定义） |
| 测试 | 1 条集成：建团成功 → WS 订阅者收到三个事件 |
| 预估 LoC | 50 · 预估人天 0.4 |
| 接口契约 | [§27.4](./interface-contracts.md#274-aion_create_team-成功后的-ws-事件) |

---

### W5-D27 — Team Guide stdio bridge 分支

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-app/src/bridge.rs`（D6 主 bridge 里加 `if env::var(AION_MCP_BACKEND).is_ok()` 分支） |
| 职责 | 只做 bridge 端分叉：env 里有 `AION_MCP_BACKEND` → 走 guide bridge 模式，每条 tools/call payload 额外带 `backend` + `conversation_id`；否则走 team bridge（不动） |
| 依赖 | D6（bridge 主体） + W5-D26（guide server 约定协议） |
| 测试 | 2 条：guide 模式 payload 含 backend+conversation_id；team 模式（无 backend env）行为不变 |
| 预估 LoC | 80 · 预估人天 0.7 |
| 不能再拆理由 | bridge 的 if/else 分叉是单点入口，分叉条件检查和两条路径选择不能拆分 |
| 接口契约 | [§28](./interface-contracts.md#28-guide-stdio-bridge-分支) |
| 事实来源 | [aionui-audit §3.3 stdio↔TCP 桥架构](./aionui-audit.md#33-stdio--tcp-桥架构) |

---

### W5-D28a — `is_team_capable_backend` 纯函数 + 白名单常量

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/guide/capability.rs`（新增） |
| 职责 | 只做纯函数：`pub fn is_team_capable_backend(backend: &str, mcp_stdio_capable: bool) -> bool` + 硬白名单常量 `TEAM_CAPABLE_BACKENDS = &["claude", "codex", "gemini", "aionrs"]` |
| 依赖 | 无 |
| 测试 | 3 条：白名单命中；非白名单 + mcp_stdio_capable=true 命中；非白名单 + false 不命中 |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§29.1](./interface-contracts.md#291-is_team_capable_backend-纯函数) |

### W5-D28b — Guide prompt 注入到 instructions + 互斥 guard

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/acp_agent.rs`（构造 instructions 的地方加分支） |
| 职责 | 只做 instructions 拼接分支：<br>`if extra.team_mcp_stdio_config.is_none() && is_team_capable_backend(...)` → 调 `build_team_guide_prompt(backend, leader_label=None)` append 进 instructions；否则不动 |
| 依赖 | W5-D28a + D5a（prompt builder） + D2（AcpBuildExtra） |
| 测试 | 3 条：solo claude 含 Guide prompt；已在 team 的 agent 不含；solo backend="unknown" 不含 |
| 预估 LoC | 60 · 预估人天 0.5 |
| 接口契约 | [§29.2](./interface-contracts.md#292-guide-prompt-注入到-instructions) |
| 事实来源 | [aionui-audit §8 #17 Guide 互斥](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点) |

### W5-D28c — `session/new.mcp_servers` 追加 Guide config + 互斥 guard

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-ai-agent/src/acp_agent.rs`（`session_new_and_prompt` 里 mcp_servers 构造的地方加分支） |
| 职责 | 只做 mcp_servers 追加分支：<br>同 D28b 的 guard 条件 → 若满足 → `guide_server.stdio_config(backend, conv_id)` 转成 `McpServer` 追加进 vec；否则不动 |
| 依赖 | W5-D28a + W5-D26（guide config 来源） + W2 D10（mcp_servers 注入已有） |
| 测试 | 3 条：solo claude 的 mcp_servers 包含 guide；已在 team 的 agent 不包含；solo 非白名单不包含 |
| 预估 LoC | 60 · 预估人天 0.5 |
| 接口契约 | [§29.3](./interface-contracts.md#293-session-new-mcp_servers-追加-guide) |

---

### W5-D29a-1 — `SpawnAgentRequest` 类型 + `spawn_agent` 方法骨架

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（新 struct `SpawnAgentRequest` + `TeamSession::spawn_agent` 空壳 fn 签名，body `todo!()`） |
| 职责 | 只做类型声明 + 方法签名：`SpawnAgentRequest { name, agent_type, custom_agent_id, model }` + `pub async fn spawn_agent(caller_slot_id, req) -> Result<TeamAgent, TeamError>` |
| 依赖 | 无 |
| 测试 | 1 条：类型可构造 + 方法签名可编译（trait 对象 Send） |
| 预估 LoC | 30 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.1.1](./interface-contracts.md#301-spawnagentrequest--校验层) |

### W5-D29a-2 — `spawn_agent` 校验：caller role == Lead

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 第一段校验） |
| 职责 | 只做一件事：按 `caller_slot_id` 取 agent；若 `agent.role != Lead` → `Err(TeamError::LeaderOnly)` |
| 依赖 | W5-D29a-1（方法骨架） |
| 测试 | 2 条：caller Lead 通过；非 Lead 返 Err |
| 预估 LoC | 20 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.1.2](./interface-contracts.md#301-spawnagentrequest--校验层) |

### W5-D29a-3 — `spawn_agent` 校验：name 归一化 + 唯一性

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 第二段校验） |
| 职责 | 只做一件事：调 W3-D14a `normalize_name`；对比现有 agents 规范化名；冲突 → `Err(TeamError::NameConflict)` |
| 依赖 | W5-D29a-2 + W3-D14a（normalize_name） |
| 测试 | 2 条：新 name 通过；已有规范化同名返 Err |
| 预估 LoC | 25 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.1.3](./interface-contracts.md#301-spawnagentrequest--校验层) |

### W5-D29a-4 — `spawn_agent` 校验：backend 在白名单

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 第三段校验） |
| 职责 | 只做一件事：`req.agent_type` 缺省继承 caller.backend；校验在 `SPAWN_BACKEND_WHITELIST`（`["claude", "codex"]`）；非白名单 → `Err(TeamError::BackendNotAllowed)` |
| 依赖 | W5-D29a-3 |
| 测试 | 2 条：白名单通过；非白名单返 Err |
| 预估 LoC | 25 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.1.4](./interface-contracts.md#301-spawnagentrequest--校验层) |
| 事实来源 | [aionui-audit §2.1 MCP spawn](./aionui-audit.md#21-能力清单) · backend-audit §1.5.2 SPAWN_BACKEND_WHITELIST |

### W5-D29b — `TeamSessionService::add_agent` 扩展用于 spawn

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（`add_agent` 扩展：原签名保留，内部持 W4-D23 lock + build conversation + 分配 slot_id + update repo） |
| 职责 | 只做 add_agent 扩展：把现有 add_agent 流程（read-modify-write agents）封装为"持锁 + 新建 conversation + 分配 slot_id"的原子操作，返回 `TeamAgent` |
| 依赖 | W4-D23（add_agent_locks） |
| 测试 | 2 条：持锁期间其他 add_agent 阻塞；返回的 TeamAgent 有 slot_id + conv_id |
| 预估 LoC | 100 · 预估人天 1.0 |
| 不能再拆理由 | add_agent 的 RMW（read-modify-write）持锁窗口必须在同一函数内完成；拆开会让锁范围失控 |
| 接口契约 | [§30.2](./interface-contracts.md#302-add_agent-扩展) |

### W5-D29c-1 — spawn 后写 `extra.team_mcp_stdio_config`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 中段第 1 步） |
| 职责 | 只做一件事：`conversation_service.update_extra(new_conv_id, {team_mcp_stdio_config: session.stdio_spec(new_slot_id).into_config()})` |
| 依赖 | W5-D29b（new_conv_id/new_slot_id） + W2 D7a（stdio_spec） |
| 测试 | 1 条：update_extra 被调且参数含 team_mcp_stdio_config |
| 预估 LoC | 30 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.3.1](./interface-contracts.md#303-write_extra--kill--rebuild) |

### W5-D29c-2 — spawn 后 `task_manager.kill` + `get_or_build_task`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 中段第 2/3 步） |
| 职责 | 只做两步原子"重启 agent 进程"：`task_manager.kill(new_conv_id, Some(TeamSpawn))`（NotFound 视为成功）→ `task_manager.get_or_build_task(new_conv_id, opts)` |
| 依赖 | W5-D29c-1（extra 已写）+ W2 D9（kill/get_or_build_task 已有） |
| 测试 | 2 条：kill 被调；kill 后 get_or_build_task 被调 |
| 预估 LoC | 50 |
| 预估人天 | 0.4 |
| 不能再拆理由 | kill + get_or_build_task 是 "进程重启" 组合原语（mcp.md §4.3 已确立这个不可拆语义）；拆开会让 DashMap 状态不一致（kill 成功但 rebuild 失败时无人负责回滚） |
| 接口契约 | [§30.3.2](./interface-contracts.md#303-write_extra--kill--rebuild) |

### W5-D29d-1 — spawn 后写欢迎消息到新 agent mailbox

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 末段第 1 步） |
| 职责 | 只做一件事：`mailbox.write(from=caller_slot_id, to=new_slot_id, Message, content="You have been spawned as <name>. Read your mailbox and wait for instructions.")` |
| 依赖 | W5-D29c-2（new agent 已就绪）|
| 测试 | 1 条：mailbox 读 new_slot_id 的未读 = 1 条欢迎消息 |
| 预估 LoC | 20 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.4.1](./interface-contracts.md#304-欢迎消息--wake--事件) |

### W5-D29d-2 — spawn 后 wake 新 agent

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 末段第 2 步） |
| 职责 | 只做一件事：调 `wake(new_slot_id)` 触发首次 role prompt 注入（D7a 的 compute_wake_input → D7b 的 send 路径） |
| 依赖 | W5-D29d-1 + W4-D18a（wake lock 可用） + D7b（send 路径接 wake） |
| 测试 | 1 条：wake 被调且 task_manager.send_message 收到 role prompt payload |
| 预估 LoC | 20 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.4.2](./interface-contracts.md#304-欢迎消息--wake--事件) |

### W5-D29d-3 — spawn 后 emit `team.agentSpawned` WS 事件

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`spawn_agent` 末段第 3 步） |
| 职责 | 只做一件事：`broadcaster.broadcast(WsEvent::TeamAgentSpawned { team_id, agent: new_agent })` |
| 依赖 | W5-D29d-2 |
| 测试 | 1 条：WS 订阅者收到 team.agentSpawned 事件且 payload 正确 |
| 预估 LoC | 20 |
| 预估人天 | 0.2 |
| 接口契约 | [§30.4.3](./interface-contracts.md#304-欢迎消息--wake--事件) |

---

### W5-D30a-1 — `team_send_message` 识别 `shutdown_approved` 字符串

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（`handle_send_message` 顶部加 if 分支） |
| 职责 | 只做识别分发：若 `message.trim() == "shutdown_approved"` → 调 D30a-2（处理分支）；否则走下面的 rejected / 普通消息分支 |
| 依赖 | 无（纯字符串匹配） |
| 测试 | 2 条：approved 走 approved 分支；普通消息不走 |
| 预估 LoC | 20 |
| 预估人天 | 0.2 |
| 接口契约 | [§31.1.1](./interface-contracts.md#311-approved-拦截) |

### W5-D30a-2 — approved 处理：remove_agent + 通知 leader + wake

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（在 D30a-1 识别后调用的 async helper） |
| 职责 | 只做已识别后的 approved 流程：<br>a. `scheduler.remove_agent(caller_slot_id)`（D30d 负责真 kill）<br>b. `mailbox.write(to=leader, content="Teammate '<name>' has been removed (approved shutdown).")`<br>c. `wake(leader_slot_id)` |
| 依赖 | W5-D30a-1（识别） + W5-D30d-3（remove_agent 改造完整完成） |
| 测试 | 3 条：remove_agent 被调；leader 邮箱有通知；leader 被 wake |
| 预估 LoC | 60 |
| 预估人天 | 0.5 |
| 接口契约 | [§31.1.2](./interface-contracts.md#311-approved-拦截) |
| 事实来源 | [aionui-audit §2.1 shutdown 协议](./aionui-audit.md#21-能力清单) |

### W5-D30b — `team_send_message` 识别 `shutdown_rejected: <reason>`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（同 D30a 所在位置的第二个拦截分支） |
| 职责 | 只做 rejected 拦截：识别 `starts_with("shutdown_rejected:")` → 提取 reason → mailbox.write(to=leader, content="Teammate '<name>' declined shutdown: <reason>") → wake leader（不 remove） |
| 依赖 | W5-D30a（共享 team_send_message 的拦截点，merge 顺序需协调） |
| 测试 | 2 条：rejected 触发 leader 通知；remove_agent 未被调 |
| 预估 LoC | 50 · 预估人天 0.4 |
| 接口契约 | [§31.2](./interface-contracts.md#312-rejected-拦截) |

### W5-D30c — `shutdown_agent` 目标 role=Lead 校验

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`shutdown_agent` 方法顶部） |
| 职责 | 只加一行校验：`if target.role == Lead → Err(CannotShutdownLeader)` |
| 依赖 | 无 |
| 测试 | 1 条：leader 调 shutdown_agent(target=leader) 返 Err |
| 预估 LoC | 30 · 预估人天 0.2 |
| 接口契约 | [§31.3](./interface-contracts.md#313-shutdown_agent-目标-role-校验) |
| 事实来源 | [aionui-audit §2.1 "Leader 不可 shutdown"](./aionui-audit.md#21-能力清单) |

### W5-D30d-1 — `remove_agent` 改造：`task_manager.kill`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`remove_agent` 第 1 步） |
| 职责 | 只做一件事：`task_manager.kill(conv_id, Some(AgentKillReason::Shutdown))`（NotFound 视为成功；其他 err 只 log 不阻塞） |
| 依赖 | 无（既有 task_manager） |
| 测试 | 2 条：kill 被调；kill NotFound 不 panic |
| 预估 LoC | 25 |
| 预估人天 | 0.2 |
| 接口契约 | [§31.4.1](./interface-contracts.md#314-remove_agent-真-kill) |

### W5-D30d-2 — `remove_agent` 改造：清 `active_wakes` / `wake_timeouts` / `finalized_turns`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`remove_agent` 第 2 步；使用 W4-D18a/b-1/W4-D19a 已有 API） |
| 职责 | 只做内部 state 清理 3 件（每件是 1 行调用，一起做是因为**同一个 remove_agent 函数内**，拆成跨函数反而让职责逃逸）：<br>a. `active_wakes.remove(slot_id)`（W4-D18a 提供）<br>b. `clear_wake_timeout(slot_id)`（W4-D18b-1 提供）<br>c. `finalized_turns.remove(conv_id)`（W4-D19a 提供） |
| 依赖 | W4-D18a + W4-D18b-1 + W4-D19a |
| 测试 | 1 条：三处 map 里该 slot_id/conv_id 都被清空 |
| 预估 LoC | 15 |
| 预估人天 | 0.2 |
| 不能再拆理由 | 3 次 `.remove()` 调用，拆开就是"一人一行代码"的荒谬粒度 |
| 接口契约 | [§31.4.2](./interface-contracts.md#314-remove_agent-真-kill) |

### W5-D30d-3 — `remove_agent` 改造：`slots` 移除 + emit `team.agentRemoved`

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/scheduler.rs`（`remove_agent` 第 3 步） |
| 职责 | 只做一件事：`slots.lock().remove(slot_id)` + `broadcaster.broadcast(WsEvent::TeamAgentRemoved { team_id, slot_id })` |
| 依赖 | W5-D30d-1 + W5-D30d-2（前两步已完成） |
| 测试 | 2 条：slots 里无该 slot_id；WS 订阅者收到 agentRemoved 事件 |
| 预估 LoC | 25 |
| 预估人天 | 0.2 |
| 接口契约 | [§31.4.3](./interface-contracts.md#314-remove_agent-真-kill) |
| 事实来源 | [aionui-audit §2.1 TeammateManager.removeAgent](./aionui-audit.md#21-能力清单) |

---

### W5-D31a — `TeamMcpPhase` enum + WS payload 类型

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-api-types/src/team.rs`（新 enum + 两个 payload struct） |
| 职责 | 只定义类型：<br>a. `TeamMcpPhase` 10 个 variant（tcp_ready / tcp_error / session_injecting / session_ready / session_error / load_failed / degraded / config_write_failed / mcp_tools_waiting / mcp_tools_ready）<br>b. `TeamMcpStatusPayload { team_id, slot_id, phase, port, server_count, error }`<br>c. `TeammateMessagePayload { conversation_id, content, from_slot_id, from_name }` |
| 依赖 | 无 |
| 测试 | 2 条：10 个 phase serde roundtrip；两个 payload 序列化字段齐 |
| 预估 LoC | 60 · 预估人天 0.4 |
| 接口契约 | [§32.1](./interface-contracts.md#321-teammcpphase--payload-类型) |
| 事实来源 | [aionui-audit §7.6 事件](./aionui-audit.md#76-事件--ipc后端等价需提供-websocket-或-sse) |

### W5-D31b-1 — `team.mcpStatus` tcp 层 2 点广播（mcp/server.rs）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/mcp/server.rs`（在 TCP `bind` 成功 / 失败处各插一行 broadcast） |
| 职责 | 只负责 server.rs 文件内 2 个点：`tcp_ready`（bind OK 后） + `tcp_error`（bind 失败分支）|
| 依赖 | W5-D31a（payload 类型） |
| 测试 | 2 条：正常 start 观察到 tcp_ready；port 冲突 → tcp_error |
| 预估 LoC | 30 |
| 预估人天 | 0.3 |
| 接口契约 | [§32.2.1](./interface-contracts.md#322-10-phase-广播点) |

### W5-D31b-2 — `team.mcpStatus` service 层 6 点广播（service.rs）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/service.rs`（`ensure_session` 的 6 个分支处各插 broadcast） |
| 职责 | 只负责 service.rs 内 6 个点：`session_injecting`（循环 agents 开始） + `session_ready`（sessions.insert 成功） + `session_error`（任一 agent 失败回滚） + `config_write_failed`（update_extra 失败） + `load_failed`（get_or_build_task 失败） + `degraded`（wait_for_mcp_ready timeout，hook W4-D24b-3） |
| 依赖 | W5-D31a + W4-D24b-3（degraded 需要 wait_for_mcp_ready 的 timeout 信号）|
| 测试 | 2 条集成：正常 ensure_session 观察到 session_ready；模拟 update_extra 失败 → config_write_failed |
| 预估 LoC | 40 |
| 预估人天 | 0.4 |
| 接口契约 | [§32.2.2](./interface-contracts.md#322-10-phase-广播点) |

### W5-D31b-3 — `team.mcpStatus` bridge 层 2 点广播（app/bridge.rs）

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-app/src/bridge.rs`（bridge 的 tools/list 前 / 后两个时机） |
| 职责 | 只负责 bridge.rs 内 2 个点：`mcp_tools_waiting`（bridge 连 TCP 成功、tools/list 未返前） + `mcp_tools_ready`（tools/list 成功返回后） |
| 依赖 | W5-D31a + D6（bridge 主体）+ W4-D24c（`mcp_ready` 已发） |
| 测试 | 1 条集成：bridge 启动 → 观察到 mcp_tools_waiting → 观察到 mcp_tools_ready |
| 预估 LoC | 25 |
| 预估人天 | 0.2 |
| 接口契约 | [§32.2.3](./interface-contracts.md#322-10-phase-广播点) |

### W5-D31c — `teammate_message` 左气泡 emit

| 项 | 内容 |
|---|---|
| 目标文件 | `crates/aionui-team/src/session.rs`（`compute_wake_input` 内 teammate 分支末尾遍历 unread_messages 逐条 emit） |
| 职责 | 只做 emit：若 agent.role != Lead，则遍历 unread_messages，每条 broadcast `WsEvent::ConversationResponseStream { type:"teammate_message", conversation_id, content, from_slot_id, from_name }`（Lead 不 emit） |
| 依赖 | W5-D31a |
| 测试 | 2 条：teammate 有 2 条 unread → emit 2 次；Lead 有 2 条 unread → emit 0 次 |
| 预估 LoC | 40 · 预估人天 0.3 |
| 接口契约 | [§32.3](./interface-contracts.md#323-teammate_message-emit) |
| 事实来源 | [aionui-audit §2.1 "active 期间输入字节流"](./aionui-audit.md#21-能力清单) |

---

## 4. 分配表（全 5 Wave · 87 人模块，"拆到不能再拆" 二轮）

| 开发者 | Wave | 模块 | 文件 | LoC | 人天 |
|:---:|:---:|---|---|:-:|:---:|
| D1 | 1 | team_mcp types | `aionui-api-types/src/team_mcp.rs` | 40 | 0.5 |
| D2 | 1 | AcpBuildExtra 字段 | `aionui-ai-agent/src/types.rs` | 15 | 0.3 |
| D3 | 1 | Stdio ServerSpec | `aionui-team/src/mcp/bridge.rs` | 80 | 1.0 |
| D4 | 1 | 两个新 MCP 工具（list_models / describe_assistant） | `aionui-team/src/mcp/{tools,server}.rs` | 150 | 1.0 |
| **D4b** | 1 | **`TEAM_SPAWN_AGENT_DESCRIPTION` 原文常量**（P0#48 补漏） | `aionui-team/src/mcp/tools.rs` | 40 | 0.3 |
| D5a | 1 | Team Guide Prompt | `aionui-team/src/prompts/team_guide.rs` | 120 | 0.5 |
| D5b-1 | 1 | Lead Prompt 常量 (include_str! txt) | `lead.rs` + `prompt_templates/lead.txt` | ⚠️ 48 rust+188 txt | 0.3 |
| D5b-2 | 1 | Lead Prompt builder | `aionui-team/src/prompts/lead.rs` | 30 | 0.5 |
| D5c | 1 | Teammate Prompt + wake payload | `aionui-team/src/prompts/teammate.rs` | ⚠️ 150 | 1.0 |
| D6 | 1 | mcp-bridge subcommand | `aionui-app/src/bridge.rs` | 180 | 1.5 |
| **D7a** | 2 | **TeamSession 三个新方法（compute/spec/finish）** | `aionui-team/src/session.rs` | 150 | 1.5 |
| **D7b** | 2 | **send 路径接 wake + `files` 附件 + log-not-throw**（P0#45/#46） | `aionui-team/src/session.rs` + `aionui-api-types/src/team.rs` + `routes.rs` | 120 | 1.2 |
| **D7c** | 2 | **`send_message_to_agent(silent=true)` 占位** | `aionui-team/src/session.rs` | 40 | 0.3 |
| D8 | 2 | Scheduler 首次 wake | `aionui-team/src/scheduler.rs` | 120 | 1.5 |
| D9 | 2 | ensure_session 闭环 | `aionui-team/src/service.rs` | ⚠️ 200 | 2.0 |
| D10 | 2 | acp_agent 注入 | `aionui-ai-agent/src/acp_agent.rs` | 60 | 1.0 |
| D11 | 2 | app 装配 + smoke test | `aionui-app/*` | 180 | 2.0 |
| **D11.5** | 2 | **`remove_team` 级联 kill agent 进程**（P0#47 补漏） | `aionui-team/src/service.rs` | 40 | 0.3 |
| W3-D12a | 3 | `list_teams(user_id)` | service / repo / routes | 40 | 0.3 |
| W3-D12b | 3 | `get_team(user_id, id)` 归属校验 | service / routes | 40 | 0.3 |
| W3-D12c | 3 | `remove_team(user_id, id)` 归属校验 | service / routes | 40 | 0.3 |
| W3-D13a | 3 | `IConversationRepository::list_by_team_id` | db/conversation repo | 50 | 0.5 |
| W3-D13b | 3 | `repair_team_agents_if_missing` 纯函数 | team/service.rs | 80 | 0.8 |
| W3-D13c | 3 | `get_team` 串接修复写回 | team/service.rs | 40 | 0.3 |
| W3-D14a | 3 | `normalize_name` 纯函数 | team/scheduler.rs | 50 | 0.3 |
| W3-D14b | 3 | `rename_agent` 冲突 + renamed_agents 写入 | team/scheduler.rs | 70 | 0.5 |
| W3-D14c | 3 | Prompt builder 读 renamed_agents | prompts/lead/teammate.rs | 50 | 0.5 |
| W3-D15a | 3 | `CreateAgentRequest.conversation_id` 字段 | api-types/team.rs | 30 | 0.2 |
| W3-D15b | 3 | `create_team` 复用 conversation 分支 | team/service.rs | 100 | 1.0 |
| W3-D16a | 3 | `ITeamMessageRouter` trait + 注入点 | conversation/{traits,state}.rs | 50 | 0.4 |
| W3-D16b | 3 | `send_message` 路由分叉 | conversation/service.rs | 80 | 0.7 |
| W3-D16c | 3 | `TeamSessionService impl router` + 装配 | team/service.rs + app/state_builders.rs | 70 | 0.6 |
| W3-D17a | 3 | MCP 帧 64MB 常量 | common/lib.rs + mcp/protocol.rs | 20 | 0.2 |
| W3-D17b | 3 | tool call 300s 超时 | common/lib.rs + mcp/server.rs | 40 | 0.3 |
| W4-D25a | 4 | `AgentStreamChunk` enum | ai-agent/types.rs | 40 | 0.3 |
| W4-D25b | 4 | `subscribe_stream()` trait 方法 | ai-agent/task_manager.rs | 40 | 0.3 |
| **W4-D25c-1** | 4 | **broadcast channel 字段 + impl subscribe_stream** | ai-agent/acp_agent.rs | 50 | 0.5 |
| **W4-D25c-2** | 4 | **全 chunk emit 点注入（5 个 chunk 处理点）** | ai-agent/acp_agent.rs | 50 | 0.5 |
| W4-D18a | 4 | `active_wakes` 重入锁 | team/scheduler.rs | 60 | 0.5 |
| **W4-D18b-1** | 4 | **wake_timeouts 存储字段 + clear_wake_timeout** | team/scheduler.rs | 30 | 0.2 |
| **W4-D18b-2** | 4 | **arm_wake_timeout spawn task（select! 主体）** | team/scheduler.rs | 90 | 1.0 |
| W4-D18c | 4 | session 接入 wake lock | team/session.rs | 60 | 0.5 |
| W4-D19a | 4 | `finalized_turns` 存储 + API | team/scheduler.rs | 80 | 0.7 |
| W4-D19b | 4 | session 接入 dedup | team/session.rs | 50 | 0.4 |
| W4-D20a | 4 | `detect_crash` 纯函数 | team/scheduler.rs | 60 | 0.5 |
| **W4-D20b-1** | 4 | **非 leader crash：写 testament helper** | team/scheduler.rs | 40 | 0.4 |
| **W4-D20b-2** | 4 | **非 leader crash：kill + 清 state + wake leader** | team/scheduler.rs | 60 | 0.6 |
| W4-D20c | 4 | `handle_agent_crash` leader 分支 | team/scheduler.rs | 40 | 0.3 |
| W4-D21 | 4 | 429 / rate-limit 识别 | common/lib.rs + team/session.rs | 50 | 0.3 |
| W4-D22 | 4 | Inactivity watchdog handler | team/scheduler.rs | 100 | 1.0 |
| W4-D23 | 4 | `add_agent_locks` 串行化 | team/service.rs | 80 | 0.7 |
| W4-D24a | 4 | `McpReadyNotification` 协议类型 | team/mcp/protocol.rs | 40 | 0.3 |
| **W4-D24b-1** | 4 | **TeamMcpServer ready 数据结构字段** | team/mcp/server.rs | 20 | 0.2 |
| **W4-D24b-2** | 4 | **notify_mcp_ready 方法** | team/mcp/server.rs | 30 | 0.3 |
| **W4-D24b-3** | 4 | **wait_for_mcp_ready graceful select!** | team/mcp/server.rs | 50 | 0.5 |
| W4-D24c | 4 | Bridge 发 mcp_ready | app/bridge.rs | 30 | 0.2 |
| W5-D26a | 5 | `GuideMcpServer` 结构 + 启停 | team/guide/server.rs | 80 | 0.8 |
| **W5-D26b-1** | 5 | **`aion_create_team` args 解析 + 默认值（纯函数）** | team/guide/handlers.rs | 70 | 0.6 |
| **W5-D26b-2** | 5 | **`handle_aion_create_team` 调 service + 返回结构化** | team/guide/handlers.rs | 70 | 0.7 |
| W5-D26c | 5 | `handle_aion_list_models` handler | team/guide/handlers.rs | 40 | 0.3 |
| W5-D26d | 5 | 建团成功后 3 个 WS 事件 | team/guide/handlers.rs | 50 | 0.4 |
| W5-D27 | 5 | Guide stdio bridge 分支 | app/bridge.rs | 80 | 0.7 |
| W5-D28a | 5 | `is_team_capable_backend` 纯函数 | team/guide/capability.rs | 40 | 0.3 |
| W5-D28b | 5 | Guide prompt 注入到 instructions | ai-agent/acp_agent.rs | 60 | 0.5 |
| W5-D28c | 5 | `session/new.mcp_servers` 追加 Guide | ai-agent/acp_agent.rs | 60 | 0.5 |
| **W5-D29a-1** | 5 | **`SpawnAgentRequest` 类型 + spawn_agent 骨架** | team/session.rs | 30 | 0.2 |
| **W5-D29a-2** | 5 | **spawn_agent 校验：caller role = Lead** | team/session.rs | 20 | 0.2 |
| **W5-D29a-3** | 5 | **spawn_agent 校验：name 归一化 + 唯一性** | team/session.rs | 25 | 0.2 |
| **W5-D29a-4** | 5 | **spawn_agent 校验：backend 白名单** | team/session.rs | 25 | 0.2 |
| W5-D29b | 5 | `add_agent` 扩展用于 spawn | team/service.rs | 100 | 1.0 |
| **W5-D29c-1** | 5 | **spawn 后写 `extra.team_mcp_stdio_config`** | team/session.rs | 30 | 0.2 |
| **W5-D29c-2** | 5 | **spawn 后 kill + get_or_build_task** | team/session.rs | 50 | 0.4 |
| **W5-D29d-1** | 5 | **spawn 后写欢迎消息到 mailbox** | team/session.rs | 20 | 0.2 |
| **W5-D29d-2** | 5 | **spawn 后 wake 新 agent** | team/session.rs | 20 | 0.2 |
| **W5-D29d-3** | 5 | **spawn 后 emit `team.agentSpawned` 事件** | team/session.rs | 20 | 0.2 |
| **W5-D30a-1** | 5 | **识别 `shutdown_approved` 字符串** | team/mcp/server.rs | 20 | 0.2 |
| **W5-D30a-2** | 5 | **approved 处理：remove_agent + 通知 leader + wake** | team/mcp/server.rs | 60 | 0.5 |
| W5-D30b | 5 | `shutdown_rejected` 拦截 | team/mcp/server.rs | 50 | 0.4 |
| W5-D30c | 5 | `shutdown_agent` 目标 role 校验 | team/scheduler.rs | 30 | 0.2 |
| **W5-D30d-1** | 5 | **remove_agent：task_manager.kill** | team/scheduler.rs | 25 | 0.2 |
| **W5-D30d-2** | 5 | **remove_agent：清 active_wakes / wake_timeouts / finalized_turns** | team/scheduler.rs | 15 | 0.2 |
| **W5-D30d-3** | 5 | **remove_agent：slots 移除 + emit `team.agentRemoved`** | team/scheduler.rs | 25 | 0.2 |
| W5-D31a | 5 | `TeamMcpPhase` enum + payload 类型 | api-types/team.rs | 60 | 0.4 |
| **W5-D31b-1** | 5 | **`team.mcpStatus` tcp 层 2 点广播** | mcp/server.rs | 30 | 0.3 |
| **W5-D31b-2** | 5 | **`team.mcpStatus` service 层 6 点广播** | service.rs | 40 | 0.4 |
| **W5-D31b-3** | 5 | **`team.mcpStatus` bridge 层 2 点广播** | app/bridge.rs | 25 | 0.2 |
| W5-D31c | 5 | `teammate_message` 左气泡 emit | team/session.rs | 40 | 0.3 |

**合计**：87 人模块 · 约 51 人天（单人累计）。**粗体** = 二轮拆分新增的 21 个模块（D4b / D7a/b/c / D11.5 + Wave 4 的 7 个 + Wave 5 的 10 个）。标 ⚠️ 的 3 个模块（D5b-1 prompt txt / D5c / D9）为 Wave 1/2 已批准的 200 行例外；其余**全部 ≤ 120 行**。

**并行压缩**（更新版）：
- Wave 1 并行 10 人（D1/D2/D3/D4/D4b/D5a/D5b-1/D5b-2/D5c/D6） → 关键路径 1.5 天
- Wave 2 关键路径 D7a(1.5) → D7b(1.2) → D7c(0.3) → D11.5(0.3) 串行于 session.rs / service.rs（同文件不并行）；D8/D9/D10/D11 可并行；整 Wave 2 ≈ 4 天
- Wave 3：16 人并行，关键路径 W3-D13a → W3-D13b → W3-D13c ≈ 1.6 天
- Wave 4：底座 W4-D25a → W4-D25b → W4-D25c-1 → W4-D25c-2 ≈ 1.6 天；之后 17 人并行最长链 W4-D18b-1 → W4-D18b-2（1.2 天）+ D18c（0.5）= 1.7 天；W4-D24b 链 b-1/b-2/b-3 ≈ 1 天；Wave 4 整体 ≈ 3.5 天
- Wave 5：关键路径 D26a → D26b-1 → D26b-2 → D26d ≈ 2.5 天；D29 链 D29a-1..4(0.8) → D29b(1.0) → D29c-1/c-2(0.6) → D29d-1/d-2/d-3(0.6) ≈ 3.0 天；D29 / D30 / D31 部分并行；Wave 5 整体 ≈ 6 天
- **建议总工期**：6 周（Wave 1+2 = 2 周，Wave 3 = 0.5 周，Wave 4 = 1 周，Wave 5 = 1.5 周，buffer 1 周）

**可增加并行度**：
- Wave 3 关键路径是 D13 链，16 人并行已达极限
- Wave 4 底座 D25a/b/c-1/c-2 必须串行（同文件 acp_agent.rs）
- Wave 5 D29 链多人同改 session.rs，必须串 merge（同文件不并行）——这是"一人一模块"粒度最极致后的物理约束

---

## 5. 交付硬性要求（所有模块适用）

1. **测试先行**：每个模块必须先写 2–4 条单元/集成测试（见各模块"测试策略"行），再开工；PR 必须带测试证据
2. **禁 mock 逃课**：D4 的 descriptor 文本要对比 team-prompts.md 原文；D5 的 builder 要对比 AionUi 源码拷贝；D9/D11 的集成测试用 `MockWorkerTaskManager`（**合法 mock**，只为隔离真 ACP 进程）但必须 assert 调用顺序和参数
3. **一次性交付**：按 leader 规则 #4，每个模块交付 = 该开发者下线；有返工派新人接手（每人只处理一个模块一次）
4. **文档交接**：每个模块 PR 必须更新 [interface-contracts.md](./interface-contracts.md) 的对应 section 状态（TODO → Shipped）
5. **CLAUDE.md 规则**：符合"只管 backend / 只 ACP / 事实来源是审计报告"

---

## 6. 模块拆得更细的地方（预留给 leader 裁决）

如果 leader 审查后觉得某些模块"还能拆"，以下是进一步拆分方案：

- **D6 mcp-bridge** 可拆成 **D6a（argv + env 解析 + 主循环骨架）+ D6b（stdio↔tcp 转发 + 错误路径）**
- **D9 ensure_session** 可拆成 **D9a（ConversationService.update_extra 新接口 + 单元测试）+ D9b（ensure_session 改造）**
- **D10 acp_agent 注入** 已经 60 行，无需再拆
- **D11 app 装配** 可拆成 **D11a（state_builders 改签名）+ D11b（smoke test 编写）** —— 但 D11b 必须等 D11a，拆了不省时

**默认不拆**，按 §4 分配表执行；leader 若判例外再拆。
