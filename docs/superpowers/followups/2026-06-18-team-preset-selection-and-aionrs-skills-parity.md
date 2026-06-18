# Team Preset Selection And Aionrs Skills Parity Follow-Ups

Date: 2026-06-18

This follow-up tracks confirmed D6/D7 target-contract work that is intentionally deferred from the first implementation round in `docs/superpowers/plans/2026-06-18-team-mcp-and-prompt-injection-implementation-plan.md`.

## Deferred D6 Selection-Phase Work

- Fill the Team Leader prompt `available_assistants` section from the live enabled preset assistant catalog.
- Implement `team_describe_assistant` against the live assistant catalog for the selection phase.
- Make `team_spawn_agent(custom_agent_id=...)` derive backend/model defaults from the live assistant definition when the caller does not pass compatible overrides.
- Make `team_spawn_agent` response report whether assistant rules, skills, and MCP defaults were applied to the spawned teammate snapshot.

## Deferred D7 Aionrs Skills Work

- Implement native Aionrs skill materialization from the frozen `AionrsBuildExtra.skills` snapshot once there is a stable Aionrs skill-loading path.
- Add runtime tests proving Aionrs Team preset assistants can use frozen skills after resume without re-reading the live assistant catalog.

## Non-Deferred First-Round Guarantees

- Team preset assistant rules, skills, model/default metadata, and MCP defaults are frozen into conversation runtime state at Team creation/spawn time.
- ACP consumes frozen assistant rules, skills, and MCP defaults from conversation extra.
- Aionrs consumes frozen assistant rules and MCP defaults from conversation extra.
- Aionrs parses and preserves frozen skills in `AionrsBuildExtra.skills`; only native skill loading is deferred.
