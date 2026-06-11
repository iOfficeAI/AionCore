# Phase1 里程碑

> **原则**：每个里程碑必须有"可观测的证据"（命令输出 / 日志 / 截图 / WS 事件流 / 测试通过日志）才能声明完成。凭代码 review 说"看起来对"不算。
>
> **相关文档**：[README.md](./README.md) · [modules.md](./modules.md) · [interface-contracts.md](./interface-contracts.md)
>
> **事实来源**：[backend-audit.md §3.4](./backend-audit.md#34-p0-gap-连锁关系修正版) 的 P0 连锁图 · [aionui-audit.md §2.1](./aionui-audit.md#21-能力清单)

---

## 0. 里程碑全景图

```
M0 共识冻结 → M1 W1 完工 → M2 W2 骨架 → M3 W2 闭环跑通 → M4 W2 收尾
(0.5 天)      (1 天)        (3 天)       (2 天)              (1 天)
                                                               │
                                                               ▼
                                           M5 Wave 3 merge（并行可靠性 + 规范化分叉）
                                           (2 天)
                                                               │
                                                               ▼
                                           M6 Wave 4 merge + 可靠性 smoke
                                           (4 天)
                                                               │
                                                               ▼
                                           M7 Wave 5 merge + 全量 e2e
                                           (10 天)
                                                               │
                                                               ▼
                                           M8 Phase1 全量交付
                                           (1 天)
```

每个里程碑的尾部是一次**"停下来验收"**节点，不通过不进下一个。Wave 3 / Wave 4 可以并行推进（M5 和 M6 独立），只要 Wave 2 的 M4 通过。

---

## M0 — 接口契约冻结（开工前必过）

**时点**：所有 Wave 1 开工前。

**产出物清单**：
- ✅ [interface-contracts.md](./interface-contracts.md) 已写完（本次产出的一部分）
- ⬜ 所有 Wave 1 模块认领人 Ack 过 §1–§8 的签名，**文字确认"按这个签名开工"**
- ⬜ SDK `McpServer` variant 形状由 D3 先读一次真实源码并在 PR 注释里贴出 —— [backend-audit.md §9 第 1 项](./backend-audit.md#9-仍需进一步确认的事项) 标为必须先确认

**验收证据**：
1. team lead 私聊收到每个 D1–D6 开发者的"已读并 ack"回执
2. D3 在 `crates/aionui-team/src/mcp/bridge.rs` PR 里附一段注释：`// SDK McpServer variant shape (as of agent-client-protocol-schema 0.12.0): ...`

**不通过的典型信号**：
- 任何开发者说"我觉得这个签名不对" → 暂停，重开讨论，改 interface-contracts.md 后再开工
- D3 发现 SDK 的 stdio variant 不是预期的 `{command, args, env}` → 更新 §3 的 `into_sdk()` 方案

**依赖**：无。

---

## M1 — Wave 1 全部 merge（并行原料完工）

**时点**：M0 后约 1 天（并行 10 人）。

**产出物清单**（10 个模块）：
- ⬜ D1 `aionui-api-types::team_mcp` 合并 + 单元测试绿
- ⬜ D2 `AcpBuildExtra.team_mcp_stdio_config` 字段合并 + 旧 extra 反序列化成 None 的测试绿
- ⬜ D3 `TeamMcpStdioServerSpec` 合并 + 3 条单元测试绿
- ⬜ D4 `team_list_models` + `team_describe_assistant` descriptor + 最小 handler 合并；descriptor 文本与 team-prompts.md §5.2 逐字节一致
- ⬜ **D4b `TEAM_SPAWN_AGENT_DESCRIPTION` 原文常量合并 + `diff -w` 证明零差异**（P0#48 补漏）
- ⬜ D5a/b-1/b-2/c 四个 prompt 子模块合并；快照测试绿；模板文本来自 AionUi 源码（非大模型生成）
- ⬜ D6 `aionui-backend mcp-bridge` 子命令合并；独立集成测试通过（spawn 子进程 + mock TCP 收到 `auth_token`）

**验收证据**：
1. `cargo test --workspace` 全绿（列出新增用例：`team_mcp_config_roundtrip`、`acp_build_extra_team_mcp`、`stdio_spec_env`、`team_list_models_descriptor_matches`、`build_lead_prompt_with_preset_assistants`、`mcp_bridge_forwards_tools_list` 等）
2. `cargo clippy --workspace -- -D warnings` 无新增 warning
3. 测试报告粘贴到 phase1 PR 描述里

**关键对齐点**：
- D4 descriptor 对齐：把 team-prompts.md §5.2 中 `team_list_models` / `team_describe_assistant` 的 description 原文复制到 Rust 常量，跑 `diff` 脚本证明零差异
- D5 prompt 对齐：同样方法对 `LEAD_PROMPT_TEMPLATE` / `TEAMMATE_PROMPT_TEMPLATE` / `TEAM_GUIDE_PROMPT_TEMPLATE` 三个常量做 diff，证据贴在 PR 里

**不通过的典型信号**：
- 任何 descriptor 或 prompt 常量"改写成了更清晰的版本" → 驳回，按 AionUi 原文重改（[aionui-audit §8 #5](./aionui-audit.md#8-源码中发现的硬约束agent-行为易坏点)）
- D6 bridge 丢弃了 `auth_token` 没有带入 TCP 请求 → 驳回

**依赖**：M0 通过。

---

## M2 — Wave 2 骨架打通（能跑不能证明正确）

**时点**：M1 后 4 天（D7 拆 3 个 + D11.5 新增；session.rs / service.rs 同文件串行 merge）。

**产出物清单**（8 个模块）：
- ⬜ **D7a `TeamSession` 三个新方法合并 + 4 条测试绿**（compute_wake_input / stdio_spec / on_agent_finish）
- ⬜ **D7b send 路径接 wake + `files` 附件 + log-not-throw 合并 + 5 条测试绿**（P0#45/#46）
- ⬜ **D7c `send_message_to_agent(silent=true)` 占位合并 + 2 条测试绿**
- ⬜ D8 `TeammateStatus::Pending` + 首次 wake 区分合并 + 3 条单元测试绿（**修正**：failed 不 reset 回 Pending，直接作为 settled 成员）
- ⬜ D9 `TeamSessionService::ensure_session` kill+rebuild 闭环合并 + MockWorkerTaskManager 集成测试绿
- ⬜ D10 `acp_agent::session_new_and_prompt` 注入合并 + 单元测试证明 mcp_servers 数组长度
- ⬜ D11 `build_team_state` 签名扩展合并 + `aionui-backend mcp-bridge` 在缺 env 时 1s 内退出且 exit code 非零
- ⬜ **D11.5 `remove_team` 级联 kill agent 进程合并 + 2 条集成测试绿**（P0#47 补漏，MockWorkerTaskManager kill 被调 N 次）

**验收证据**：
1. `cargo test --workspace` 全绿
2. `cargo clippy --workspace -- -D warnings` 无新增 warning
3. `cargo build --release` 成功产出 `aionui-backend` 二进制
4. 手工执行：`./target/release/aionui-backend mcp-bridge` 在无 env 时报错退出（证明 bridge 入口连通）

**关键对齐点**：
- D9 的 `ensure_session` 闭环必须在集成测试里用 `MockWorkerTaskManager` 断言**顺序**：先 update_extra，再 kill，最后 get_or_build_task；顺序错会导致新进程读到旧 extra
- D7 的 `on_agent_finish` **单独测试** `finalize_turn` 的 5s dedup 和 leader re-wake（即使 phase1 不上全部 P1 可靠性，这两条是闭环跑通的硬前提）

**不通过的典型信号**：
- D9 集成测试里 mock 没 assert 调用顺序 → 驳回要求补
- D7 的 send 路径没有 `compute_wake_input` 返回 `should_send=false` 时的 skip 分支 → 驳回

**依赖**：M1 通过。

---

## M3 — 真 ACP 闭环 smoke 跑通（证据 = 眼见为实）

**时点**：M2 后 2 天。

**产出物清单**：
- ⬜ D11 smoke test `crates/aionui-app/tests/team_phase1_smoke.rs` 合并
- ⬜ 本地机器安装 `claude --experimental-acp` 并跑通 smoke
- ⬜ 测试日志 / WS 事件流贴到 phase1 PR

**smoke test 脚本**（对应 [README.md §3](./README.md) 的 8 步）：

```
Step 1  POST /api/teams           — 创建 team: {name:"smoke-team", agents:[{name:"lead",backend:"acp",model:"claude-sonnet-4"}, {name:"coder",backend:"acp",model:"claude-sonnet-4"}]}
Step 2  POST /api/teams/{id}/session — ensure_session 返回 200
Step 3  断言 1：看 WS 事件 team.agentStatusChanged，两个 agent 都出现 Idle（或 Pending）
Step 4  断言 2：读 conversation.extra 的 DB 记录（直接用 sqlx），每个 agent 的 extra 含 team_mcp_stdio_config
Step 5  POST /api/teams/{id}/messages {content:"请创建一个任务 hello，让 coder 处理"}
Step 6  断言 3：15s 内 WS 出现 team.agentStatusChanged.Working（lead 启动）
Step 7  断言 4：60s 内 POST /api/teams/{id} 返回的 team.tasks 长度 ≥ 1（lead 真的调了 team_task_create 工具）
Step 8  断言 5：60s 内 WS 出现 team.agentStatusChanged.Working（coder 被 wake）或 lead 调过 team_send_message 的 MCP server log
```

**验收证据**：
1. smoke test 脚本跑通 → 在本地终端截图（含前 5 秒到 60 秒的时间戳）
2. `sqlite3 aionui.db "SELECT extra FROM conversations WHERE id IN (...)"` 能看到 `team_mcp_stdio_config` 字段
3. WS 事件序列抓取成文本日志（`wscat` 或后端自己的 `/api/ws-token` + client）
4. MCP server TCP log（开 `RUST_LOG=aionui_team::mcp=debug` 跑）能看到至少一次 `tools/call team_task_create`

**不通过的典型信号**：
- `ensure_session` 后 extra 里没有 `team_mcp_stdio_config` → 回退到 M2 的 D9
- lead 进程启动了但调不到 `team_*` 工具（报错 "unknown tool"）→ 回退到 M2 的 D10 或 M1 的 D6
- lead 调到工具但 coder 从没被 wake → 回退到 M2 的 D7 的 send 路径
- Finish 事件后 leader 没被 re-wake → 回退到 M2 的 D7/D9 的 Finish 订阅

**依赖**：M2 通过 + 本地可用的 ACP CLI。

---

## M4 — Wave 2 收尾（最小闭环交付节点，非 phase1 终点）

**时点**：M3 后 1 天。

**产出物清单**：
- ⬜ Wave 1 + Wave 2 所有 PR rebase 到 main 并合并
- ⬜ `cargo build --release` 无 warning
- ⬜ `cargo test --workspace` 全绿（包含 smoke 标记为 `#[ignore]` —— 真 CLI 测试不进 CI，但代码必须 compile）
- ⬜ [README.md](./README.md) 状态区更新："Wave 1 / Wave 2 / Smoke Test" 三项 ✅
- ⬜ [interface-contracts.md](./interface-contracts.md) §1–§10 状态更新为 "Shipped"
- ⬜ Wave 3 / Wave 4 的 §13–§26 接口契约解冻；对应开发者 ack 自己的模块
- ⬜ Wave 3 / Wave 4 可以开始分配人 + 并行开工

**验收证据**：
1. main 分支跑一次 CI → 绿
2. 后端独立 e2e 手工跑一遍 smoke（证据和 M3 一致）
3. 文档页面 `docs/teams/phase1/README.md` 所有链接都能点开
4. Wave 3 / Wave 4 开发者在团队频道发 "ack" 回执

**不通过的典型信号**：
- 任何 PR 带 `TODO: phase1 返工` 注释 → 修完再进 M4
- smoke test 本地机换了一台跑不起来 → 不算 M3，打回

**依赖**：M3 通过。

---

## M5 — Wave 3 全部 merge（规范化与轻量修复）

**时点**：M4 后 2 天（16 人并行；关键路径 D13a→D13b→D13c ≈ 1.6 天 + D16a→D16b→D16c ≈ 1.7 天）。

**产出物清单**：
- ⬜ W3-D12a list_teams 合并 + 1 条测试绿
- ⬜ W3-D12b get_team 归属校验合并 + 1 条测试绿
- ⬜ W3-D12c remove_team 归属校验合并 + 1 条测试绿
- ⬜ W3-D13a `list_by_team_id` repo trait 合并 + 2 条测试绿
- ⬜ W3-D13b `repair_team_agents_if_missing` 纯函数合并 + 2 条测试绿
- ⬜ W3-D13c `get_team` 串接修复写回合并 + 1 条测试绿
- ⬜ W3-D14a `normalize_name` 纯函数合并 + 3 条测试绿
- ⬜ W3-D14b `rename_agent` 冲突 + renamed_agents 合并 + 3 条测试绿
- ⬜ W3-D14c prompt builder 读 renamed_agents 合并 + 2 条快照测试绿
- ⬜ W3-D15a `CreateAgentRequest.conversation_id` 字段合并 + 2 条测试绿
- ⬜ W3-D15b `create_team` 复用分支合并 + 3 条测试绿
- ⬜ W3-D16a `ITeamMessageRouter` trait + 注入点合并 + 1 条测试绿
- ⬜ W3-D16b `send_message` 路由分叉合并 + 3 条测试绿
- ⬜ W3-D16c `TeamSessionService impl router` + 装配合并 + 2 条集成测试绿
- ⬜ W3-D17a 帧 64MB 合并 + 2 条边界测试绿
- ⬜ W3-D17b tool call 300s 超时合并 + 1 条测试绿

**验收证据**：
1. `cargo test --workspace` 全绿
2. `cargo clippy --workspace -- -D warnings` 无新增 warning
3. 多用户隔离手工验证：两个 session 分别登录 user_a / user_b 各建 team，互看都是 NotFound
4. `conversation.send_message` 发给 team 成员 conversation → 能看到 `RUST_LOG=aionui_conversation=debug` 里 `routing to team path` 的日志

**关键对齐点**：
- W3-D16 的 trait `ITeamMessageRouter` 放 `aionui-conversation` 或中间层，**禁止** `aionui-conversation` 反向依赖 `aionui-team`
- W3-D14 的 `renamed_agents` map 要能被 W2 D5 的 prompt builder 读到；若 D5 在 W2 阶段未预留参数，M5 里回头补 builder 参数（按 [interface-contracts.md §5](./interface-contracts.md#5-aionui-teamprompts-大幅扩写wave-1--模块-d5) 本来就有）

**不通过的典型信号**：
- W3-D12 的实现把越权访问暴露成 `Forbidden` 而不是 `NotFound` → 驳回（信息泄漏）
- W3-D16 的 trait 误放 `aionui-team` 导致 `aionui-conversation → aionui-team` 循环依赖 → 驳回
- W3-D17 的常量写死在 team crate 而非 common crate → 驳回（后续其他 MCP 复用要）

**依赖**：M4 通过；Wave 4 可并行推进不阻塞。

---

## M6 — Wave 4 全部 merge + 可靠性 smoke

**时点**：M4 后 3.5 天（底座链 D25a→D25b→D25c-1→D25c-2 约 1.6 天 → 其他并行 2 天）。**可与 M5 并行**。

**产出物清单**（22 个子模块）：
- ⬜ W4-D25a `AgentStreamChunk` enum 合并 + 1 条 serde 测试绿
- ⬜ W4-D25b `subscribe_stream()` trait 方法合并 + 1 条 trait 对象测试绿
- ⬜ **W4-D25c-1 broadcast channel 字段 + subscribe_stream impl 合并 + 2 条测试绿**
- ⬜ **W4-D25c-2 五个 chunk emit 点注入合并 + 3 条测试绿**
- ⬜ W4-D18a `active_wakes` 重入锁合并 + 2 条并发测试绿
- ⬜ **W4-D18b-1 wake_timeouts 存储字段 + clear 合并 + 2 条测试绿**
- ⬜ **W4-D18b-2 arm_wake_timeout spawn task（select! 主体）合并 + 3 条测试绿**
- ⬜ W4-D18c session 接入 wake lock 合并 + 2 条集成测试绿
- ⬜ W4-D19a `finalized_turns` 存储合并 + 3 条测试绿
- ⬜ W4-D19b session 接入 dedup 合并 + 2 条集成测试绿
- ⬜ W4-D20a `detect_crash` 纯函数合并 + 4 条单元测试绿
- ⬜ **W4-D20b-1 非 leader crash：写 testament helper 合并 + 2 条测试绿**
- ⬜ **W4-D20b-2 非 leader crash：kill + 清 state + wake leader 合并 + 3 条测试绿**
- ⬜ W4-D20c leader crash 分支合并 + 2 条测试绿
- ⬜ W4-D21 429 识别合并 + 3 条测试绿
- ⬜ W4-D22 inactivity watchdog handler 合并 + 3 条测试绿
- ⬜ W4-D23 `add_agent_locks` 合并 + 2 条并发压测绿
- ⬜ W4-D24a `McpReadyNotification` 协议类型合并 + 1 条 serde 测试绿
- ⬜ **W4-D24b-1 ready 数据结构字段合并 + 1 条测试绿**
- ⬜ **W4-D24b-2 notify_mcp_ready 方法合并 + 2 条测试绿**
- ⬜ **W4-D24b-3 wait_for_mcp_ready graceful select! 合并 + 3 条测试绿**
- ⬜ W4-D24c bridge 发 mcp_ready 合并 + 1 条集成测试绿
- ⬜ **可靠性 smoke test** `crates/aionui-app/tests/team_reliability_smoke.rs` 通过

**可靠性 smoke test 脚本**：

```
前置：M5 / M4 已合并
1. 建 team（2 agent，lead + teammate）+ ensure_session
2. 发消息给 lead 启动一轮
3. kill -9 teammate 的 claude 子进程（模拟 agent crash）
   → 断言：30s 内 WS 出现 team.agentStatusChanged{teammate, Failed}
   → 断言：leader mailbox 有 testament message
   → 断言：leader 被 wake（再次出现 Working）
4. 复位，重建 teammate（手工 HTTP removeAgent + addAgent）
5. 模拟 429：mock 后端返回带 "HTTP 429" 文本的 Error chunk
   → 断言：teammate.status = Failed（不是 crash 路径）
6. 复位，发消息给 teammate
7. mock ACP 不发任何 chunk 60s+
   → 断言：teammate.status = Failed
   → 断言：leader mailbox 有 inactivity notification
8. 并发：100 个 goroutine 同时对同一个 team 调 addAgent
   → 断言：team.agents 长度增加 100（没有丢）
9. 普通场景：发消息给 lead → 一次 Finish → finalize 只触发一次
```

**验收证据**：
1. 上述 smoke 脚本实跑日志（含时间戳）
2. `RUST_LOG=aionui_team=debug` 运行日志显示所有 8 个 phase 的触发点
3. `cargo test --workspace` 全绿

**关键对齐点**：
- **W4-D25 必须最先合并**（D18/D20/D21/D22 都订阅它）
- W4-D18 的 `release_wake_lock` 必须在"消息发出成功"时立即调用，不等 finish（aionui-audit §8 #2）
- W4-D19 的 `clear_finalized_turn` 必须在 re-wake 前调用（aionui-audit §8 #3）
- W4-D24 的 timeout 必须 graceful resolve，不能 reject（aionui-audit §8 #11）

**不通过的典型信号**：
- 任何模块绕过 W4-D25 自己订阅 ACP stream → 驳回（违反 DRY）
- W4-D18 的 wake lock 在 finish 事件之后才释放 → 死锁风险，驳回
- W4-D20 的 leader crash 逻辑走了 remove 路径而不是只 failed → 驳回（aionui-audit §2.1）

**依赖**：M4 通过；M5 不阻塞（Wave 3 与 Wave 4 互不依赖）。

---

## M7 — Wave 5 全部 merge + 全量 e2e

**时点**：M5 和 M6 **均通过**后 6 天（Wave 5 关键路径 D26a→D26b→D26d + D29 链 D29a→D29b→D29c→D29d 两条并行，约 6 日历日）。

**产出物清单**（31 个子模块）：
- ⬜ W5-D26a GuideMcpServer 启停合并 + 2 条测试绿
- ⬜ **W5-D26b-1 `aion_create_team` args 解析 + 默认值（纯函数）合并 + 4 条测试绿**
- ⬜ **W5-D26b-2 `handle_aion_create_team` 调 service + 返回结构化合并 + 3 条测试绿**
- ⬜ W5-D26c `handle_aion_list_models` handler 合并 + 1 条测试绿
- ⬜ W5-D26d 建团后 3 个 WS 事件合并 + 1 条集成测试绿
- ⬜ W5-D27 Guide stdio bridge 分支合并 + 2 条测试绿
- ⬜ W5-D28a `is_team_capable_backend` 纯函数合并 + 3 条测试绿
- ⬜ W5-D28b Guide prompt 注入合并 + 3 条测试绿
- ⬜ W5-D28c `session/new.mcp_servers` 追加 Guide 合并 + 3 条测试绿
- ⬜ **W5-D29a-1 SpawnAgentRequest 类型 + 骨架合并 + 1 条编译测试绿**
- ⬜ **W5-D29a-2 校验 caller = Lead 合并 + 2 条测试绿**
- ⬜ **W5-D29a-3 校验 name 归一化 + 唯一性合并 + 2 条测试绿**
- ⬜ **W5-D29a-4 校验 backend 白名单合并 + 2 条测试绿**
- ⬜ W5-D29b `add_agent` 扩展合并 + 2 条测试绿
- ⬜ **W5-D29c-1 写 extra 合并 + 1 条测试绿**
- ⬜ **W5-D29c-2 kill + get_or_build_task 合并 + 2 条测试绿**
- ⬜ **W5-D29d-1 写欢迎消息合并 + 1 条测试绿**
- ⬜ **W5-D29d-2 wake 新 agent 合并 + 1 条测试绿**
- ⬜ **W5-D29d-3 emit `team.agentSpawned` 事件合并 + 1 条测试绿**
- ⬜ **W5-D30a-1 识别 `shutdown_approved` 字符串合并 + 2 条测试绿**
- ⬜ **W5-D30a-2 approved 处理 remove_agent + 通知 + wake 合并 + 3 条测试绿**
- ⬜ W5-D30b `shutdown_rejected` 拦截合并 + 2 条测试绿
- ⬜ W5-D30c `shutdown_agent` 目标 role 校验合并 + 1 条测试绿
- ⬜ **W5-D30d-1 remove_agent：kill 合并 + 2 条测试绿**
- ⬜ **W5-D30d-2 remove_agent：清 3 种 state 合并 + 1 条测试绿**
- ⬜ **W5-D30d-3 remove_agent：slots 移除 + agentRemoved 事件合并 + 2 条测试绿**
- ⬜ W5-D31a `TeamMcpPhase` + payload 类型合并 + 2 条 serde 测试绿
- ⬜ **W5-D31b-1 mcpStatus tcp 层 2 点广播合并 + 2 条测试绿**
- ⬜ **W5-D31b-2 mcpStatus service 层 6 点广播合并 + 2 条测试绿**
- ⬜ **W5-D31b-3 mcpStatus bridge 层 2 点广播合并 + 1 条集成测试绿**
- ⬜ W5-D31c `teammate_message` emit 合并 + 2 条测试绿
- ⬜ **全量 e2e smoke test** `crates/aionui-app/tests/team_full_e2e_smoke.rs` 通过

**全量 e2e smoke test 脚本**：

```
前置：M6 已合并；开发机装 claude --experimental-acp

场景 A：单聊→建团（MCP 路径）
1. 用 solo claude agent 发送消息 "帮我拉一个团做一个简单的 todo app"
2. agent 基于 Guide prompt 调 aion_list_models → aion_create_team
3. 断言：team 被创建，lead 复用原 conversation
4. 断言：WS 收到 team.listChanged + deepLink.received
5. 断言：lead agent 自动收到 summary 消息并开始 team 工作流

场景 B：真实 spawn（team 内 MCP 路径）
6. lead 调 team_spawn_agent("coder", agent_type="claude")
7. 断言：team.agents 长度 +1
8. 断言：新 agent 被 wake 且收到欢迎消息
9. 断言：新 agent 能调 team_members 看到所有人

场景 C：真 kill + shutdown 协议
10. 用户对 team 说 "dismiss coder"
11. lead 调 team_shutdown_agent("coder")
12. coder 回 team_send_message("shutdown_approved")
13. 断言：coder 的 claude 子进程被 SIGKILL
14. 断言：team.agents 长度 -1
15. 断言：lead mailbox 收到 "coder removed" 确认消息

场景 D：互斥验证
16. 进 team 的 agent 构造新 session 时 → mcp_servers 中 ✕ 没有 aion_* 工具
17. 断言：agent instructions 中 ✕ 没有 Team Guide prompt
```

**验收证据**：
1. 上述 3 + 互斥 4 个场景全部实跑日志（含 WS 事件流 + MCP server log）
2. `sqlite3 aionui.db` 查场景 A 后 conversations 表确认复用：单聊 conversation.extra.team_id 被写入且 conversation 未删除
3. 场景 C 后 `ps aux | grep claude` 确认 coder 进程已消失
4. `cargo test --workspace` 全绿

**关键对齐点**：
- 场景 A 依赖 Wave 3 W3-D15（conversation 复用）正确实现
- 场景 B 依赖 Wave 4 W4-D18（wake 锁）+ W4-D23（add_agent_locks）
- 场景 C 依赖 Wave 4 W4-D18（清 wake 锁）+ W4-D19（清 finalized_turns）
- 场景 D 是硬约束（aionui-audit §8 #17），失败直接影响产品语义

**不通过的典型信号**：
- 场景 A 后单聊 conversation 消失 → 复用逻辑错误，回滚 W3-D15
- 场景 B spawn 后新 agent 永远 Pending 不被 wake → W5-D29 步骤 10 未调 wake 或 W4-D18 lock 错用
- 场景 C shutdown_approved 后 agent 进程仍在 → W5-D30 的 `remove_agent` 未调 kill
- 场景 D 进 team 的 agent 能调 aion_create_team → W5-D28 互斥 guard 失效

**依赖**：M5 **且** M6 通过。

---

## M8 — Phase1 全量交付

**时点**：M7 后 1 天。

**产出物清单**：
- ⬜ Wave 1–5 全部 PR 合并到 main
- ⬜ `cargo build --release` 无 warning
- ⬜ `cargo test --workspace` 全绿
- ⬜ [README.md](./README.md) 状态区所有里程碑 ✅
- ⬜ [interface-contracts.md](./interface-contracts.md) §1–§32 全部 "Shipped"
- ⬜ 未覆盖的 P1/P2 项（若有，如 preset assistant 体系）整理成 phase2 backlog

**验收证据**：
1. 后端独立 e2e 手工跑 Wave 2 smoke + Wave 4 可靠性 smoke + Wave 5 全量 e2e，证据齐全
2. 所有 PR 链接集中列在 phase1 交付 PR 的 body
3. 文档页面所有链接都能点开
4. 两个新开发者（未参与 phase1 的人）按 [README.md §7 开发者入口](./README.md#7-开发者入口) 能在 30 分钟内理清某个模块的上下文

**依赖**：M7 通过。

---

## 1. 模块 ↔ 里程碑交叉表（87 模块）

| 里程碑 | W1（10） | W2（8） | W3（16） | W4（22） | W5（31） |
|:---:|---|---|---|---|---|
| M0 (ack) | D1–D4b + D5a/b/c + D6 all ack | D7a/b/c + D8–D11 + D11.5 ack | — | — | — |
| M1 (merge) | D1–D6 + D4b all ✅ | — | — | — | — |
| M2 (merge) | — | D7a → D7b → D7c → D11.5 串行 + D8/D9/D10/D11 ✅（D11 骨架） | — | — | — |
| M3 (smoke) | — | D11 smoke ✅ | — | — | — |
| M4 (W2 收尾) | post-fix | post-fix | ack（16 人） | ack（22 人） | — |
| M5 (W3 merge) | — | — | W3-D12a..D17b ✅ | — | — |
| M6 (W4 merge) | — | — | — | W4-D18a..D25c-2 ✅ + 可靠性 smoke | — |
| M7 (W5 merge) | — | — | — | — | W5-D26a..D31c ✅（31 子模块）+ 全量 e2e |
| M8 (ship) | post-fix | post-fix | post-fix | post-fix | post-fix |

---

## 2. 风险与降级

| 风险 | 触发条件 | 降级策略 |
|------|---------|---------|
| ACP SDK 的 `McpServer` stdio variant 不符合预期 | D3 在 M0 阶段发现 | 改用 HTTP transport 注入（backend-audit §4.3 备选）；延期 M1 半天 |
| `claude --experimental-acp` 本地机跑不起来 | M3 / M7 阶段 | 手工 WS 连 + curl 校验 DB extra + mock ACP 响应；smoke 改成半手工 |
| Wave N 某模块做到一半发现签名冲突 | 各 Wave 阶段 | 暂停该模块，开 issue 改 interface-contracts.md，leader 裁决；其他模块不冻结 |
| D5 的 AionUi prompt 文本在移植时有 UTF-8/换行问题 | M1 阶段 | 改用 `include_str!("prompt_templates/lead.txt")` 把 AionUi 原文件逐字节拷进来，再 diff |
| `task_manager.kill` + `get_or_build_task` 组合在 team agent 首次启动（DashMap 里本来就没）时行为不确定 | M2 / M7 阶段 | D9 / W5-D29 显式处理：`kill` 返回 `NotFound` 视为成功 |
| Finish 事件订阅导致后台 task 泄漏 | M3 / M6 阶段 | D9 / W4-D18 在 `stop_session` 里 abort 所有订阅 task 的 JoinHandle；smoke test 跑完断言后台 task 已退出 |
| W4-D25 broadcast channel 被 lagged 订阅拖慢 | M6 阶段 | channel size 已取 256；lagged 订阅者 tokio broadcast 自动 skip 不影响其他订阅 |
| W5-D26 Guide MCP 和 W3-D15 conversation 复用语义冲突 | M7 阶段 | W3-D15 冲突校验已在 §16 定义；W5-D26 MCP 路径走相同 service 方法自动兼容 |
| W5-D29 spawn 闭环任一步失败导致半成品 agent | M7 阶段 | phase1 最小实现：log + set_status(Failed) 不回滚 agents 数组；W5-D30 的 removeAgent 可手工清理 |
| Wave 3 与 Wave 4 同时改 `scheduler.rs` 导致大量 merge 冲突 | M5 / M6 阶段 | Wave 3 不动 scheduler.rs（见各 W3-Dn 的"目标文件"，只 W3-D14 改 scheduler.rs 的 rename_agent，与 Wave 4 范围天然隔离） |

---

## 3. Phase1 全量覆盖清单（不再有 "phase2 backlog"）

> 原 phase1 规划（只做 Wave 1/2）里的 "phase2 backlog" 已经**全部并入** Wave 3/4/5。以下是覆盖证明。

| 原 "phase2" 条目 | 新归属 Wave | 模块 |
|------------------|:-----------:|------|
| activeWakes 重入锁 | Wave 4 | W4-D18 |
| wakeTimeouts 60s 看门狗 | Wave 4 | W4-D18 |
| finalizedTurns 5s dedup | Wave 4 | W4-D19 |
| crash recovery | Wave 4 | W4-D20 |
| 429 / rate-limit 识别 | Wave 4 | W4-D21 |
| inactivity watchdog | Wave 4 | W4-D22 |
| addAgentLocks per-team 串行化 | Wave 4 | W4-D23 |
| leader 不可 shutdown 的 target role 检查 | Wave 5 | W5-D30 |
| `team_send_message` 识别 `shutdown_approved/rejected` | Wave 5 | W5-D30 |
| `team_spawn_agent` 真实 spawn | Wave 5 | W5-D29 |
| `team_rename_agent` 规范化 + renamed_agents map | Wave 3 | W3-D14 |
| `teammate_message` WS 事件 | Wave 5 | W5-D31 |
| Team Guide MCP 单例（aion_create_team / aion_list_models） | Wave 5 | W5-D26 / W5-D27 / W5-D28 |
| `mcp_ready` 握手 | Wave 4 | W4-D24 |
| 300s 请求超时 + 64MB 帧 | Wave 3 | W3-D17 |
| `getTeam` 的 `repairTeamAgentsIfMissing` | Wave 3 | W3-D13 |
| user-scope 过滤 | Wave 3 | W3-D12 |
| `ConversationService::send_message` 识别 `extra.team_id` | Wave 3 | W3-D16 |
| conversation 复用（单聊→建团） | Wave 3 (REST) + Wave 5 (MCP) | W3-D15 + W5-D26 |
| `team.mcpStatus` 10-phase WS 事件 | Wave 5 | W5-D31 |

**真正延后到 phase2** 的只剩 P2 项（非 phase1 交付目标）：

- `Team.workspace` / `workspace_mode` / `session_mode` 字段暴露给调用方（P2#38）
- `CreateTeamRequest.agents[].role` 字段尊重（非硬编码首 agent=Lead）（P2#39）
- `updateWorkspace` 级联更新每个 agent conversation 的 extra（P2#40）
- `setSessionMode` 供 spawn 继承（P2#41）
- HTTP 端点拉任务板 / 邮箱历史（P2#43，调用方靠 WS 事件即可，锦上添花）
- `team_describe_assistant` 的 preset assistant 体系 + locale 解析（P2#44，backend 无 preset 配置来源）
- `team_task_list` 输出格式对齐（backend P2#55）

这些 P2 项整体不涉及 team 核心协作行为，可按需求落地；phase1 范围不做。
