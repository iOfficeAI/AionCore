# Team Mode Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:using-git-worktrees before executing any round. Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the finalized Team mode architecture from `docs/superpowers/specs/2026-06-10-team-mode-architecture-review.md` as one coordinated change set, split into reviewable implementation rounds.

**Architecture:** Team UI sends Team commands only through Team API. Team domain owns Team semantics through TeamOrchestrator-style service boundaries, while Conversation/runtime capabilities are reused through narrow Team-facing ports/adapters assembled in app composition. Old Team send paths, old event names, and long-term compatibility fallbacks are removed before final merge.

**Tech Stack:** Rust 2024 (`AionCore`), Axum routes, SQLite repository traits, Tokio event loops, React 19/Electron/Vite (`AionUi`), WebSocket events, Vitest/Playwright.

---

## 0. Repositories, Branches, And Worktrees

This change spans two repositories:

- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore`
- AionUi: `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi`

All implementation rounds must run inside `.worktrees`, not in the main checkout. Both repositories already have `.worktrees/` ignored:

- AionCore `.gitignore:559` ignores `.worktrees/`.
- AionUi `.gitignore:197` ignores `.worktrees/`.

Use the same branch name in both repositories:

```bash
feat/team-mode-architecture
```

Use the same worktree directory name in both repositories:

```bash
.worktrees/team-mode-architecture
```

Start each execution session with these checks.

For AionCore:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionCore
git rev-parse --show-toplevel
git check-ignore -v .worktrees
git status --short
```

If the worktree does not exist yet:

```bash
git worktree add .worktrees/team-mode-architecture -b feat/team-mode-architecture
```

If the branch already exists and the worktree does not:

```bash
git worktree add .worktrees/team-mode-architecture feat/team-mode-architecture
```

Then use:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture
```

For AionUi:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionUi
git rev-parse --show-toplevel
git check-ignore -v .worktrees
git status --short
```

If the worktree does not exist yet:

```bash
git worktree add .worktrees/team-mode-architecture -b feat/team-mode-architecture
```

If the branch already exists and the worktree does not:

```bash
git worktree add .worktrees/team-mode-architecture feat/team-mode-architecture
```

Then use:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture
```

Physical PR shape:

- If the code host requires one PR per repository, create one AionCore PR and one AionUi PR with the same branch name and link them.
- Treat the pair as one architecture change set. Do not merge one without the other unless the final verification proves it is independently safe.

Do not run `cargo test --workspace` at the start of a round. Follow AionCore `AGENTS.md`: use affected-crate tests during development, then broader verification near the end.

---

## 1. Source Documents And Round Contract

Every round must read these before editing:

- `docs/superpowers/specs/2026-06-10-team-mode-architecture-review.md`
- This plan: `docs/superpowers/plans/2026-06-10-team-mode-architecture-implementation.md`
- Latest `Round Handoff Log` entries at the bottom of this file
- AionCore `AGENTS.md`

Each round must end with:

- Code compiles for affected Rust crates.
- Affected tests pass, or failures are recorded with exact command and failure.
- AionUi lint/test commands relevant to touched files are run, or skipped with a concrete reason.
- Old paths targeted by that round are deleted, not left as long-term fallback.
- The `Round Handoff Log` is updated in this file.
- A commit is created in the round worktree unless the round is intentionally stopped for review before commit.

Each round commit should be small enough to review:

```bash
git add <changed files>
git commit -m "feat(team): <round-specific summary>"
```

---

## 2. Target File Map

### AionCore

Team API and domain:

- `crates/aionui-team/src/routes.rs`: HTTP boundary, `CurrentUser`, request/response mapping, ApiError mapping.
- `crates/aionui-team/src/service.rs`: current TeamSessionService; migration target for TeamOrchestrator-style boundary, ownership checks, session operations.
- `crates/aionui-team/src/session.rs`: current TeamSession; migration target for mailbox/scheduler/session lifecycle and removal of inline wake fallback.
- `crates/aionui-team/src/event_loop.rs`: current per-agent event loop; migration target for `AgentTurnExecutionPort`.
- `crates/aionui-team/src/mailbox.rs`: mailbox wrapper.
- `crates/aionui-team/src/task_board.rs`: task board wrapper.
- `crates/aionui-team/src/scheduler/*.rs`: scheduler state, actions, wake, lifecycle.
- `crates/aionui-team/src/events.rs`: Team event names/payload helpers.
- `crates/aionui-team/src/error.rs`: crate-owned errors.
- `crates/aionui-team/src/types.rs`: Team aggregate and message types.
- `crates/aionui-team/src/lib.rs`: module exports.

New Team modules to create when their round starts:

- `crates/aionui-team/src/ports.rs`: Team-facing traits including `AgentTurnExecutionPort`.
- `crates/aionui-team/src/message_projection.rs`: `TeamMessageProjection` and visibility projection.
- `crates/aionui-team/src/provisioning.rs`: `TeamAgentProvisioner`.
- `crates/aionui-team/src/visibility.rs`: visibility policy types and helpers.

Conversation/runtime:

- `crates/aionui-conversation/src/service.rs`: public/internal service capabilities used by app adapters; ordinary send guard for team-owned conversations.
- `crates/aionui-conversation/src/routes.rs`: ordinary `/api/conversations/:id/messages` remains route-only.
- `crates/aionui-conversation/src/turn_orchestrator.rs`: currently `pub(crate)` user-turn orchestrator; migration target for reusable conversation-backed turn execution API.
- `crates/aionui-conversation/src/session_context.rs`: typed Team binding/runtime seed parsing.
- `crates/aionui-conversation/src/state.rs`: router state shape if service instance sharing changes.
- `crates/aionui-conversation/src/lib.rs`: exports for app composition only when needed.

API and data:

- `crates/aionui-api-types/src/team.rs`: Team request/response/event payloads and event docs.
- `crates/aionui-api-types/src/conversation.rs`: send error contract if ordinary Team conversation send is rejected with a typed response.
- `crates/aionui-db/src/repository/team.rs`: Team repository trait; user-scoped lookup/list helpers.
- `crates/aionui-db/src/repository/sqlite_team.rs`: SQLite implementations.
- `crates/aionui-db/tests/team_repository.rs`: repository coverage.

Composition:

- `crates/aionui-app/src/router/state.rs`: construct shared ConversationService, Team adapters, Team state.
- `crates/aionui-app/tests/team_e2e.rs`: cross-domain Team behavior.
- `crates/aionui-app/tests/team_phase1_smoke.rs`: router wiring smoke tests.
- `crates/aionui-app/tests/conversation_e2e.rs`: ordinary send guard coverage.

Existing Team tests to update or extend:

- `crates/aionui-team/tests/session_service_integration.rs`
- `crates/aionui-team/tests/e2e_team_flow.rs`
- `crates/aionui-team/tests/mailbox_integration.rs`
- `crates/aionui-team/tests/scheduler_integration.rs`
- `crates/aionui-team/tests/task_board_integration.rs`
- `crates/aionui-team/tests/prompts_events_integration.rs`

### AionUi

Team adapter and types:

- `packages/desktop/src/common/adapter/ipcBridge.ts`: add Team send APIs; switch event names.
- `packages/desktop/src/common/types/team/teamTypes.ts`: new Team event payload types.
- `packages/desktop/src/common/adapter/teamMapper.ts`: payload mapping when backend payloads change.

Team page:

- `packages/desktop/src/renderer/pages/team/TeamPage.tsx`: pass Team send context to each agent chat slot.
- `packages/desktop/src/renderer/pages/team/components/TeamChatView.tsx`: render Team-specific send behavior instead of ordinary conversation send.
- `packages/desktop/src/renderer/pages/team/hooks/useTeamSession.ts`: consume new event names.
- `packages/desktop/src/renderer/pages/team/hooks/TeamPermissionContext.tsx`: ensure session readiness remains Team-owned.

Conversation chat/send components reused by Team:

- `packages/desktop/src/renderer/pages/conversation/platforms/acp/AcpChat.tsx`
- `packages/desktop/src/renderer/pages/conversation/platforms/acp/AcpSendBox.tsx`
- `packages/desktop/src/renderer/pages/conversation/platforms/acp/useAcpMessage.ts`
- `packages/desktop/src/renderer/pages/conversation/platforms/aionrs/AionrsChat.tsx`
- `packages/desktop/src/renderer/pages/conversation/platforms/aionrs/AionrsSendBox.tsx`
- `packages/desktop/src/renderer/pages/conversation/platforms/aionrs/useAionrsMessage.ts`
- `packages/desktop/src/renderer/pages/conversation/Messages/hooks.ts`

E2E:

- `tests/e2e/cases/teams/team-communication.e2e.ts`
- `tests/e2e/cases/teams/team-agent-lifecycle.e2e.ts`
- `tests/e2e/cases/teams/team-create.e2e.ts`

---

## 3. Logging And Observability Contract

Add or preserve structured logs for hard-to-observe Team critical paths. Do not log prompt text, user input text, tool input/output, file contents, provider requests/responses, tokens, or secrets.

Use these fields consistently where available:

```text
team_id
slot_id
conversation_id
turn_id
session_id
mailbox_id
task_id
event_name
outcome
error_code
```

Recommended levels:

- `info`: team session started/stopped, agent provisioned/adopted/restored, team turn started/completed, Team MCP server started/stopped, ordinary send rejected for team-owned conversation.
- `debug`: mailbox drain, scheduler decision, target agent notify, visibility projection decision, event loop idle/resume.
- `warn`: unknown slot, duplicated or already-processed mailbox item, missing Team binding in conversation extra, safely handled old/malformed request.
- `error`: turn execution failed, projection write failed when required, provisioning failed, event broadcast failed when the operation depends on it.

Each round must include a log review line in its handoff:

```text
Logs: record one of these outcomes: added; unchanged because existing logs cover the affected path; intentionally unnecessary with a concrete reason.
```

---

## 4. Round 1: Team API Security And UI Send Path

**Goal:** Make Team UI use Team API as the only Team user-send path, and block ordinary conversation send for team-owned conversations.

**Files:**

- Modify AionCore: `crates/aionui-team/src/routes.rs`
- Modify AionCore: `crates/aionui-team/src/service.rs`
- Modify AionCore: `crates/aionui-team/src/error.rs`
- Modify AionCore: `crates/aionui-db/src/repository/team.rs`
- Modify AionCore: `crates/aionui-db/src/repository/sqlite_team.rs`
- Modify AionCore: `crates/aionui-conversation/src/service.rs`
- Modify AionCore: `crates/aionui-api-types/src/team.rs`
- Modify AionCore tests: `crates/aionui-team/tests/session_service_integration.rs`
- Modify AionCore tests: `crates/aionui-app/tests/team_e2e.rs`
- Modify AionCore tests: `crates/aionui-app/tests/conversation_e2e.rs`
- Modify AionUi: `packages/desktop/src/common/adapter/ipcBridge.ts`
- Modify AionUi: `packages/desktop/src/common/types/team/teamTypes.ts`
- Modify AionUi: `packages/desktop/src/renderer/pages/team/TeamPage.tsx`
- Modify AionUi: `packages/desktop/src/renderer/pages/team/components/TeamChatView.tsx`
- Modify AionUi send components only as needed to inject a Team send override.

**Steps:**

- [ ] Add `CurrentUser` extraction to Team route handlers that currently lack it: `list_teams`, `get_team`, `rename_team`, `send_message`, `send_message_to_agent`, `ensure_session`, `stop_session`, `set_session_mode`.
- [ ] Change matching service methods to accept `user_id: &str`.
- [ ] Add Team ownership check helper in `TeamSessionService`, for example `load_owned_team(user_id, team_id) -> Result<Team, TeamError>`.
- [ ] Add `TeamError::Forbidden(String)` and map it to `ApiError::Forbidden` in `routes.rs`.
- [ ] Make `list_teams` user-scoped. Prefer adding `ITeamRepository::list_teams_by_user(user_id)` and `SqliteTeamRepository::list_teams_by_user`; if SQL already has equivalent filtering helpers, use those instead.
- [ ] Ensure `get_team`, `rename_team`, `send_message`, `send_message_to_agent`, `ensure_session`, `stop_session`, and `set_session_mode` reject cross-user access before reading or mutating session state.
- [ ] In `ConversationService::send_message`, detect team-owned conversations through typed or current `extra.teamId` state and reject ordinary send with `ConversationError::Forbidden { reason: "Team-owned conversations must be sent through Team API".into() }`.
- [ ] Preserve ordinary reads for team-owned conversation history.
- [ ] Add `ipcBridge.team.sendMessage` mapping to `POST /api/teams/:team_id/messages`.
- [ ] Add `ipcBridge.team.sendMessageToAgent` mapping to `POST /api/teams/:team_id/agents/:slot_id/messages`.
- [ ] Update Team page chat send behavior so Team leader sends call `team.sendMessage` and member sends call `team.sendMessageToAgent`.
- [ ] Keep Team message rendering on existing conversation message stream; do not insert a second optimistic bubble in the Team UI unless it is deduped by backend `msg_id`.
- [ ] Delete any Team UI path that submits user input through `ipcBridge.conversation.sendMessage`.

**Verification:**

AionCore:

```bash
cargo fmt --all -- --check
cargo test -p aionui-db team_repository
cargo test -p aionui-team session_service_integration
cargo test -p aionui-app team_e2e
cargo test -p aionui-app conversation_e2e
cargo clippy -p aionui-team -p aionui-conversation -p aionui-app -- -D warnings
```

AionUi:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture
bun run lint
bun run test -- packages/desktop/src/renderer/pages/team
```

If the targeted Vitest path has no tests, run:

```bash
bun run test -- --run
```

**Round acceptance:**

- Team user send from Team UI does not call ordinary conversation send.
- Ordinary `/api/conversations/:id/messages` returns forbidden for team-owned conversations.
- Cross-user Team list/get/mutate/send/session operations are rejected.
- Reads of team-owned conversation messages still work.
- Logs include low-sensitive rejection context for ordinary send attempts on team-owned conversations.

---

## 5. Round 2: TeamOrchestrator Boundary, Visibility Policy, And Message Projection

**Goal:** Centralize Team semantics and message projection so mailbox, visible bubbles, hidden messages, teammate mirror, and dedupe rules stop living in ad hoc service/session code.

**Files:**

- Create AionCore: `crates/aionui-team/src/visibility.rs`
- Create AionCore: `crates/aionui-team/src/message_projection.rs`
- Modify AionCore: `crates/aionui-team/src/lib.rs`
- Modify AionCore: `crates/aionui-team/src/service.rs`
- Modify AionCore: `crates/aionui-team/src/session.rs`
- Modify AionCore: `crates/aionui-team/src/events.rs`
- Modify AionCore tests: `crates/aionui-team/tests/mailbox_integration.rs`
- Modify AionCore tests: `crates/aionui-team/tests/e2e_team_flow.rs`
- Modify AionCore tests: `crates/aionui-team/tests/prompts_events_integration.rs`

**Steps:**

- [ ] Define `TeamVisibilityPolicy` with explicit decisions for: write mailbox, insert user visible bubble, insert teammate visible bubble, allow hidden conversation message, strip system notes.
- [ ] Define `TeamProjectionRequest` carrying `team_id`, `slot_id`, `conversation_id`, `source`, `content`, `files`, `visibility`, and optional dedupe key.
- [ ] Implement `TeamMessageProjection` around the conversation message insertion capability currently reached through `ConversationService::insert_raw_message`.
- [ ] Move user right-bubble insert logic out of `TeamSession::send_message` and `TeamSession::send_message_to_agent` into `TeamMessageProjection`.
- [ ] Move teammate left-bubble mirror logic out of `TeamSession::mirror_unread_to_conversation` into `TeamMessageProjection`.
- [ ] Keep mailbox writes in Team domain; do not move mailbox writes into projection.
- [ ] Add dedupe key strategy for teammate mirror events using mailbox message id plus target conversation id.
- [ ] Emit `team.teammateMessage` from projection or Team orchestrator with the same persisted `msg_id` used for the visible bubble.
- [ ] Remove direct raw `MessageRow` construction from session methods once projection covers it.

**Verification:**

```bash
cargo fmt --all -- --check
cargo test -p aionui-team mailbox_integration
cargo test -p aionui-team e2e_team_flow
cargo test -p aionui-team prompts_events_integration
cargo clippy -p aionui-team -- -D warnings
```

**Round acceptance:**

- User visible bubble creation has one code path.
- Teammate visible bubble creation has one code path.
- Team internal state is not stored as hidden messages.
- Hidden messages remain available only for explicit runtime/message semantics.
- No Team session method directly constructs conversation message rows for Team user/teammate bubbles.
- Projection logs contain `team_id`, `slot_id`, `conversation_id`, `event_name`, and no sensitive content.

---

## 6. Round 3: TeamAgentProvisioner And Typed Session Context

**Goal:** Make create_team initial agents, dynamic spawn agents, and restore/rebuild use one provisioning path and typed Team runtime context.

**Files:**

- Create AionCore: `crates/aionui-team/src/provisioning.rs`
- Modify AionCore: `crates/aionui-team/src/service.rs`
- Modify AionCore: `crates/aionui-team/src/session.rs`
- Modify AionCore: `crates/aionui-team/src/service/spawn_support.rs`
- Modify AionCore: `crates/aionui-conversation/src/session_context.rs`
- Modify AionCore: `crates/aionui-ai-agent/src/session_context.rs`
- Modify AionCore: `crates/aionui-ai-agent/src/types.rs`
- Modify AionCore: `crates/aionui-api-types/src/team.rs`
- Modify AionCore tests: `crates/aionui-team/tests/session_service_integration.rs`
- Modify AionCore tests: `crates/aionui-conversation/src/service_test.rs` or add focused inline tests near `session_context.rs`
- Modify AionCore tests: `crates/aionui-ai-agent/tests/acp_agent_integration.rs` if typed context reaches ACP build behavior.

**Steps:**

- [ ] Define typed Team context structs: `TeamSessionBinding`, `TeamRuntimeSeed`, `TeamMcpRuntimeConfig`.
- [ ] Extend `AgentSessionContext` with `team: Option<TeamSessionBinding>` or an equivalent typed field.
- [ ] Update `SessionContextBuilder` to parse existing Team keys: `teamId`, `slot_id`, `role`, `backend`, `session_mode`, `current_model_id`, `team_mcp_stdio_config`.
- [ ] Make `belongs_to_team` derive from typed Team context rather than repeated raw `extra.teamId` parsing.
- [ ] Define `TeamAgentProvisioner` responsible for create/adopt conversation, typed Team binding/runtime seed write, `teams.agents` persistence, Team MCP runtime config update, warmup/rebuild, event loop registration.
- [ ] Move initial agent creation logic from `TeamSessionService::create_team` into `TeamAgentProvisioner`.
- [ ] Move `add_agent` creation logic into `TeamAgentProvisioner`.
- [ ] Move `spawn_agent` persistence and process attach logic into `TeamAgentProvisioner`.
- [ ] Keep scheduler decisions out of `TeamAgentProvisioner`.
- [ ] Delete duplicated raw JSON patches for Team runtime config after provisioner owns them.

**Verification:**

```bash
cargo fmt --all -- --check
cargo test -p aionui-conversation session_context
cargo test -p aionui-ai-agent session
cargo test -p aionui-team session_service_integration
cargo clippy -p aionui-conversation -p aionui-ai-agent -p aionui-team -- -D warnings
```

**Round acceptance:**

- create_team and add_agent use the same provisioning code path.
- MCP spawn uses the same provisioning code path after role/capability validation.
- `SessionContextBuilder` is the authoritative parser for Team conversation extra.
- ai-agent code consumes typed session context and does not parse raw Team JSON keys directly.
- Logs show provisioning lifecycle without logging prompt/input content.

---

## 7. Round 4: AgentTurnExecutionPort And Conversation Turn Adapter

**Goal:** Remove Team event loop direct dependency on Conversation runtime internals by routing agent turn execution through a narrow Team-defined port implemented in app composition.

**Files:**

- Create AionCore: `crates/aionui-team/src/ports.rs`
- Modify AionCore: `crates/aionui-team/src/event_loop.rs`
- Modify AionCore: `crates/aionui-team/src/session.rs`
- Modify AionCore: `crates/aionui-team/src/service.rs`
- Modify AionCore: `crates/aionui-team/src/lib.rs`
- Modify AionCore: `crates/aionui-conversation/src/turn_orchestrator.rs`
- Modify AionCore: `crates/aionui-conversation/src/service.rs`
- Modify AionCore: `crates/aionui-conversation/src/lib.rs`
- Modify AionCore: `crates/aionui-app/src/router/state.rs`
- Modify AionCore tests: `crates/aionui-team/tests/scheduler_integration.rs`
- Modify AionCore tests: `crates/aionui-team/tests/e2e_team_flow.rs`
- Modify AionCore tests: `crates/aionui-conversation/tests/stream_relay_tool_call.rs`

**Steps:**

- [ ] Define `AgentTurnExecutionPort` in `aionui-team::ports`.
- [ ] Keep the trait narrow: `run_agent_turn(request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError>`.
- [ ] Put Team metadata into `AgentTurnRequest`: `team_id`, `slot_id`, `conversation_id`, `user_id`, `content`, `files`, and source metadata.
- [ ] Put only execution result data into `AgentTurnOutcome`: `conversation_id`, `turn_id`, `status`, terminal/runtime summary needed by Team finalize logic.
- [ ] Do not add create/adopt conversation, visible bubble insert, mailbox writes, task board updates, scheduler updates, or cascade wake methods to this port.
- [ ] Expose a conversation-side API that can run a conversation-backed non-ordinary user turn without Team importing `StreamRelay`, `SendMessageData`, `TurnClaim`, or `IAgentTask::send_message`.
- [ ] Implement the app composition adapter in `crates/aionui-app/src/router/state.rs` or a focused app module if the file becomes too large.
- [ ] Inject `Arc<dyn AgentTurnExecutionPort>` into Team state/service/session/event loop.
- [ ] Replace Team event loop direct calls to `conversation_service.warmup`, `runtime_state().try_claim_turn`, `StreamRelay::new`, `handle.send_message`, and manual claim release with `run_agent_turn`.
- [ ] Keep mark-read/finalize/cascade wake in Team event loop or Team service after `AgentTurnOutcome`.

**Verification:**

```bash
cargo fmt --all -- --check
cargo test -p aionui-conversation stream_relay_tool_call
cargo test -p aionui-team scheduler_integration
cargo test -p aionui-team e2e_team_flow
cargo test -p aionui-app team_e2e
cargo clippy -p aionui-team -p aionui-conversation -p aionui-app -- -D warnings
rg -n "StreamRelay|SendMessageData|try_claim_turn|IAgentTask|send_message\\(" crates/aionui-team/src
```

The final `rg` command must not find Team event loop/runtime execution imports or direct runtime execution calls. It may still find Team API methods named `send_message` that write mailbox and call Team orchestration.

**Round acceptance:**

- `aionui-team` does not import `aionui_conversation::stream_relay::StreamRelay`.
- `aionui-team` does not import `aionui_ai_agent::types::SendMessageData`.
- Team event loop calls `AgentTurnExecutionPort`.
- Conversation runtime turn lifecycle remains in Conversation domain/app adapter.
- Team finalize, mark read, and cascade wake remain Team-owned.
- Logs show `team_id`, `slot_id`, `conversation_id`, `turn_id`, and outcome around each port call.

---

## 8. Round 5: Shared Conversation Service Assembly And Event Names

**Goal:** Make app composition own shared service/adapters and switch Team WebSocket events to the final two-level camelCase contract.

**Files:**

- Modify AionCore: `crates/aionui-app/src/router/state.rs`
- Modify AionCore: `crates/aionui-team/src/events.rs`
- Modify AionCore: `crates/aionui-team/src/service.rs`
- Modify AionCore: `crates/aionui-team/src/session.rs`
- Modify AionCore: `crates/aionui-team/src/scheduler/*.rs`
- Modify AionCore: `crates/aionui-api-types/src/team.rs`
- Modify AionCore tests: `crates/aionui-team/tests/prompts_events_integration.rs`
- Modify AionCore tests: `crates/aionui-app/tests/team_e2e.rs`
- Modify AionUi: `packages/desktop/src/common/adapter/ipcBridge.ts`
- Modify AionUi: `packages/desktop/src/common/types/team/teamTypes.ts`
- Modify AionUi: `packages/desktop/src/renderer/pages/team/hooks/useTeamSession.ts`
- Modify AionUi: `packages/desktop/src/renderer/pages/team/hooks/useTeamList.ts`
- Modify AionUi: `packages/desktop/src/renderer/pages/team/hooks/useTeamCreatedRedirect.ts`
- Modify AionUi: `packages/desktop/src/renderer/pages/team/hooks/useSiderTeamBadges.ts`

**Steps:**

- [ ] Update `build_conversation_state` and `build_team_state` so Team uses app-assembled adapters and does not construct its own unrelated `ConversationService` instance for runtime-facing operations.
- [ ] Keep `AppServices` as the construction center; do not construct concrete dependencies inside domain crates.
- [ ] Rename backend events to final names: `team.listChanged`, `team.created`, `team.removed`, `team.renamed`, `team.agentStatusChanged`, `team.agentSpawned`, `team.agentRemoved`, `team.agentRenamed`, `team.teammateMessage`, `team.mcpStatus`, `team.taskChanged`, `team.sessionChanged`.
- [ ] Delete old backend events: `team.agent.status`, `team.agent.spawned`, `team.agent.removed`, `team.agent.renamed`, `team.teammate.message`, `team.list-changed`, `team.mcp.status`.
- [ ] Update AionUi event listeners to final names.
- [ ] Add AionUi listener surfaces for `team.mcpStatus`, `team.taskChanged`, and `team.sessionChanged` even if only stored/ignored initially for future UI.
- [ ] Remove old AionUi event listeners and old type comments that document legacy names.

**Verification:**

AionCore:

```bash
cargo fmt --all -- --check
cargo test -p aionui-team prompts_events_integration
cargo test -p aionui-app team_e2e
cargo clippy -p aionui-team -p aionui-app -- -D warnings
rg -n "team\\.agent\\.|team\\.teammate\\.message|team\\.list-changed|team\\.mcp\\.status" crates
```

AionUi:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture
bun run lint
bun run test -- packages/desktop/src/renderer/pages/team
rg -n "team\\.agent\\.|team\\.teammate\\.message|team\\.list-changed|team\\.mcp\\.status" packages/desktop/src
```

The final `rg` commands must return no production event names. Test fixtures may reference old names only if they explicitly assert migration removal; prefer deleting those fixtures.

**Round acceptance:**

- New event names are used end to end.
- Old event names are deleted, not dual-emitted or dual-listened.
- App composition remains the only construction center for concrete dependencies.
- Logs use `event_name` with the new event names.

---

## 9. Round 6: Cleanup, Regression Tests, And Final Architecture Verification

**Goal:** Remove migration leftovers and prove the final architecture matches the spec.

**Files:**

- Modify any touched AionCore files only to remove old paths or tighten tests.
- Modify any touched AionUi files only to remove old paths or tighten tests.
- Update this plan's `Round Handoff Log`.
- Optionally add a short implementation summary under `docs/superpowers/specs/2026-06-10-team-mode-architecture-review.md` only if the final code intentionally differs from the spec.

**Steps:**

- [ ] Search and delete long-term fallback paths introduced during rounds.
- [ ] Search for Team UI ordinary conversation send usage and delete any remaining call path.
- [ ] Search for Team direct runtime internals and delete any remaining call path.
- [ ] Search for raw Team extra parsing outside `SessionContextBuilder` and delete or route through typed context.
- [ ] Search for old event names in AionCore and AionUi and delete them.
- [ ] Confirm every new Team endpoint and modified Team endpoint has ownership/security coverage.
- [ ] Confirm Team logs contain enough lifecycle information to observe send, projection, provisioning, turn execution, session, and event failure.
- [ ] Run final affected-crate and frontend verification.

**Final architecture search commands:**

AionCore:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture
rg -n "StreamRelay|SendMessageData|try_claim_turn|IAgentTask|runtime_state\\(\\).*claim" crates/aionui-team/src
rg -n "team\\.agent\\.|team\\.teammate\\.message|team\\.list-changed|team\\.mcp\\.status" crates
rg -n "teamId|team_mcp_stdio_config|slot_id|current_model_id" crates/aionui-ai-agent/src crates/aionui-team/src crates/aionui-conversation/src
rg -n "conversation\\.sendMessage|sendMessage\\.invoke" /Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture/packages/desktop/src/renderer/pages/team
```

Expected:

- No Team source imports or directly uses Conversation runtime internals.
- No old event names remain in production code.
- Raw Team extra parsing is concentrated in `SessionContextBuilder` and Team provisioning/write-side code.
- Team page does not submit user Team input through ordinary conversation send.

AionCore final commands:

```bash
cargo fmt --all -- --check
cargo clippy -p aionui-team -p aionui-conversation -p aionui-ai-agent -p aionui-app -- -D warnings
cargo test -p aionui-db team_repository
cargo test -p aionui-conversation
cargo test -p aionui-ai-agent
cargo test -p aionui-team
cargo test -p aionui-app team
```

Only after affected-crate verification is clean, run full workspace verification if time allows:

```bash
cargo test --workspace
```

AionUi final commands:

```bash
cd /Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture
bun run lint
bun run test -- --run
bun run test:e2e:team
```

**Round acceptance:**

- Final code matches section 11 of the architecture spec.
- Old send paths, old event names, and temporary compatibility logic are removed.
- Handoff log contains one entry per round with exact verification results.
- The PR description can be written from this plan and handoff log without relying on chat history.

---

## 10. Per-Round Handoff Format

At the end of every round, append an entry under `Round Handoff Log` using this exact structure:

```md
### Round N Handoff - short descriptive title

Status: completed | blocked | partial
Worktrees:
- AionCore: absolute path
- AionUi: absolute path or not touched
Changed:
- file or module: what changed
Deleted:
- old path, old event, or fallback removed
Tests:
- `command`: pass | fail | not run, concrete reason
Logs:
- added | unchanged | intentionally unnecessary, concrete reason
Known follow-up for next round:
- specific next action
Commit:
- commit sha or not committed
```

If a round is blocked, include the exact blocker and the safest next step. Do not continue to the next round until the blocker is resolved or explicitly accepted.

---

## 11. Round Handoff Log

### Round 0 Handoff - Planning

Status: completed
Worktrees:
- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore` for planning only; implementation must use `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture`
- AionUi: `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi` inspected for planning only; implementation must use `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture`
Changed:
- `docs/superpowers/plans/2026-06-10-team-mode-architecture-implementation.md`: created round-based implementation plan.
Deleted:
- None.
Tests:
- `git check-ignore -v .worktrees` in AionCore: pass, `.gitignore:559` ignores `.worktrees/`.
- `git check-ignore -v .worktrees` in AionUi: pass, `.gitignore:197` ignores `.worktrees/`.
Logs:
- unchanged; planning-only change.
Known follow-up for next round:
- Start Round 1 in both `.worktrees`, implement Team API security and UI send path.
Commit:
- not committed

### Round 1 Handoff - Team API Security And UI Send Path

Status: completed
Worktrees:
- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture`
- AionUi: `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture`
Changed:
- AionCore `crates/aionui-db/src/repository/team.rs`, `crates/aionui-db/src/repository/sqlite_team.rs`: added user-scoped Team listing via `list_teams_by_user`.
- AionCore `crates/aionui-team/src/routes.rs`, `crates/aionui-team/src/service.rs`, `crates/aionui-team/src/error.rs`: added `CurrentUser` extraction and ownership checks for Team list/get/mutate/send/session APIs; added `TeamError::Forbidden`.
- AionCore `crates/aionui-conversation/src/service.rs`: rejects ordinary sends to `extra.teamId` team-owned conversations while preserving reads.
- AionCore tests in `crates/aionui-db/tests/team_repository.rs`, `crates/aionui-team/tests/session_service_integration.rs`, `crates/aionui-app/tests/team_e2e.rs`, `crates/aionui-app/tests/conversation_e2e.rs`: added owner filtering, cross-user rejection, Team-owned ordinary-send rejection, and read preservation coverage.
- AionUi `packages/desktop/src/common/adapter/ipcBridge.ts`, `packages/desktop/src/common/types/team/teamTypes.ts`: added Team send adapters and corrected `session-mode` request body to `mode`.
- AionUi Team chat path and send boxes: Team leader/member sends now route through Team APIs via injected override; reusable ordinary conversation send remains only for non-Team contexts.
Deleted:
- Team UI ordinary user-send path through `ipcBridge.conversation.sendMessage` / `ipcBridge.acpConversation.sendMessage` under `packages/desktop/src/renderer/pages/team`; no long-term fallback kept.
Tests:
- `cargo fmt --all -- --check`: pass.
- `cargo test -p aionui-db team_repository`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-db --test team_repository`: pass, 35 passed.
- `cargo test -p aionui-team session_service_integration`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team --test session_service_integration`: pass, 48 passed.
- `cargo test -p aionui-app team_e2e`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-app --test team_e2e`: pass, 41 passed.
- `cargo test -p aionui-app conversation_e2e`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-app --test conversation_e2e`: pass, 40 passed.
- `cargo clippy -p aionui-team -p aionui-conversation -p aionui-app -- -D warnings`: pass.
- `bun run lint`: pass, 730 existing warnings and 0 errors.
- `bun run test -- packages/desktop/src/renderer/pages/team`: fail as expected for the plan fallback, no matching Vitest files found.
- `bun run test -- --run`: pass, 151 test files passed, 1 skipped; 1131 tests passed, 3 skipped.
- `rg -n "ipcBridge\\.(conversation|acpConversation)\\.sendMessage|conversation\\.sendMessage\\.invoke|acpConversation\\.sendMessage\\.invoke" packages/desktop/src/renderer/pages/team`: pass, no matches.
Logs:
- added low-sensitive `info` rejection context for ordinary send attempts on team-owned conversations; logs include conversation id, team id, outcome, and error code only.
Known follow-up for next round:
- Start Round 2 only after confirmation; implement Team mailbox task execution and related E2E flow per this plan.
Commit:
- AionCore: `8bbc837d3e24652a8b98ab18154a81d187f8d1dc`
- AionUi: `0882d7b38695575db04212e30e585bd4938f1609`

### Round 2 Handoff - Team Message Projection Boundary

Status: completed
Worktrees:
- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture`
- AionUi: not touched; `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture` remained clean.
Changed:
- AionCore `crates/aionui-team/src/visibility.rs`: added explicit `TeamVisibilityPolicy` decisions and user-visible system-note stripping.
- AionCore `crates/aionui-team/src/message_projection.rs`: added `TeamProjectionRequest`, `TeamProjectionSource`, `TeamMessageProjection`, projection store trait, teammate mirror dedupe by mailbox id + target conversation id, and `team.teammateMessage` broadcast with persisted `msg_id`.
- AionCore `crates/aionui-team/src/session.rs`: moved user right-bubble and teammate left-bubble construction to `TeamMessageProjection`; mailbox writes remain in TeamSession.
- AionCore `crates/aionui-team/src/events.rs`, `crates/aionui-team/src/lib.rs`: exported the new projection/visibility modules and centralized the teammate message event name.
- AionCore tests in `crates/aionui-team/tests/prompts_events_integration.rs`: added projection policy, user bubble, teammate bubble, dedupe, and event payload coverage.
- AionCore tests in `crates/aionui-team/tests/mailbox_integration.rs`: added coverage that mailbox file attachments remain mailbox-owned.
Deleted:
- Direct TeamSession construction of conversation `MessageRow` values for Team user/teammate visible bubbles.
- Direct `insert_raw_message` calls from TeamSession bubble paths.
- Old `team.teammate.message` emission from the TeamSession teammate mirror path.
Tests:
- `cargo fmt --all -- --check`: pass.
- `cargo test -p aionui-team mailbox_integration`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team e2e_team_flow`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team prompts_events_integration`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team --test mailbox_integration`: pass, 13 passed.
- `cargo test -p aionui-team --test e2e_team_flow`: pass, 18 passed, 2 ignored.
- `cargo test -p aionui-team --test prompts_events_integration`: pass, 17 passed.
- `cargo clippy -p aionui-team -- -D warnings`: pass.
- `rg -n "MessageRow|insert_raw_message" crates/aionui-team/src/session.rs`: pass, no matches.
- `rg -n "team\\.teammate\\.message|team\\.teammateMessage" crates/aionui-team/src crates/aionui-team/tests`: pass, only new `team.teammateMessage` production constant and test assertion matched.
Logs:
- added low-sensitive `info` projection logs with `team_id`, `slot_id`, `conversation_id`, `event_name`, and `outcome`; no prompt/content/file payloads are logged.
Known follow-up for next round:
- Start Round 3 only after confirmation; implement TeamAgentProvisioner and typed Team session context.
Commit:
- AionCore: `394a8f8`
- AionUi: not touched

### Round 3 Handoff - Team Agent Provisioner And Typed Session Context

Status: completed
Worktrees:
- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture`
- AionUi: not touched; `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture` remained clean.
Changed:
- AionCore `crates/aionui-api-types/src/team.rs`, `crates/aionui-api-types/src/lib.rs`: added typed Team runtime context DTOs `TeamSessionBinding`, `TeamRuntimeSeed`, and `TeamMcpRuntimeConfig`.
- AionCore `crates/aionui-ai-agent/src/session_context.rs`: added typed `team: Option<TeamSessionBinding>` to `AgentSessionContext` and propagated it into ACP/AionRS build contexts.
- AionCore `crates/aionui-conversation/src/session_context.rs`: made `SessionContextBuilder` parse Team conversation extra into typed Team context and derive `belongs_to_team` from that typed context.
- AionCore `crates/aionui-ai-agent/src/factory/acp.rs`, `crates/aionui-ai-agent/src/factory/aionrs.rs`: Team guide skip logic now consumes typed Team build context instead of raw Team JSON semantics.
- AionCore `crates/aionui-team/src/provisioning.rs`: added `TeamAgentProvisioner` for create/adopt conversation, typed Team binding/runtime seed writes, agent list persistence, Team MCP config writes, runtime attach/warmup, and session mode seed persistence.
- AionCore `crates/aionui-team/src/service.rs`, `crates/aionui-team/src/service/spawn_support.rs`, `crates/aionui-team/src/session.rs`, `crates/aionui-team/src/lib.rs`: moved create_team, add_agent, ensure_session rebuild, dynamic spawn persistence/attach, and session mode persistence onto the provisioner path while keeping scheduler decisions in TeamSession.
- AionCore tests in `crates/aionui-conversation/src/session_context.rs` and `crates/aionui-team/tests/session_service_integration.rs`: added typed Team context parser coverage and provisioning convergence coverage.
- AionCore test constructors in `crates/aionui-ai-agent/src/task_manager.rs`, `crates/aionui-ai-agent/tests/agent_types_integration.rs`, and `crates/aionui-ai-agent/tests/factory_provider_integration.rs`: updated for typed Team context field.
Deleted:
- Duplicate raw Team conversation extra construction from `TeamSessionService::create_team`, `TeamSessionService::add_agent`, `TeamSessionService::rebuild_agent_processes`, `TeamSessionService::set_session_mode`, `TeamSessionService::persist_spawned_agent`, and `TeamSession::attach_spawned_agent_process_bg`.
- Direct raw `belongs_to_team` derivation from ACP/AionRS build-context code paths; it is now derived from parsed typed Team context.
Tests:
- `cargo fmt --all -- --check`: pass.
- `cargo test -p aionui-conversation session_context`: pass, 12 passed.
- `cargo test -p aionui-ai-agent session`: pass, 99 passed; 2 ignored in `acp_agent_integration`; other integration filters matched 0 tests.
- `cargo test -p aionui-team session_service_integration`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team --test session_service_integration`: pass, 49 passed.
- `cargo clippy -p aionui-conversation -p aionui-ai-agent -p aionui-team -- -D warnings`: pass.
Logs:
- added low-sensitive provisioning lifecycle `info` logs with `team_id`, `slot_id`, `conversation_id`, and `outcome`; no prompt/input/tool/file/secrets are logged.
Known follow-up for next round:
- Start Round 4 only after confirmation; introduce `AgentTurnExecutionPort` and move Team event loop runtime execution behind the Team-defined port.
Commit:
- AionCore: `2c449d31dee86f753849095e886873a2f3e3b267`
- AionUi: not touched

### Round 4 Handoff - Agent Turn Execution Port

Status: completed
Worktrees:
- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture`
- AionUi: not touched; `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture` remained clean.
Changed:
- AionCore `crates/aionui-team/src/ports.rs`, `crates/aionui-team/src/lib.rs`: added the Team-owned `AgentTurnExecutionPort` boundary, request/outcome DTOs, source metadata, and execution status/error types.
- AionCore `crates/aionui-team/src/event_loop.rs`: replaced direct warmup/runtime-claim/StreamRelay/agent-send execution with `AgentTurnExecutionPort::run_agent_turn`; Team-owned mirror, mark-read, finalize, and cascade wake remain in the event loop.
- AionCore `crates/aionui-team/src/session.rs`, `crates/aionui-team/src/service.rs`: injected the turn port through TeamSessionService, TeamSession, and initial/dynamic event loop registration; removed inline direct-send wake fallback.
- AionCore `crates/aionui-conversation/src/service.rs`, `crates/aionui-conversation/src/turn_orchestrator.rs`, `crates/aionui-conversation/src/lib.rs`: exposed a conversation-owned, awaitable `run_agent_turn` API that reuses `ConversationTurnOrchestrator` for build/send/StreamRelay/persistence/release while ordinary `send_message` remains route/API specific.
- AionCore `crates/aionui-app/src/router/team_turn_adapter.rs`, `crates/aionui-app/src/router/state.rs`, `crates/aionui-app/src/router/mod.rs`, `crates/aionui-app/Cargo.toml`: added app composition adapter `TeamConversationTurnAdapter` and wired it into Team state construction.
- AionCore tests in `crates/aionui-team/tests/e2e_team_flow.rs`, `crates/aionui-team/src/session.rs`, `crates/aionui-team/tests/session_service_integration.rs`: updated wake-path expectations from direct agent send to Team turn port / mailbox-retained semantics.
- AionCore `crates/aionui-conversation/tests/stream_relay_tool_call.rs`: added coverage that `ConversationService::run_agent_turn` still drives StreamRelay behavior for empty tool-call ids.
- AionCore `docs/superpowers/plans/2026-06-10-team-mode-architecture-implementation.md`: added this Round 4 handoff to the worktree copy of the plan.
Deleted:
- Team event loop direct imports/usages of `aionui_conversation::stream_relay::StreamRelay`, `aionui_ai_agent::types::SendMessageData`, `ConversationService::runtime_state().try_claim_turn`, `TurnClaim`, and direct `IAgentTask::send_message`.
- `TeamSession::try_wake_inline` legacy fallback path; messages remain in mailbox until a registered event loop drains them.
Tests:
- `cargo fmt --all -- --check`: pass.
- `cargo test -p aionui-conversation stream_relay_tool_call`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-conversation --test stream_relay_tool_call`: pass, 2 passed.
- `cargo test -p aionui-team scheduler_integration`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team --test scheduler_integration`: pass, 19 passed.
- `cargo test -p aionui-team e2e_team_flow`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team --test e2e_team_flow`: pass, 18 passed, 2 ignored.
- `cargo test -p aionui-app team_e2e`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-app --test team_e2e`: pass, 41 passed.
- `cargo test -p aionui-team`: pass, 332 lib tests passed; integration tests passed including 18 passed/2 ignored in e2e_team_flow and 49 session_service_integration tests.
- `cargo clippy -p aionui-team -p aionui-conversation -p aionui-app -- -D warnings`: pass.
- `rg -n "StreamRelay|SendMessageData|try_claim_turn|IAgentTask|send_message\\(" crates/aionui-team/src`: pass; only Team API/MCP/scheduler/session method names and tests named `send_message` matched, with no runtime execution imports or direct agent-send calls.
Logs:
- added Team event loop `info`/`warn` logs around each turn port call with `team_id`, `slot_id`, `conversation_id`, `turn_id` when available, `outcome`, and no prompt/content/file/tool payloads.
Known follow-up for next round:
- Start Round 5 only after confirmation; sync shared ConversationService assembly and Team event-name cleanup per the plan.
Commit:
- AionCore: `2144419cd48935a6029eaa5b227620f92fd4546a`
- AionUi: not touched

### Round 5 Handoff - Shared Conversation Service And Event Names

Status: completed
Worktrees:
- AionCore: `/Users/zhangyaxiong/workshop/iOfficeAI/AionCore/.worktrees/team-mode-architecture`
- AionUi: `/Users/zhangyaxiong/workshop/iOfficeAI/AionUi/.worktrees/team-mode-architecture`
Changed:
- AionCore `crates/aionui-app/src/services.rs`, `crates/aionui-app/src/router/state.rs`: moved the default `ConversationService` instance into `AppServices` and made both conversation route state and Team state clone that app-owned service for Team-facing adapters.
- AionCore `crates/aionui-team/src/events.rs`, `crates/aionui-team/src/service.rs`, `crates/aionui-team/src/mcp/server.rs`, `crates/aionui-team/src/scheduler/agent_lifecycle.rs`: switched Team WebSocket event names to final two-level camelCase names, added explicit `team.removed`/`team.renamed`/`team.listChanged` lifecycle broadcasts, and made shutdown acknowledgement internal rather than a legacy WebSocket event.
- AionCore `crates/aionui-api-types/src/team.rs`, `crates/aionui-api-types/src/lib.rs`: updated Team event payload documentation to final names and removed the unused shutdown event payload.
- AionCore Team docs/tests under `crates/aionui-team`: updated old event-name expectations and documentation so architecture search commands no longer find legacy names.
- AionUi `packages/desktop/src/common/adapter/ipcBridge.ts`, `packages/desktop/src/common/types/team/teamTypes.ts`: switched Team listener names to final events and added listener surfaces for `team.mcpStatus`, `team.taskChanged`, and `team.sessionChanged`.
- AionUi Team hooks `useTeamSession.ts`, `useTeamList.ts`, `useTeamCreatedRedirect.ts`: consumed final listener surfaces and removed stale legacy-event comments.
Deleted:
- Old backend event names `team.agent.status`, `team.agent.spawned`, `team.agent.removed`, `team.agent.renamed`, `team.agent.shutdown`, `team.teammate.message`, `team.list-changed`, and `team.mcp.status`.
- Old AionUi listener names for `team.agent.*`, `team.teammate.message`, and `team.list-changed`.
- Separate Team-owned construction of an unrelated `ConversationService` instance in `build_team_state`.
Tests:
- `cargo fmt --all -- --check`: pass.
- `cargo test -p aionui-team prompts_events_integration`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-team --test prompts_events_integration`: pass, 17 passed.
- `cargo test -p aionui-app team_e2e`: pass, but Cargo filter matched 0 tests.
- `cargo test -p aionui-app --test team_e2e`: pass, 41 passed.
- `cargo clippy -p aionui-team -p aionui-app -- -D warnings`: pass.
- `rg -n "team\\.agent\\.|team\\.teammate\\.message|team\\.list-changed|team\\.mcp\\.status" crates`: pass, no matches.
- `bun run lint`: pass, 730 existing warnings and 0 errors.
- `bun run test -- packages/desktop/src/renderer/pages/team`: fail, no matching Vitest files found for that path.
- `bun run test -- --run`: pass, 151 test files passed, 1 skipped; 1131 tests passed, 3 skipped.
- `rg -n "team\\.agent\\.|team\\.teammate\\.message|team\\.list-changed|team\\.mcp\\.status" packages/desktop/src`: pass, no matches.
Logs:
- added low-sensitive lifecycle broadcast logs with `team_id`, final `event_name`, and action where applicable; existing projection and turn logs continue to use final event names without prompt/content/tool/file payloads.
Known follow-up for next round:
- Start Round 6 only after confirmation; perform final cleanup, regression searches, and architecture verification.
Commit:
- pending; commit will be created after this handoff update.
