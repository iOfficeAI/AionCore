# Team 提示词体系（AionUi 参考实现）

> 本文档完整记录 AionUi main 分支中 team 模块的三层提示词设计。
> 源码路径前缀：`/Volumes/Macintosh HD/Users/zhuqingyu/project/AionUi/src/process/team/prompts/`

**相关文档**：
- [MCP 通信](./mcp.md) — Team 内部 MCP 的 `team_*` 工具定义
- [内部调度](./internals.md) — scheduler 状态机、wake 机制（prompt 里的 "Standing By" 与 wake 紧密关联）
- [前端接入指南](./frontend-guide.md) — 前端视角的 team 接入
- [后端 GAP 分析](./backend-current-state-and-gap.md) — 后端缺失的 prompt 能力清单
- [AionUi 完整调研](./aionui-team-complete.md) — team 模块全貌（含 prompt 注入路径）

---

## 1. 两层结构

```
┌────────────────────────────────────────────────────────┐
│  Layer 1: Leader Prompt                                │
│  注入对象：team leader agent                            │
│  目的：协调团队，不亲自干活，通过 MCP 工具调度             │
│  文件：leadPrompt.ts                                    │
└──────────────────────┬─────────────────────────────────┘
                       │ leader spawn 出 teammate
                       ▼
┌────────────────────────────────────────────────────────┐
│  Layer 2: Teammate Prompt                              │
│  注入对象：team 里的每个 teammate                        │
│  目的：接收任务、执行、汇报、等待                         │
│  文件：teammatePrompt.ts                                │
└────────────────────────────────────────────────────────┘
```

辅助文件：
- `buildRolePrompt.ts` — 入口分发，根据 `agent.role` 选 leader 或 teammate prompt
- `toolDescriptions.ts` — `team_spawn_agent` 工具的描述文本

---

## 2. Layer 1: Leader Prompt（团队 leader）

**文件**：`leadPrompt.ts` → `buildLeaderPrompt(params)`

**注入时机**：leader agent 首次激活或 crash recovery 时

### 角色定义

> You coordinate a team of AI agents. You do NOT do implementation work yourself.

### 动态注入的上下文

| 区块 | 条件 | 内容 |
|------|------|------|
| `## Your Teammates` | 始终有 | 每个 teammate 的 name + agentType + status；无人时提示"先提方案再 spawn" |
| `## Available Agent Types for Spawning` | `availableAgentTypes` 非空 | 每种 type 一行（`claude — general-purpose AI assistant`） |
| `## Available Preset Assistants for Spawning` | `availableAssistants` 非空 | preset 的 customAgentId + name + backend + description + skills |
| `## Team Workspace` | `teamWorkspace` 非空 | 共享工作目录路径 |

### Spawn 阵容确认流程

```
Step 1:  收到用户请求
Step 2:  判断是否需要更多 teammate
Step 3:  需要 → 先调 team_list_models 查可用 model
Step 4:  回复文本：一句话理由 + 阵容表（name / responsibility / type / model）
Step 5:  问用户是否同意，同时告知"后续可以随时调整阵容"
Step 6:  **结束 turn，不调 team_spawn_agent**
Step 7:  等用户确认
Step 8:  确认后 → team_spawn_agent
Step 9:  建任务 → team_task_create
Step 10: 分配任务 → team_send_message
```

### Preset Assistant 选择逻辑

```
1. 扫描 preset 的描述和 skills
   └─ 明确匹配 → 直接 team_spawn_agent(assistant_id=preset_id)
2. 两个以上可能匹配
   └─ 调 team_describe_assistant 对比后选最佳
3. 无匹配
   └─ 回退到 Available Agent Types 里选通用 agent
```

### Agent Type 表格格式规则

| 来源 | 格式 |
|------|------|
| Preset Assistant | `显示名 (backend)` 例：`Story Roleplay (gemini)` |
| 通用 CLI Agent | 纯 backend 名 例：`claude` |

### 关键行为规则

- **严禁用平台内置工具**（SendMessage/TaskCreate/Agent），必须用 `team_*` MCP 工具
- **Idle 是正常状态**：teammate idle 不代表出错，发消息即可唤醒
- **依赖串行调度**（CRITICAL）：B 依赖 A 的产出 → 先派 A，A 完成后再派 B。**不要让 B "stand by" 等 A**，否则 B 的 LLM stream 会在 ~300s 后超时 fail
- **Leader 不得擅自下线成员**：只有在**用户明确要求**时才能 shutdown teammate。Leader 不能自作主张淘汰/替换/下线任何成员
- **Shutdown 协议**：用 `team_shutdown_agent` 而非 `team_send_message`
- **Model 选择**：复杂推理用最强 model，常规任务用快/便宜 model；model ID 必须来自 `team_list_models` 返回值

### Shutdown 规则（原文，必须原样复用到后端 prompt）

以下是 AionUi leader prompt 中关于 shutdown 的**原文**，后端实现时应直接复用：

```
## Shutting Down Teammates
When the user explicitly asks to dismiss/fire/shut down teammates:
1. Use **team_shutdown_agent** to send a formal shutdown request
2. Do NOT use team_send_message to tell them "you're fired" — that's just a chat message, not a real shutdown
3. The teammate will confirm (approved) or reject (with reason) — you'll be notified either way
4. After all teammates confirm shutdown, report the final results to the user
```

以及 Important Rules 里的相关条目原文：

```
- When the user says "dismiss", "fire", "shut down", "remove", or "下线/解雇/开除" a teammate → use team_shutdown_agent
```

**注意**：触发条件是 `When the user explicitly asks` — leader 自己判断"这个人不行"不构成 shutdown 理由，必须是用户主动要求。

---

## 3. Layer 2: Teammate Prompt（团队成员）

**文件**：`teammatePrompt.ts` → `buildTeammatePrompt(params)`

### 角色定义

> Name: {agentName}, Role: {roleDescription(agentType)}

`roleDescription` 映射：
- claude → general-purpose AI assistant
- gemini → Google Gemini AI assistant
- codex → code generation specialist
- qwen → Qwen AI assistant
- 其他 → `{type} AI assistant`

### 动态注入

| 区块 | 内容 |
|------|------|
| `## Your Team` | Leader 名 + Teammates 名列表（含 rename 历史） |
| `## Workspaces` | Team workspace（项目文件）+ 个人工作目录（笔记/日志） |

### 工作流程

```
Step 1: 读未读消息，理解任务
Step 2: 有任务且无阻塞 → 立即开干
Step 3: team_task_update → in_progress
Step 4: 实际工作（读文件、写代码、搜索...）
Step 5: team_task_update → completed
Step 6: team_send_message → 向 leader 汇报
```

### Standing By 规则（CRITICAL）

> "Standing by" 意味着**结束 turn**，不是在 LLM stream 里生成空闲文字。

三种 standing by 触发条件：
1. 任务板为空且消息里没有具体任务
2. Leader 要求等前置（如"等 reviewer-1 完成"）
3. 当前任务完成，没有新任务

正确做法：
1. （可选）发一条简短确认给 leader
2. **停止生成，交回控制权**

错误做法（会导致 ~300s 超时 fail）：
- 持续输出 "I am waiting..." / "still standing by..."
- 推理循环
- 重复状态更新

### Shutdown 协议

收到 `shutdown_request` 消息后：
- 同意下线 → `team_send_message("shutdown_approved")`
- 拒绝 → `team_send_message("shutdown_rejected: <reason>")`

---

## 4. MCP Tool Description 原文（后端必须原样复用）

以下是 AionUi 每个 MCP 工具的 description + schema 原文。后端在 `tools/list` 返回的 `ToolDescriptor` 里必须使用这些文本，不要改写。

### 4.1 Team 内部 MCP（10 个工具）

#### team_send_message

**Description**：
```
Send a message to a teammate by name. The message is delivered to their mailbox and they will be woken up to process it.

Use this to:
- Assign work to a teammate
- Share findings or results
- Ask a teammate for help
- Coordinate next steps

The "to" field should be a teammate name (e.g., "researcher", "developer").
Use "*" to broadcast to all teammates.
```

**Schema**：
```
to: string — Recipient teammate name, or "*" for broadcast to all
message: string — The message content to send
summary: string (optional) — A short 5-10 word summary for the UI
```

#### team_spawn_agent

**Description**（来自 `TEAM_SPAWN_AGENT_DESCRIPTION`）：
```
Create a new teammate agent to join the team.

Use this only when one of the following is true:
- The user explicitly approved the proposed teammate lineup in a previous message
- The user explicitly instructed you to create a specific teammate immediately

Before calling this tool in the normal planning flow:
- Start with one short sentence explaining why additional teammates would help
- Tell the user which teammate(s) you recommend
- Present the proposal as a table with: name, responsibility, recommended agent type/backend, and recommended model
- Include each teammate's responsibility, recommended agent type/backend, and model
- Ask whether to create them as proposed or change any names, responsibilities, or agent types
- In that approval question, remind the user that they can later ask you to replace or adjust any teammate if the lineup is not working well
- Do NOT call this tool in that same turn; wait for explicit approval in a later user message

When calling this tool, provide the model parameter if a specific model was recommended and approved.

The new agent will be created and added to the team. You can then assign tasks and send messages to it.
```

**Schema**：
```
name: string — Name for the new teammate (e.g., "researcher", "developer", "tester")
agent_type: string (optional) — Agent type/backend to use for the new teammate. Must be one of the types listed in "Available Agent Types for Spawning". Defaults to the leader type when omitted. Ignored when assistant_id is set.
assistant_id: string (optional) — Preset assistant ID from "Available Preset Assistants for Spawning" (e.g., "builtin-word-creator"). When set, the teammate inherits that preset's rules and skills; agent_type is derived from the preset.
model: string (optional) — Model ID to use for this agent (e.g. "claude-sonnet-4", "gemini-2.5-pro"). Defaults to the backend's preferred model when omitted.
```

#### team_task_create

**Description**：
```
Create a new task on the team's shared task board.

Tasks are visible to all team members and help coordinate work.
Each task has a subject, optional description, and optional owner.

Best practices:
- Create tasks before assigning work
- Set the owner to the teammate who should work on it
- Break large tasks into smaller, actionable items
```

**Schema**：
```
subject: string — Short task title (what needs to be done)
description: string (optional) — Detailed description of the task
owner: string (optional) — Teammate name to assign this task to
```

#### team_task_update

**Description**：
```
Update the status or assignment of an existing task.

Use this to:
- Mark a task as completed or in_progress
- Reassign a task to a different teammate
- Update task status when work is done
```

**Schema**：
```
task_id: string — Task ID (first 8 chars are enough)
status: enum [pending, in_progress, completed, deleted] (optional) — New task status
owner: string (optional) — New owner (teammate name)
```

#### team_task_list

**Description**：
```
List all tasks on the team's task board.

Shows task ID, subject, status, and owner for each task.
Use this to check what work is pending, in progress, or completed.
```

**Schema**：（无参数）

#### team_members

**Description**：
```
List all current team members with their names, types, and status.
Use this to discover available teammates before sending messages or assigning tasks.
```

**Schema**：（无参数）

#### team_rename_agent

**Description**：
```
Rename a teammate. Use this to give a teammate a more descriptive name.
```

**Schema**：
```
agent: string — Current agent name or slot ID
new_name: string — New name for the agent
```

#### team_shutdown_agent

**Description**：
```
Request a teammate to shut down gracefully. The teammate can accept or reject the request.

Use this when:
- The user explicitly asks to dismiss, fire, or shut down a teammate

The teammate will receive a shutdown request and respond with approval or rejection.
You will be notified of the result either way.
```

**Schema**：
```
agent: string — Teammate name to request shutdown
```

#### team_describe_assistant

**Description**：
```
Get detailed information about a preset assistant before spawning it as a teammate.

Returns the preset's full description, enabled skills, and example tasks so you can
judge whether it fits the user's request. Use this when two or more presets look
relevant from the one-line catalog in your system prompt.

Only works on preset assistants listed in "Available Preset Assistants for Spawning".
After confirming a match, call team_spawn_agent with the same assistant_id.
```

**Schema**：
```
assistant_id: string — The preset assistant ID from the "Available Preset Assistants" catalog (e.g., "word-creator").
locale: string (optional) — Locale like "zh-CN" or "en-US". Defaults to the user's current UI language when omitted.
```

#### team_list_models

**Description**：
```
Query available models for team agent types. Returns the real-time model list that matches the frontend model selector.

Use this to:
- Check what models are available before spawning an agent with a specific model
- See all available agent types and their models at once
- Verify a model ID is valid for a given agent type

Pass agent_type to query a specific backend, or omit it to see all.
```

**Schema**：
```
agent_type: string (optional) — Agent type/backend to query (e.g. "gemini", "claude", "codex"). Shows all when omitted.
```

---

## 5. 后端实现现状

**后端（aionui-backend）已有**：
- `crates/aionui-team/src/prompts.rs` — leader + teammate prompt 的基础版本
- `crates/aionui-team/src/mcp/tools.rs` — `team_spawn_agent` 工具描述

**后端缺失**：
- ⚠️ `availableAgentTypes` / `availableAssistants` 动态注入 — 没有 AgentRegistry
- ⚠️ `leaderLabel`（preset assistant 显示名） — 没有 preset 机制
- ⚠️ Preset Assistant 选择逻辑 — 没有 `team_describe_assistant` 工具
- ⚠️ `team_list_models` 工具 — 没有

### 后端已有 prompt vs AionUi prompt 对比

后端的 `prompts.rs` 需要与 AionUi 对齐的点：
1. Leader prompt 的"先出阵容表、等确认、再 spawn"流程是否已包含
2. Teammate prompt 的 "Standing By" 超时防护是否已包含
3. "依赖串行调度"规则是否已包含
4. Model 选择指引是否已包含

（需要读后端 prompts.rs 做逐项比对，本文档先记录 AionUi 侧事实）

---

## 6. 关键设计决策（从 prompt 反推的产品意图）

1. **"先确认再 spawn"不是建议，是硬性要求** — prompt 里用 "STRICT — follow every step, do NOT skip" 强调
2. **Model 选择是 agent 的职责** — 用户确认的是阵容表（含 model 推荐），agent 要先查 `list_models` 再推荐
3. **Preset assistant 优先于通用 agent** — "When a task matches a preset's specialty, prefer spawning the preset over a generic CLI agent"
4. **Standing by = 结束 turn** — 这是防止 LLM stream 超时的关键设计，不是可选的
5. **依赖串行不能并行等待** — "B 等 A" 的正确做法是"A 完成后才派 B"，不是"同时派 A 和 B 让 B stand by"
