# Phase 2 Bugfix Status — 2026-05-02

## Branch: `fix/team-communication-bugs`

## 当前状态：所有后端 bug 已修复，未提交。前端有独立 issue 待处理。

---

## Problem 1: Leader 不 spawn 成员 (卡在 working 状态)

### 状态: ✅ 已修复

### 根因

Leader prompt (`lead.txt`) 规则 9/10 和 87-88 行要求先提方案等确认。Guide agent 发给 leader 的 send_message 是纯 summary，没有行为指示。Leader 按规则理解为需要先提方案。

### 修复

**双保险：**
1. `crates/aionui-team/src/guide/server.rs:156` — send_message 追加 `[SYSTEM NOTE]` 明确指示直接 spawn
2. `crates/aionui-team/src/prompts/prompt_templates/lead.txt` — 规则 9/10/88 增加 SYSTEM NOTE 例外

---

## Problem 2: 前端不自动跳转 (单聊 → team)

### 状态: 后端已修复，前端待配合

### 根因

后端 create_team 路由只返回 HTTP 200，没有 broadcast 任何 WebSocket 事件。

### 修复

- `crates/aionui-team/src/service.rs` — create_team 成功后 broadcast `team.created` 事件
- 同时在 adopt 旧 conversation 时广播 `conversation.listChanged(updated)` 让前端移除旧对话

### 前端待做

前端需监听 `team.created` + `conversation.listChanged` 实现自动跳转。已提 issue: https://github.com/iOfficeAI/AionUi/issues/2734

---

## Problem 3: 成员 MCP 工具调用卡在权限确认

### 状态: ✅ 已修复

### 根因

Team 成员 conversation extra 里没有 `session_mode`。Claude Code 默认 default mode，MCP 调用触发权限确认。

### 修复

在 team 成员创建时强制写入 `"session_mode": "bypassPermissions"` 到 conversation extra：
- `crates/aionui-team/src/service.rs` — `rebuild_agent_processes` 的 patch 加 `session_mode`
- `crates/aionui-team/src/session.rs` — `attach_spawned_agent_process_bg` 的 patch 加 `session_mode`

---

## Problem 4: Leader 无限 working，无法与 leader 对话

### 状态: ✅ 已修复

### 根因

**A. wake_lock 竞态：** `wake_agent_in_session` 的 Finish event 在 `release_wake_lock` 之前 emit，`on_agent_finish` 被跳过。

**B. 缺少 StreamRelay：** `try_wake` 路径没有 StreamRelay，agent 响应不可见。

**C. 缺少 turn.completed：** 前端永远显示"正在处理中"。

### 修复

- `service.rs` — `wake_agent_in_session`: release_wake_lock 后手动 `on_agent_finish` + 加 StreamRelay
- `session.rs` — `try_wake`: 加 StreamRelay（含 turn completion）
- `conversation/service.rs` — 新增 `pub fn repo()` accessor

---

## Problem 5: shutdown_request 成员收不到

### 状态: ✅ 已修复

### 根因

`exec_shutdown_agent` 只把 shutdown_request 写入 mailbox，没有 wake 目标 agent。成员 idle 时不会主动读 mailbox。

### 修复

`crates/aionui-team/src/mcp/server.rs` — `exec_shutdown_agent` 写入 mailbox 后调用 `svc.wake_agent_in_session(team_id, &target_slot_id)` 唤醒目标。

---

## Problem 6: ensure_session 竞态导致 MCP 端口错配

### 状态: ✅ 已修复

### 根因

`create_team` 内部调 `ensure_session`（第一个 session 启动中），前端紧接着调 `POST /session`（第二次 `ensure_session`）。第一次还没插入 sessions map，第二次通过了 `contains_key` 检查，启动了第二个 session。第二个覆盖了第一个，但 agents 的 MCP config 指向第一个 session 的端口（已关闭）→ `Connection refused (os error 61)`。

### 修复

`crates/aionui-team/src/service.rs` — 新增 `ensure_session_locks: DashMap<String, Mutex>` 字段，`ensure_session` 加 per-team mutex + double-check locking。

---

## Problem 7: Guide MCP 调用偶发 connection error

### 状态: ✅ 已修复（加重试）

### 根因

Session resume 时 CLI 重新 spawn 已删除的单聊升级 MCP 子进程，Guide HTTP server 偶尔在极短时间窗口内不可达（TCP 连接接受但 HTTP 请求体读取失败）。

### 修复

`crates/aionui-app/src/guide_stdio.rs` — `forward_tool` 加最多 3 次重试（间隔 500ms/1s/1.5s）。

---

## Problem 8: 成员面板消息重复显示

### 状态: ✅ 已修复

### 根因

`mirror_unread_to_conversation` 同时：(1) 写入 DB message (2) 广播 `team.teammateMessage` WebSocket 事件。前端两个通道都处理了，显示两次。

### 修复

WebSocket 事件 payload 中增加 `msg_id` 字段，供前端去重。前端应检查已渲染的 msg_id 避免重复插入。

---

## Problem 9: 单聊第一条消息重复

### 状态: ⚠️ 前端 bug，后端无需修改

### 现象

新会话第一条消息在前端显示两条相同的 right bubble。

### 排查结论

后端日志确认只收到一次 POST send_message（单个 msg_id）。但 53 秒后前端又发了第二次 POST（不同 msg_id，相同内容），带 `resume` flag。这是前端的重发逻辑 bug。

### 需要前端做

发送按钮点击后立即 disable + 对同一内容做去重。已包含在 issue #2734。

---

## 已修复的问题汇总

| # | Issue | Commit/状态 | Status |
|---|-------|--------|--------|
| 1 | spawn_agent 缺 finish_subscriber | PR #140 | ✅ |
| 2 | finalize_turn 去重窗口丢事件 | PR #140 | ✅ |
| 3 | wake/finish 竞态 | PR #140 | ✅ |
| 4 | 单聊转群聊会话复用 | bcc89b2 | ✅ |
| 5 | guide server user_id | bcc89b2 | ✅ |
| 6 | MCP 工具权限白名单 | bcc89b2 | ✅ |
| 7 | MCP bridge 缺 JSON-RPC id → 死锁 | bcc89b2 | ✅ |
| 8 | Guide HTTP 大 body 读取不完整 | a3b6cb9 | ✅ |
| 9 | spawn_agent warmup 阻塞 MCP 响应 | 9f31504 | ✅ |
| 10 | Leader 不 spawn（prompt 规则） | 本次未提交 | ✅ |
| 11 | 成员 session_mode 缺失卡权限 | 本次未提交 | ✅ |
| 12 | 后端无 team.created WebSocket 事件 | 本次未提交 | ✅ |
| 13 | Leader 无限 working（wake_lock 竞态） | 本次未提交 | ✅ |
| 14 | Leader 输出不可见（缺 StreamRelay） | 本次未提交 | ✅ |
| 15 | shutdown_request 未 wake 目标 | 本次未提交 | ✅ |
| 16 | ensure_session 竞态端口错配 | 本次未提交 | ✅ |
| 17 | Guide MCP 偶发连接失败 | 本次未提交 | ✅ |
| 18 | 成员面板消息重复（mirror + WS 双写） | 本次未提交 | ✅ |
| 19 | conversation.listChanged 缺失 | 本次未提交 | ✅ |
| 20 | leader mirror 跳过（成员给 leader 消息不可见） | 本次未提交 | ✅ |

---

## 未修复/待前端处理

| # | Issue | 状态 |
|---|-------|------|
| 21 | 前端不自动跳转/刷新边栏 | 后端就绪，前端 issue #2734 |
| 22 | 成员消息气泡缺头像+名称 | 后端已提供数据，前端 issue #2734 |
| 23 | 单聊第一条消息重复 | 前端 bug，issue #2734 |

---

## 改动文件清单（均未提交）

```
crates/aionui-team/src/service.rs          — wake_lock 修复、StreamRelay、ensure_session 锁、team.created 事件、conversation.listChanged、session_mode
crates/aionui-team/src/session.rs          — try_wake 加 StreamRelay、移除 leader mirror 跳过、session_mode、msg_id 去重
crates/aionui-team/src/guide/server.rs     — send_message 加 [SYSTEM NOTE]
crates/aionui-team/src/prompts/prompt_templates/lead.txt — SYSTEM NOTE 例外规则
crates/aionui-team/src/mcp/server.rs       — shutdown_agent 加 wake
crates/aionui-team/docs/phase2/bugfix.md   — 本文档
crates/aionui-conversation/src/service.rs  — pub fn repo() accessor
crates/aionui-app/src/guide_stdio.rs       — forward_tool 重试逻辑
```

---

## 重要操作提示

1. build release 后必须执行 `pkill -f aionui-backend` 杀掉旧进程，再重启前端
2. 当前 release binary 已包含所有修复：`/Users/zhuqingyu/project/aionui-backend/target/release/aionui-backend`
3. 代码改动均在工作区未提交，需要 commit

---

## 环境信息

- Backend binary: `/Users/zhuqingyu/project/aionui-backend/target/release/aionui-backend`
- Symlink: `~/.local/bin/aionui-backend` → 上述 binary
- Frontend: `/Users/zhuqingyu/project/AionUi` branch `feat/backend-migration`
- Backend fix branch: `fix/team-communication-bugs`
- Log file: `/Users/zhuqingyu/Library/Logs/AionUi-Dev/2026-05-02.backend.log`
- Frontend issue: https://github.com/iOfficeAI/AionUi/issues/2734
