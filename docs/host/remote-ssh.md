# Headless Host over SSH (E14)

Brand: **CSBU WorkMate**. Transport uses **system OpenSSH** only (MIT/BSD).
No custom SSH server. No AGPL terminal stacks.

## Defaults (D14)

| ID | Rule |
|----|------|
| D14-1 | Headless binds **loopback** (`127.0.0.1` / `::1`) unless `--allow-non-loopback-bind` |
| D14-2 | Existing workspace auth required — `--local` / `--identity-mode local` are rejected with `--headless` |
| D14-3 | No password-in-argv; use SSH agent / keys / WorkMate token files (`0600`) |
| D14-4 | Plugin network allowlist unchanged |

## Start headless Host (remote machine)

```bash
aioncore --headless --host 127.0.0.1 --port 25808
# stdout includes: WORKMATE_HOST_READY mode=headless url=http://127.0.0.1:25808/health
```

Health check (local to the Host):

```bash
curl -fsS http://127.0.0.1:25808/health
```

## Desktop / client tunnel (system OpenSSH)

On the machine running WorkMate desktop/WebUI:

```bash
ssh -N -L 25808:127.0.0.1:25808 user@remote-host
```

Point the client at `http://127.0.0.1:25808` (same protocol as a local Host).
The tunnel is opaque transport; auth remains WorkMate workspace credentials.

Optional reverse tunnel (Host reaches client services) is out of scope for the
first PR — document only when needed.

## Resume cursors (contract)

Realtime envelopes may carry an optional `cursor: { epoch, sequence }`
(`aionui_api_types::EventCursor`). Clients should store the last acknowledged
cursor and send it on reconnect. Full event-log replay is a follow-up; the
type is stable for forward compatibility.

## Smoke

See `scripts/remote-host-ssh-smoke.sh` (manual / local; not required in CI when
no sshd is available).

## Non-goals

- Vendoring or forking AGPL PTY/SSH servers
- Anonymous remote access
- Shipping a WorkMate-branded SSH daemon
