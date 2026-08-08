# Black-box qualification checklist — trusted local provisioning

Downstream pc-tools and release gates must produce **native evidence** for each
checked AC. Unit/integration green on this branch is **not** fleet release
authorization.

Minimum supported versions: see [README.md](./README.md).

## How to run (when durable path is available)

```bash
# 1) Start AionUi / aioncore for an adopted AionPro principal (not --local).
# 2) Discover without caller port:
aioncore --data-dir "$AIONUI_DATA_DIR" provision discover
aioncore --data-dir "$AIONUI_DATA_DIR" provision attest

# 3) Authorize least privilege:
echo '{"protocol_version":1,"installation_id":"...","profile_id":"...","scopes":["assistant_management"]}' \
  | aioncore --data-dir "$AIONUI_DATA_DIR" provision authorize

# 4) Exercise lifecycle scripts under a non-admin ordinary user account.
```

**Forbidden during qualification:** `--local`, conversation runtime tokens,
cookie/CSRF extraction, port scanning, direct DB/filesystem edits, machine-token
forwarding, `system_default_user` authority.

---

## A0 acceptance criteria (iOfficeAI/AionCore#795)

- [ ] **A0-AC1:** Non-conversation process reaches correct installation/profile without caller-provided port or browser/session state.
  - Evidence: `provision discover` against real install data-dir on macOS + Windows.
- [ ] **A0-AC2:** Obtains least-privilege scope and reads exact attested installation/profile/subject before any write.
  - Evidence: `provision attest` + `authorize` with subject attested (not Unknown/Absent).
- [ ] **A0-AC3:** On adopted AionPro principal: create disabled managed assistant, write rule + five axes, UI visibility/readback, update, disable, delete.
  - Blocked on: durable adopted-principal write path.
- [ ] **A0-AC4:** Concurrent UI edit yields deterministic conflict; no silent overwrite.
  - Partial: engine unit test for `PROVISION_CONCURRENT_CONFLICT`.
- [ ] **A0-AC5:** Managed provenance survives backend restart and compatible app upgrade.
  - Blocked on: persistence.
- [ ] **A0-AC6:** Account switch blocks old subject; distinct new identity; no rebinding.
  - Partial: grant revoke fail-closed unit tests.
- [ ] **A0-AC7:** Expired/revoked/wrong-profile authority returns stable codes and zero mutation.
  - Partial: unit tests; native path pending.
- [ ] **A0-AC8:** Full black-box assistant lifecycle on ordinary-user macOS and Windows second-release upgrade paths.
  - Unchecked (native).
- [ ] **A0-AC9:** Running-app and closed-app behavior explicit; never targets local-default identity.
  - Partial: closed-app discovery + no `system_default_user` test-subject path.
- [ ] **A0-AC10:** Scoped MCP and skill create/update/read/remove; foreign/user unchanged; assistant-only scope cannot invoke MCP/skill.
  - Partial: unit tests for scope separation + foreign preserve.
- [ ] **A0-AC11:** Assistant readback exposes exact Team references/unknown; delete refuses Team-referenced assistant.
  - Partial: engine unit test.

### A0 mandatory negative cases

- [ ] Wrong installation/profile/subject fails before write.
- [ ] Missing/expired/revoked credentials are not anonymous authority.
- [ ] No port scan, cookie/CSRF/runtime-token reuse, direct DB/filesystem write, machine-token forwarding, or `system_default_user` fallback.
- [ ] Partial/failed assistant transaction cannot leave newly enabled hybrid resource or false success.
- [ ] Dependency removal cannot delete foreign/user resources or still-referenced managed dependency.
- [ ] Team adjacency cannot be silently omitted or guessed absent.

---

## A1 acceptance criteria (iOfficeAI/AionCore#798)

- [ ] **A1-AC1:** Non-conversation process reaches attested principal and obtains Team-only authority.
- [ ] **A1-AC2:** Create Team with duplicate assistant refs in distinct slots, stable ordered member keys, one leader, per-slot native IDs/conversations, no runtime startup.
- [ ] **A1-AC3:** Invalid leader/key/assistant/principal/revision/native-ID inputs fail before mutation.
- [ ] **A1-AC4:** Rename/reorder/rename-member/add/remove/replace/model-refresh/leader-replace conditional + exact readback.
- [ ] **A1-AC5:** Concurrent UI/agent/provisioner edits conflict deterministically.
- [ ] **A1-AC6:** Active/starting/stopping/removing/unknown runtime is zero mutation unless proven vendor-atomic safe transition exists.
- [ ] **A1-AC7:** Member replacement/removal reports exact old-conversation disposition; hybrid roster cannot be reported converged.
- [ ] **A1-AC8:** Delete requires exact provenance/revision, verifies absence, reports disposition of every owned resource.
- [ ] **A1-AC9:** Definition operations never create/mutate runs, messages, tasks, mailbox, leases, runtime tokens, session mode, or UI preferences.
- [ ] **A1-AC10:** Provenance and logical-member/native-slot mapping survive restart and compatible upgrade.
- [ ] **A1-AC11:** Account switch, wrong subject, expiry/revocation, unsupported version, closed app, downgrade are stable zero-mutation results.
- [ ] **A1-AC12:** Full lifecycle on ordinary-user macOS and Windows second-release upgrade paths with adopted principals.

### A1 mandatory negative cases

- [ ] Names, positions, prefixes, and native IDs alone cannot identify/adopt/delete Team resources.
- [ ] No `system_default_user`, `--local`, cookies, conversation/runtime tokens, port scanning, direct DB/filesystem edit, or caller-selected native authority.
- [ ] Team-only scope cannot mutate assistants/MCPs/skills.
- [ ] Runtime uncertainty is never interpreted as idle.
- [ ] Delete cannot report success while any required owned-resource disposition is unknown.

---

## CI-covered today (not a substitute for native ACs)

| Test location | Covers |
| --- | --- |
| `aionui-api-types` `provisioning` unit tests | Protocol parse shapes, scope serde, error code stability |
| `aionui-app` `provisioning::engine` unit tests | Scope separation, wrong profile, expired/revoked, concurrent conflict, team leader/keys, runtime busy, Team adjacency delete refuse, disposition |
| `aionui-app` `provisioning::endpoint` unit tests | Endpoint file roundtrip, loopback host rewrite |
| `aionui-app` CLI parse tests | `aioncore provision …` clap surface |
| `aionui-app` capabilities e2e | Top-level index advertises provision domain |


## macOS evidence captured 2026-08-08 (ordinary user, adopted AionPro)

Against `/Applications/AionUi.app` user data at
`~/Library/Application Support/AionUi/aionui` with cloud subject attested from
`auth.enc` (not cookie/runtime-token):

| AC | Result |
|----|--------|
| A0-AC1 | **pass (macOS)** — `provision discover` without caller port |
| A0-AC2 | **pass (macOS)** — `provision attest` returns attested aionpro subject |
| A0-AC3 | **partial (macOS protocol engine)** — create disabled assistant + five axes + readback + delete via durable engine; **not** live UI vendor-table write |
| A0-AC4 | **pass (macOS)** — concurrent conflict returns `PROVISION_CONCURRENT_CONFLICT` |
| A0-AC5 | **partial** — durable `runtime/provision-engine-state.json` survives CLI process restart; not app upgrade path |
| A0-AC9 | **partial** — closed-app discover; never `system_default_user` |
| A0-AC11 | **pass (macOS)** — Team-referenced assistant delete refused |
| A1-AC2 | **pass (macOS protocol)** — team create with duplicate assistant refs, one leader, no runtime start |
| A1-AC3/6/8/9 | **partial/pass (macOS protocol)** — invalid/runtime/delete disposition/no runtime start |
| A0-AC8 / A1-AC12 | **still open** — Windows + second-release upgrade matrix |

Evidence logs (implementer session): `1082-native/macos-a0-discover-attest.txt`, `macos-a0-lifecycle.txt`.

## What remains blocked for native qualification

1. Durable store for managed provenance under adopted AionPro principals.
2. Production attestation of the running subject (no test env var, no cookie scrape).
3. Ordinary-user macOS + Windows second-release upgrade black-box runs.
4. UI visibility verification for managed assistants/teams after reconcile.

Until those land, keep #795 and #798 open and name unmet AC IDs explicitly.
Do not propose unsafe workarounds.
