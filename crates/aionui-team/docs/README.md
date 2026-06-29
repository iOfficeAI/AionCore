# aionui-team

多 Agent 协作模块：一个团队 = 一个 Lead + N 个 Teammate，共享任务板与邮箱，Lead 派单、Teammate 执行、完成后通知 Lead 汇总。

## 架构

```
                ┌───────────────────────────────────┐
                │          HTTP REST (/api/teams)   │
                └───────────────┬───────────────────┘
                                │
                      ┌─────────▼─────────┐
                      │   TeamSession     │  每 team 一份（内存）
                      │  (session.rs)     │
                      └─┬────────┬────────┘
                        │        │
              ┌─────────▼──┐  ┌──▼──────────────┐
              │ Scheduler  │  │  TeamMcpServer  │ 127.0.0.1:随机端口
              │(scheduler) │  │  (mcp/server)   │ Agent 通过 TCP+JSON-RPC 连接
              └─┬────────┬─┘  └────┬────────────┘
                │        │         │
      ┌─────────▼┐  ┌────▼──────┐  │  调用 8 个 MCP tool
      │ Mailbox  │  │ TaskBoard │  │  (send_message / task_* / ...)
      │(SQLite)  │  │ (SQLite)  │  │
      └──────────┘  └───────────┘  │
                                   │
                          持久化：teams / mailbox / team_tasks 三张表
```

## 模块

- **[HTTP API](./api.md)** — REST 端点、请求/响应字段、错误码
- **[内部调度](./internals.md)** — 状态机、wake→dispatch 时序、已知 bug
- **[MCP 通信](./mcp.md)** — agent ↔ 后端协作协议、工具清单、后端 GAP 分析
- **[前端接入指南](./frontend-guide.md)** — 客户端该做什么、不该做什么

## 前端必读

1. **Team/Agent 所有增删改，都是普通 REST**，客户端不需要任何专属调度/状态机逻辑。
2. **用户→agent 发消息 = 单聊**：走 `POST /api/conversations/{conversation_id}/messages`，`conversation_id` 从 `TeamAgentResponse.conversation_id` 取。team 模块不提供消息端点。
3. **Agent 消息历史也走单聊**：`GET /api/conversations/{conversation_id}/messages`。
4. **实时状态只走 WebSocket**（`team.agentStatusChanged` / `team.agentSpawned` / ...），HTTP 不提供轮询状态接口。
5. **MCP 是后端 ↔ agent 进程之间的事，浏览器不直接接触**（见 [mcp.md](./mcp.md)）；但要知道"单聊 → 自动建团"和 lead 的 `team_spawn_agent` 在后端 ⚠️ 都未实现，建团必须显式调 `POST /api/teams`。
