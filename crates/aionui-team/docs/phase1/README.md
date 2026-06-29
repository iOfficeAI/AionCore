# aionui-backend Phase1 — 完整调研 + 全量开发计划

> **阶段目标**：phase1 = **完整调研 + backend 实现 AionUi 参考 team 能力的全量开发计划**。
>
> 后续 phase（phase2..N）= 按本 phase1 产出的图纸，一波波（Wave）实打实开工；phase1 **不为 Wave 之后的返工留回旋余地**——所有模块拆解、接口契约、里程碑验收标准都在 phase1 冻结。
>
> phase1 全部 Wave 开工顺序：
> - **Wave 1 / Wave 2** = 最小可跑闭环（一个 lead + 一个 teammate 能在对话里真的跑起来）
> - **Wave 3** = 规范化与轻量修复（多用户过滤 / agent 修复 / rename 规范化 / conversation 复用 / MCP 帧&超时）
> - **Wave 4** = 鲁棒性与可靠性（activeWakes / wakeTimeouts / finalizedTurns / crash / 429 / watchdog / addAgentLocks / mcp_ready 握手）
> - **Wave 5** = 业务闭环补全（Team Guide MCP + 真实 spawn + 真 kill + shutdown 协议 + teammate_message 左气泡 + team.mcpStatus 事件）
>
> **事实来源**：
> - [backend-audit.md](./backend-audit.md) — rebase 后的后端现状 + 55 条 GAP（P0=16, P1=31, P2=8）
> - [aionui-audit.md](./aionui-audit.md) — AionUi 能力清单（`ed8a6bcd3`）
>
> **开发文档**：
> - [modules.md](./modules.md) — 所有 Wave 的模块拆解 + 依赖拓扑 + 分配表
> - [interface-contracts.md](./interface-contracts.md) — 所有 Wave 的接口契约（各 Wave 开工前冻）
> - [milestones.md](./milestones.md) — 所有 Wave 的里程碑 + 验收证据

---

## 1. 硬约束（不可违反）

1. **只描述后端要做什么**（aionui-backend 仓内），不涉及 AionUi 参考实现的渲染/交互层
2. **只考虑 ACP agent type**（claude / codex 走 ACP；Gemini 走 ACP；aionrs 是 stub）
3. **一人一模块 ≤ 200 行**（[modules.md §2/§3/§7/§8/§9](./modules.md)；例外见下方"例外模块"清单）
4. **Wave 先拆纯净非业务，再拆业务串接**；底层不允许超级模块
5. **文档必须带交叉引用链接**（全局 CLAUDE.md 规则）
6. **所有结论基于审计报告的事实，禁止推测**（aionui-audit / backend-audit 都锚定到源码行号）

**拆分粒度要求**（team-lead 2026-04-29 确认，"拆到不能再拆"）：
- 一个模块一件事：职责描述里不允许出现"且/和/同时/并"；出现必须继续拆
- 每模块 LoC 目标 ≤ 100；超 100 行必须在模块详单的"不能再拆理由"行给出解释
- 一人一模块交付即下线，返工派新人

**二轮拆分后仅剩 3 个超 200 行例外模块**（Wave 1/2 历史遗留，leader 已批）：
- `D5b-1`（188 行 AionUi 原文 .txt + 48 行 Rust `include_str!`）——模板文本是原料不是逻辑
- `D5c`（~150 行 teammate prompt 模板 + builder）——模板原料 + 单一 builder
- `D9`（~200 行 ensure_session）——5 步原子启动流水线，中间失败需回滚整个 session，不可拆

原 D7（280 行）**已按 team-lead 二轮要求拆为 D7a + D7b + D7c**，不再属于例外。

Wave 3/4/5 共 **70 个新模块全部 ≤ 120 行**；其中超 100 行的仅有 W3-D15b(120) / W4-D22(100) / W5-D29b(100)，均在 modules.md 对应条目给出"不能再拆理由"。

---

## 2. Phase1 全量范围（按 Wave 划分）

### Wave 1（纯净原料，10 人并行）
> **开工准则**：彼此无依赖，单文件可交付。
- D1  `aionui-api-types::team_mcp` 子模块（类型种子）
- D2  `AcpBuildExtra.team_mcp_stdio_config` 字段
- D3  `TeamMcpStdioServerSpec`（bridge 入口 spec）
- D4  两个新 MCP 工具 descriptor + 最小 handler（`team_list_models` / `team_describe_assistant`）
- **D4b `TEAM_SPAWN_AGENT_DESCRIPTION` 原文常量**（补漏 P0#48，替换现有极简描述为 AionUi `toolDescriptions.ts` 原文） — [backend-audit §3.5 #48](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现)
- D5a `TEAM_GUIDE_PROMPT_TEMPLATE` 常量 + `build_team_guide_prompt()`
- D5b-1 Lead prompt 常量（`include_str!` 原文件）
- D5b-2 Lead prompt builder 实现
- D5c Teammate prompt + wake payload
- D6  `aionui-backend mcp-bridge` 子命令

详见 [modules.md §2](./modules.md#2-wave-1-模块详单每人--200-行)。

### Wave 2（最小闭环业务串接，8 人）
> **开工准则**：依赖 Wave 1 全部完成；D7 原 280 行例外被驳回，拆为 D7a + D7b + D7c（+ 合并 P0#45/#46）；新增 D11.5（P0#47）。关键路径 D7a → D7b → D7c → D11.5（全部在 session.rs / service.rs 同文件串行 merge）。
- D7a **TeamSession 三个新方法**（compute_wake_input / stdio_spec / on_agent_finish）
- D7b **send 路径接 wake + `files` 附件（P0#45）+ log-not-throw（P0#46）** — [backend-audit §3.5 #45/#46](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现)
- D7c **`send_message_to_agent(silent=true)` 占位**（Wave 5 MCP-spawn 用到）
- D8  Scheduler 首次 wake 区分 + `Pending` variant + settled 集合扩展
- D9  `TeamSessionService::ensure_session` 打通 kill+rebuild 闭环
- D10 `acp_agent::session_new_and_prompt` 注入 mcp_servers
- D11 `aionui-app` 装配 + e2e smoke test
- **D11.5 `remove_team` 级联 kill agent 进程**（补漏 P0#47，避免 agent 孤儿进程） — [backend-audit §3.5 #47](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现)

详见 [modules.md §3](./modules.md#3-wave-2-模块详单每人--200-行)。

### Wave 3（规范化与轻量修复，16 人并行）
> **开工准则**：依赖 Wave 2 merge；各子模块彼此独立可并行（除 D12b/c 等 D13a 提供的 repo trait）。
> **拆分原则**（team-lead 2026-04-29 确认）：一人一模块一件事，每模块 ≤ 100 行；超 100 必须说明"不能再拆"理由。
> **目标**：把所有"语义正确性 / 多用户隔离 / 协议硬上限"类 GAP 一次性补齐，不动 scheduler 内核。

- W3-D12a / W3-D12b / W3-D12c — user-scope 过滤三个方法拆三人（list / get / remove） — [backend-audit P1#31](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W3-D13a 新 `list_by_team_id` repo trait / W3-D13b `repair_team_agents_if_missing` 纯函数 / W3-D13c `get_team` 串接修复写回 — [backend-audit P1#37](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W3-D14a `normalize_name` 纯函数 / W3-D14b `rename_agent` 冲突 + renamed_agents 写入 / W3-D14c Prompt builder 读 renamed_agents — [backend-audit P1#24](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W3-D15a `CreateAgentRequest.conversation_id` 字段 / W3-D15b `create_team` 复用 conversation 分支 — [aionui-audit §1.1](./aionui-audit.md#11-能力清单) "单聊→team 的 conversation 复用"
- W3-D16a `ITeamMessageRouter` trait + 注入点 / W3-D16b `send_message` 路由分叉 / W3-D16c `TeamSessionService impl router` + 装配 — [backend-audit P1#32](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W3-D17a MCP 帧 64MB / W3-D17b tool call 300s 超时 — [backend-audit P1#35/#36](./backend-audit.md#32-p1能跑但体验差--关键硬约束)

**验收标准**（Wave 3 完工门禁）：
- ❏ 两个不同 userId 建的 team 互相不可见（W3-D12a/b/c 集成测试）
- ❏ `get_team` 在 team.agents 空数组时能从 conversation.extra 反推恢复（W3-D13a/b/c）
- ❏ rename 后 `## Your Teammates` prompt 段显示 `[formerly: X]`（W3-D14a/b/c）
- ❏ 单聊会话通过 REST 建团后该 conversation 仍存在且 extra.team_id 被写入（W3-D15a/b）
- ❏ `POST /api/conversations/{id}/messages` 对 team 成员 conversation 的请求会路由到 team 发送路径（W3-D16a/b/c）
- ❏ 64MB 大 JSON tool 响应不被拒（W3-D17a）+ 空 tool handler 300s 超时返回 error（W3-D17b）

### Wave 4（鲁棒性与可靠性，22 人，W4-D25 底座链先行 → 其余并行）
> **开工准则**：依赖 Wave 2；所有订阅型模块必须先让 D25a/b/c-1/c-2 完成 chunk 订阅公共底座。
> **拆分原则**：同 Wave 3。二轮拆分后 D18b/D20b/D24b/D25c 全部按 tokio 并发原语 / 状态清理 / 协议层各自的自然边界拆开。
> **目标**：把 AionUi §8 列的 17 条硬约束里 phase1 Wave 2 跳过的 8 条全部落地。

- W4-D25a `AgentStreamChunk` enum / W4-D25b `subscribe_stream()` trait / W4-D25c-1 **broadcast channel 字段 + subscribe_stream impl** / W4-D25c-2 **5 个 chunk emit 点注入** — [backend-audit P1#53](./backend-audit.md#35-交叉审阅补漏二轮对照-aionui-audit-7-8-后新发现)
- W4-D18a `active_wakes` 重入锁 / W4-D18b-1 **wake_timeouts 存储字段 + clear** / W4-D18b-2 **arm_wake_timeout spawn task（select! 主体）** / W4-D18c session 接入 wake lock — [aionui-audit §8 #1/#2](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点)
- W4-D19a `finalized_turns` 存储 + API / W4-D19b session 接入 dedup — [aionui-audit §8 #3](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点)
- W4-D20a `detect_crash` 纯函数 / W4-D20b-1 **非 leader crash：写 testament helper** / W4-D20b-2 **非 leader crash：kill + 清 state + wake leader** / W4-D20c leader crash 分支 — [backend-audit P1#17](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W4-D21 429 / rate-limit 识别 — [backend-audit P1#18](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W4-D22 Inactivity watchdog handler — [aionui-audit §2.1 inactivity watchdog](./aionui-audit.md#21-能力清单)
- W4-D23 `add_agent_locks` per-team 串行化 — [backend-audit P1#19](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W4-D24a `McpReadyNotification` 协议类型 / W4-D24b-1 **ready 数据结构字段** / W4-D24b-2 **notify_mcp_ready 方法** / W4-D24b-3 **wait_for_mcp_ready graceful select!** / W4-D24c bridge 发 mcp_ready — [backend-audit P1#34](./backend-audit.md#32-p1能跑但体验差--关键硬约束)

**验收标准**（Wave 4 完工门禁）：
- ❏ 并发 wake 同 slot 只跑一次（W4-D18a 单元测试）
- ❏ 60s 无 chunk 触发 timeout handler；chunk 到达 reset（W4-D18b）
- ❏ 同一 turn 多次 Finish 事件只触发一次 finalize（W4-D19a/b）
- ❏ agent 进程假崩（kill -9 子进程）后 leader 收到 testament 并 failed（W4-D20a/b）
- ❏ leader 自己 crash 只 failed 不 remove（W4-D20c）
- ❏ mock ACP 返回 429 text → agent 进入 Failed 状态不走 crash（W4-D21）
- ❏ mock teammate 静默 > 60s 无 chunk → agent failed + idle_notification（W4-D22）
- ❏ 并发 `add_agent` 10 次 → agents 数组长度 10（W4-D23）
- ❏ bridge 发 `mcp_ready` 后 server 立即解锁；30s 无 ready 仍 graceful resolve（W4-D24a/b/c）
- ❏ `subscribe_stream()` 订阅能收到 Text / ToolUse / Thought / Finish 全部 chunk（W4-D25a/b/c）

### Wave 5（业务闭环补全，31 人）
> **开工准则**：依赖 Wave 3（conversation 复用、send 识别 team_id）+ Wave 4（activeWakes / finalized_turns / watchdog 给 spawn/shutdown 用）。
> **拆分原则**：同 Wave 3。二轮拆分后 D26b 拆 2、D29a 拆 4、D29c 拆 2、D29d 拆 3、D30a 拆 2、D30d 拆 3、D31b 拆 3。
> **目标**：让"单聊→建团→真 spawn→真 kill"四条完整链路全部闭环，等价 AionUi 参考实现。

- W5-D26a `GuideMcpServer` 启停 / W5-D26b-1 **`aion_create_team` args 解析 + 默认值** / W5-D26b-2 **`handle_aion_create_team` 调 service + 返回结构化** / W5-D26c `handle_aion_list_models` / W5-D26d 建团后 3 个 WS 事件 — [backend-audit P1#28/#33](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W5-D27 Team Guide stdio bridge 分支 — [backend-audit P1#28](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W5-D28a `is_team_capable_backend` 纯函数 / W5-D28b Guide prompt 注入到 instructions + 互斥 guard / W5-D28c `session/new.mcp_servers` 追加 Guide + 互斥 guard — [backend-audit P1#29/#30/#52](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W5-D29a-1 **SpawnAgentRequest 类型 + 骨架** / W5-D29a-2 **校验 caller = Lead** / W5-D29a-3 **校验 name 归一化 + 唯一性** / W5-D29a-4 **校验 backend 白名单** / W5-D29b `add_agent` 扩展 / W5-D29c-1 **写 extra** / W5-D29c-2 **kill + get_or_build_task** / W5-D29d-1 **欢迎消息** / W5-D29d-2 **wake 新 agent** / W5-D29d-3 **emit `team.agentSpawned` 事件** — [backend-audit P1#22](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W5-D30a-1 **识别 `shutdown_approved` 字符串** / W5-D30a-2 **approved 处理 remove_agent + 通知 + wake** / W5-D30b `shutdown_rejected` 拦截 / W5-D30c `shutdown_agent` 目标 role 校验 / W5-D30d-1 **remove_agent：kill** / W5-D30d-2 **remove_agent：清 3 种 state** / W5-D30d-3 **remove_agent：slots 移除 + event** — [backend-audit P1#21/#23/P0#20](./backend-audit.md#32-p1能跑但体验差--关键硬约束)
- W5-D31a `TeamMcpPhase` enum + payload 类型 / W5-D31b-1 **mcpStatus tcp 层 2 点广播** / W5-D31b-2 **mcpStatus service 层 6 点广播** / W5-D31b-3 **mcpStatus bridge 层 2 点广播** / W5-D31c `teammate_message` 左气泡 emit — [backend-audit P2#42 / P1#27](./backend-audit.md)

**验收标准**（Wave 5 完工门禁）：
- ❏ solo agent 能调 `aion_create_team` 建出 team 且 leader 被正确复用（W5-D26a/b/c/d e2e）
- ❏ 进了 team 的 agent **不再注入** Guide prompt + mcp_servers（W5-D28b/c 互斥断言）
- ❏ leader 调 `team_spawn_agent` 后 team.agents 长度 +1 且新 agent 真的被 wake（W5-D29a/b/c/d e2e）
- ❏ leader 调 `team_shutdown_agent` + teammate 回 `shutdown_approved` → agent 进程真被 kill（W5-D30a/d e2e）
- ❏ teammate 回 `shutdown_rejected: reason` → agent 未被 kill + leader 邮箱有 reason（W5-D30b）
- ❏ leader 被传为 shutdown target → 拒绝（W5-D30c）
- ❏ `team.mcpStatus` 的 10 个 phase 至少有 `tcp_ready / session_ready / mcp_tools_ready` 能在正常启动时观察到（W5-D31a/b）
- ❏ teammate 收到的 mailbox message 额外 emit `conversation.responseStream { type: 'teammate_message' }`；Lead 不 emit（W5-D31c）

---

## 3. Smoke Test 验收脚本（仅 Wave 2 的 8 步闭环）

phase1 的**Wave 2 完工**证据 = 以下脚本跑通（对应 [milestones.md §M3](./milestones.md#m3--真-acp-闭环-smoke-跑通证据--眼见为实)）；Wave 3/4/5 各自有独立的 smoke 见 [milestones.md](./milestones.md)。

```
前置：
- 本地安装 `claude --experimental-acp`
- 编译 `cargo build --release`，跑 `./target/release/aionui-backend`
- 登录 + 取 WS token（`POST /api/ws-token`）+ 订阅 `team.*` 事件

1. POST /api/teams
   body: {
     "name": "smoke-team",
     "agents": [
       {"name":"lead",  "role":"leader",   "backend":"acp","model":"claude-sonnet-4"},
       {"name":"coder", "role":"teammate", "backend":"acp","model":"claude-sonnet-4"}
     ]
   }
   expect: 201，返回 team_id

2. POST /api/teams/{team_id}/session
   expect: 200
   assert: WS 收到两次 team.agentStatusChanged（lead / coder 都 Pending 或 Idle）
   assert: sqlite DB 查 2 个 agent 的 conversation.extra → 均含 team_mcp_stdio_config {port, token, slot_id}

3. POST /api/teams/{team_id}/messages
   body: {"content": "请创建一个任务叫 'hello'，让 coder 处理，然后让 coder 做完就好"}
   expect: 200

4. 15s 内 WS 收到 team.agentStatusChanged{slot_id=lead, status="Working"}

5. 60s 内 GET /api/teams/{team_id}.tasks.length >= 1（lead 调了 team_task_create）

6. 60s 内 MCP server log 出现 tools/call team_send_message 或 team.agentStatusChanged{slot_id=coder, status="Working"}

7. 120s 内 WS 收到 coder 的 Finish（通过 team.agentStatusChanged 回到 Idle 观察）

8. 120s 内 WS 收到 lead 的第二次 Working（leader 在 coder idle 后被 re-wake，maybe_wake_leader_when_all_idle）
```

**证据要求**（缺一项不算通过）：
- [ ] 抓取的 WS 事件流 txt 日志（含时间戳）
- [ ] `sqlite3 aionui.db "SELECT id, extra FROM conversations WHERE extra LIKE '%team_mcp%'"` 输出
- [ ] 后端 `RUST_LOG=aionui_team=debug,aionui_ai_agent::manager::acp=debug` 运行日志（含 MCP tools/call 行）
- [ ] 每个断言（1–8 步）对应的期望 + 实际值的 diff

---

## 4. 总工作量估算

| Wave | 目标 | 模块数 | 关键路径 | 总人天（单人累计） |
|------|------|:-:|:-:|:-:|
| Wave 1（原料） | 最小闭环纯净模块 + 补 D4b 工具描述原文（P0#48） | 10 | 1.5 天 | 6.9 人天 |
| Wave 2（串接） | 最小闭环业务 + D7 拆 3 + D11.5（P0#45/#46/#47 补漏） | 8 | 5 天 | 10.6 人天 |
| Wave 3（规范化） | 多用户/协议硬上限 | 16 | 2 天 | 7.5 人天 |
| Wave 4（鲁棒性） | scheduler 可靠性 | 22 | 3.5 天 | 13.2 人天 |
| Wave 5（闭环补全） | Guide MCP / spawn / shutdown / 10-phase 事件 | 31 | 6 天 | 12.7 人天 |
| **合计** | — | **87 人模块** | **~18 日历日** | **~51 人天** |

**建议工期**：6 周（Wave 1+2 = 2 周，Wave 3 = 0.5 周，Wave 4 = 1 周，Wave 5 = 1.5 周，buffer 1 周覆盖 D7a/b/c 同文件串行 + 真 CLI 跑不起来降级）。

> Wave 5 模块数从 19 → 31，因为：D26b 拆 2、D29a 拆 4、D29c 拆 2、D29d 拆 3、D30a 拆 2、D30d 拆 3、D31b 拆 3（共 +12；另 W5-D27/28/29b/30c 保持不动）。

各 Wave 分配表详见 [modules.md §4](./modules.md#4-分配表)。

---

## 5. 关键设计决策（已确认，直接用）

| 决策 | 内容 | 作用域 | 来源 |
|------|------|:-:|------|
| stdio bridge 打包 | 打进主二进制 `aionui-backend mcp-bridge` subcommand；Wave 5 Guide bridge 复用同一 subcommand 加 `--guide` 分支 | W1 / W5 | team-lead 确认 · [mcp.md §4.6](../mcp.md#46-stdio-bridge-方案打进主二进制) |
| MCP 注入方式 | 走 stdio：`session/new` payload 的 `mcpServers` 数组加一项 | W1 / W5 | [mcp.md §4.4](../mcp.md#44-acp-注入链路stdio-注入方式) |
| Agent 进程重启 | `IWorkerTaskManager::kill` + `get_or_build_task`，不引入 `skipCache`；conversation 不变，agent 进程换，session 走 resume | W2 / W5 | [mcp.md §4.3](../mcp.md#43-agent-进程重启机制mcp-动态注入的关键) |
| Prompt 注入路径 | **wake 时作为首个 send_message content**（不走 preset_context） | W2 / W5 | team-lead 确认 · [aionui-audit §2.1](./aionui-audit.md#21-能力清单) wake 时机 |
| AcpBuildExtra 字段 | `team_mcp_stdio_config: Option<TeamMcpStdioConfig>`（snake_case） | W1 | [interface-contracts §2](./interface-contracts.md#2-aionui-ai-agenttypesacpbuildextra-扩展wave-1--模块-d2) |
| 后端 Prompt 文本 | 必须**原样**复用 AionUi `leadPrompt.ts` / `teammatePrompt.ts` / `teamGuidePrompt.ts`，禁翻译禁改写 | 所有 Wave | [aionui-audit §8 #5](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点) |
| MCP 工具描述 | 12 个工具的 description 文本来自 [team-prompts.md §5](../team-prompts.md#5-mcp-tool-description-原文后端必须原样复用) 原文 | 所有 Wave | 同上 |
| Wave 1 两个新工具最小实现 | `team_list_models` 返回硬编码列表；`team_describe_assistant` 返回 "Preset not found" | W1 → W5 接数据源 | [modules.md D4](./modules.md#d4--两个-mcp-工具-team_list_models--team_describe_assistant) |
| Conversation 复用范围 | phase1 只实现 REST 复用（Wave 3 W3-D15）；`aion_create_team` MCP 路径等 Wave 5 W5-D26 | W3 / W5 | [aionui-audit §1.1](./aionui-audit.md#11-能力清单) |
| `conversation.send_message` 识别 team_id | Wave 3 W3-D16 实现；读 `extra.team_id` 后委托 `TeamSessionService` | W3 | team-lead 修正（原 phase1 Out） |
| Guide MCP 与 team 内部 MCP 互斥 | `!extra.team_mcp_stdio_config` 才注入 Guide；写死在注入链路里，Wave 5 实现 | W5 | [aionui-audit §8 #17](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点) |
| `team.mcp_status` 10 phase | Wave 5 W5-D31 完整落地；Wave 2 只保留占位不 emit | W5 | [aionui-audit §7.6](./aionui-audit.md#76-事件--ipc后端等价需提供-websocket-或-sse) |
| `TeammateStatus::Pending` | Wave 2 D8 新增 variant 区分"首次 wake"；`failed` **不**会回退到 Pending（状态机：`failed` 保持 `failed`；下次 wake 时 `status in {pending, failed}` 直接触发 role prompt 注入；inactivity / crash / 429 三条路径都只 `set_status(Failed)` 不回退） | W2 | [aionui-audit §2.1 状态机](./aionui-audit.md#22-agent-生命周期状态机) |

---

## 6. 进度追踪（开工后更新）

| 里程碑 | Wave | 状态 | 证据 |
|--------|:-:|:-:|------|
| M0 接口契约冻结 | W1/W2 | ⬜ | — |
| M1 Wave 1 全部 merge | W1 | ⬜ | — |
| M2 Wave 2 骨架打通 | W2 | ⬜ | — |
| M3 真 ACP smoke 跑通 | W2 | ⬜ | — |
| M4 W2 收尾 + 最小闭环交付 | W2 | ⬜ | — |
| M5 Wave 3 merge | W3 | ⬜ | — |
| M6 Wave 4 merge + 可靠性 smoke | W4 | ⬜ | — |
| M7 Wave 5 merge + 全量 e2e | W5 | ⬜ | — |
| M8 Phase1 全量交付 | 全部 | ⬜ | — |

详见 [milestones.md](./milestones.md)。

---

## 7. 开发者入口

- 拿到模块任务后：先读 [interface-contracts.md](./interface-contracts.md) 的对应 section（一句话记住签名） → 再看 [modules.md](./modules.md) 的对应模块单元 → 再看 [milestones.md](./milestones.md) 看本模块属于哪个里程碑
- 所有 PR 必须带测试 + 更新 interface-contracts.md 的状态（TODO → Shipped）
- 交付即下线（每个人只处理一个模块一次，返工派新人）
- Wave N+1 的开发者开工前必须确认 Wave N 对应的 merge 门禁已过（见 [milestones.md](./milestones.md) 各 milestone 的"验收证据"节）

---

## 8. 版本锚定

- AionUi 源码 commit：`ed8a6bcd3`（aionui-audit 锚定）
- aionui-backend 分支：`docs/api-for-frontend`，HEAD `21abc46`（backend-audit 锚定）
- Phase1 规划产出时间：2026-04-29（Wave 1/2 + Wave 3/4/5 全量计划同日产出）
