# Trusted local provisioning protocol (A0 / A1)

Conversation-independent, least-privilege local provisioning for adopted
principals. Program epic: `sparkfn/pc-client#1082`.

| Track | Issue | Scope |
| --- | --- | --- |
| A0 | [iOfficeAI/AionCore#795](https://github.com/iOfficeAI/AionCore/issues/795) | assistants, MCP, skills + discovery/auth/attestation |
| A1 | [iOfficeAI/AionCore#798](https://github.com/iOfficeAI/AionCore/issues/798) | `team_definition` (separate scope) |

**Release boundary:** landing this surface authorizes merge consideration only.
It does **not** authorize a fleet release, deployment, promotion, or secret mutation.

## Entry points

```bash
# Capability contract (no runtime env, no port)
aioncore provision capabilities

# Port-free discovery via --data-dir endpoint advertisement
aioncore --data-dir /path/to/install/data provision discover
aioncore --data-dir /path/to/install/data provision attest

# Scoped grant + mutations use stdin JSON (see capabilities document)
aioncore provision authorize < authorize.json
aioncore provision assistants reconcile < assistant.json
aioncore provision teams create < team.json
```

Top-level index also lists the domain:

```bash
aioncore capabilities   # includes provision domain
```

## What this PR implements

| Area | Status |
| --- | --- |
| Versioned protocol types (`aionui-api-types::provisioning`) | Done |
| Scope advertisement: `assistant_management`, `mcp_configuration`, `skill_registration`, `team_definition` | Done |
| Data-dir endpoint discovery (no caller port / no port scan) | Done |
| Server writes/removes `runtime/local-provision-endpoint.json` | Done |
| Attestation shape (installation/profile/subject/capability version) | Done |
| Short-lived scoped grants with fail-closed checks | Done (in-process engine) |
| Conditional assistant reconcile + exact readback + Team adjacency | Skeleton |
| Conditional MCP / skill reconcile; foreign resource preservation | Skeleton |
| Team definition CRUD, one leader, no runtime start, disposition | Skeleton |
| Managed provenance fields | Done (engine + types) |
| Stable error codes | Done |
| Unit tests (parse, scopes, fail-closed) | Done |
| Black-box qualification checklist | Documented (pending native runs) |
| Durable adopted-principal persistence | **Not done** |
| Production principal attestation channel (no test env) | **Not done** |
| Native macOS/Windows ordinary-user black-box | **Not done** |

## Explicit non-goals / forbidden paths

The protocol surface must **not**:

- require `AIONUI_CONVERSATION_ID` / `AIONUI_RUNTIME_TOKEN`
- accept caller-provided port as authority
- port-scan, scrape cookies/CSRF, or reuse agent helper tokens
- fall back to `--local` / `system_default_user`
- let assistant-only grants mutate Teams (or Team-only grants mutate assistants/MCP/skills)
- start Team runtime from definition operations
- claim fleet release

## Minimum versions (surface)

| Component | Minimum for this protocol surface |
| --- | --- |
| AionCore | this branch / first release that includes `aioncore provision` |
| Protocol | `protocol_version = 1` |

Exact minimum versions for **adopted-principal durable writes** will be filled when
A0-AC3–AC11 / A1-AC2–AC12 have native evidence.

## AC coverage status

See [black-box-qualification.md](./black-box-qualification.md) for the full
checklist. Summary:

### A0 (iOfficeAI/AionCore#795)

| ID | Status | Notes |
| --- | --- | --- |
| A0-AC1 | Partial | Data-dir discovery implemented; native install path qualification pending |
| A0-AC2 | Partial | Attest + scoped grant shapes; production subject channel pending |
| A0-AC3 | Unchecked | Needs adopted AionPro durable write path |
| A0-AC4 | Partial | Engine enforces concurrent conflict; UI race on durable store pending |
| A0-AC5 | Unchecked | In-process provenance only; restart survival needs persistence |
| A0-AC6 | Partial | Revoke / account-switch fail-closed in engine |
| A0-AC7 | Partial | Stable codes + unit tests; native path pending |
| A0-AC8 | Unchecked | macOS/Windows black-box |
| A0-AC9 | Partial | Closed-app explicit; no local-default fallback |
| A0-AC10 | Partial | Scope separation + foreign preserve unit-tested |
| A0-AC11 | Partial | Team adjacency exact in engine readback |

### A1 (iOfficeAI/AionCore#798)

| ID | Status | Notes |
| --- | --- | --- |
| A1-AC1 | Partial | Team-only scope grant |
| A1-AC2 | Partial | Create skeleton: duplicate assistant refs, ordered keys, one leader, no runtime start |
| A1-AC3 | Partial | Invalid leader/key fail-closed unit tests |
| A1-AC4 | Partial | Update path + exact readback skeleton |
| A1-AC5 | Partial | Concurrent conflict on expected_revision |
| A1-AC6 | Partial | Runtime busy fail-closed |
| A1-AC7 | Partial | Delete disposition structure |
| A1-AC8 | Partial | Delete requires revision; disposition reported |
| A1-AC9 | Partial | Definition ops do not start runtime |
| A1-AC10 | Unchecked | Persistence across restart |
| A1-AC11 | Partial | Wrong profile / expired / revoked / closed unit paths |
| A1-AC12 | Unchecked | Native macOS/Windows black-box |

## Logging

- Server: `info` when endpoint advertisement is written/removed; `warn` on
  degraded write/remove (`BOOTSTRAP_DEGRADED_PROVISION_ENDPOINT`).
- No production logs of grant secrets, rule content, MCP transport secrets, or
  prompts.

## Next executable steps (blocked for full AC close)

1. Wire durable storage + managed provenance for assistants/MCP/skills/teams under adopted principals.
2. Principal attestation from the running AionPro session without cookies/CSRF/runtime-token reuse.
3. Run [black-box-qualification.md](./black-box-qualification.md) on ordinary-user macOS and Windows second-release upgrade paths.
4. Keep A0/A1 issues open until native evidence lands; do not invent `--local` workarounds.
