#!/usr/bin/env python3
"""AionUi troubleshooting helper — engine-agnostic.

Discovers the live aioncore backend (port, log-dir, data-dir, version) from the
running process, then exposes troubleshooting reads over the backend's REST API,
its SQLite store, and its log files. Nothing here is specific to a single engine
(claude / aionrs / gemini / openclaw); everything goes through AionUi's own
project-level data so the same checks work no matter which agent backend a
conversation runs on.

Usage:
  python3 aion_diag.py discover          # locate backend: port, log-dir, data-dir, version
  python3 aion_diag.py health            # GET /health
  python3 aion_diag.py get <path>        # raw GET against the REST API (e.g. /api/teams)

  python3 aion_diag.py conversations [--limit N]      # list conversations + runtime state
  python3 aion_diag.py conversation <id>              # one conversation: status + runtime + recent errors
  python3 aion_diag.py messages <id> [--limit N] [--errors]   # messages for a conversation (SQLite)

  python3 aion_diag.py providers         # LLM providers + model_health (api_key REDACTED)
  python3 aion_diag.py mcp               # MCP servers + tool counts (flags enabled-but-0-tools)
  python3 aion_diag.py teams             # teams + members + each member's conversation state
  python3 aion_diag.py crons             # scheduled jobs from SQLite (no REST API for these)

  python3 aion_diag.py logs [--lines N] [--errors] [--conv <id>]   # tail aioncore log
  python3 aion_diag.py overview          # one-shot health snapshot across everything

Exit code 3 == backend not running / not discoverable.
"""
import sys
import os
import re
import json
import sqlite3
import subprocess
import urllib.request
import urllib.error
import glob

# ── discovery ────────────────────────────────────────────────────────────────


def _ps_aioncore():
    """Return (pid, full_command) for the aioncore backend process, or (None, None).

    The long-lived backend process is aioncore. Short-lived helper subprocesses
    may include `mcp-team-stdio` for active Team sessions.
    """
    try:
        out = subprocess.check_output(["ps", "-axo", "pid,command"], text=True)
    except Exception:
        return None, None
    for line in out.splitlines():
        if "aioncore" in line and "--data-dir" in line and "--port" in line:
            line = line.strip()
            pid = line.split(None, 1)[0]
            cmd = line.split(None, 1)[1] if " " in line else ""
            if pid.isdigit():
                return int(pid), cmd
    return None, None


def _arg(cmd, flag):
    """Extract a `--flag VALUE` from a command string (VALUE has no spaces here)."""
    m = re.search(re.escape(flag) + r"\s+(\S+)", cmd or "")
    return m.group(1) if m else None


def _listen_ports(pid):
    """All TCP ports the given pid is LISTENing on."""
    try:
        out = subprocess.check_output(["lsof", "-nP", "-p", str(pid)], text=True,
                                      stderr=subprocess.DEVNULL)
    except Exception:
        return []
    ports = set()
    for line in out.splitlines():
        if "LISTEN" in line:
            m = re.search(r":(\d+)\s+\(LISTEN\)", line)
            if m:
                ports.add(int(m.group(1)))
    return sorted(ports)


def _try_health(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1.5) as r:
            body = json.loads(r.read().decode())
            if isinstance(body, dict) and body.get("status") == "ok":
                return body
    except Exception:
        pass
    return None


_CACHE = {}


def discover(quiet=False):
    """Locate the backend. Returns dict with base_url/port/log_dir/data_dir/version,
    or exits(3) if not found. Memoized per-process."""
    if _CACHE:
        return _CACHE
    pid, cmd = _ps_aioncore()
    if not pid:
        if not quiet:
            sys.stderr.write(
                "AionUi backend (aioncore) is not running. Ask the user to launch "
                "AionUi — do not guess a port.\n")
        sys.exit(3)
    log_dir = _arg(cmd, "--log-dir")
    data_dir = _arg(cmd, "--data-dir")
    version = _arg(cmd, "--app-version")
    # aioncore is started with `--port 0` (dynamic), so the real REST port is NOT
    # in argv. Probe every port it listens on and keep the one answering /health.
    base = None
    port = None
    for p in _listen_ports(pid):
        if _try_health(p):
            base, port = f"http://127.0.0.1:{p}", p
            break
    if not base:
        if not quiet:
            sys.stderr.write(
                f"Found aioncore pid {pid} but no REST port answered /health "
                f"(ports tried: {_listen_ports(pid)}).\n")
        sys.exit(3)
    info = {
        "pid": pid,
        "base_url": base,
        "port": port,
        "log_dir": log_dir,
        "data_dir": data_dir or os.path.expanduser("~/.aionui"),
        "version": version,
        "db_path": os.path.join(data_dir or os.path.expanduser("~/.aionui"),
                                "aionui-backend.db"),
    }
    _CACHE.update(info)
    return info


# ── REST helpers ─────────────────────────────────────────────────────────────


def api_get(path):
    base = discover()["base_url"]
    url = base + path if path.startswith("/") else base + "/" + path
    try:
        with urllib.request.urlopen(url, timeout=10) as r:
            return r.getcode(), json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        return e.code, None
    except Exception as e:
        return None, {"_error": str(e)}


def _unwrap(body):
    """REST responses are {success, data}. Return data (or body itself)."""
    if isinstance(body, dict) and "data" in body:
        return body["data"]
    return body


# ── SQLite helpers ───────────────────────────────────────────────────────────


def _db():
    path = discover()["db_path"]
    if not os.path.exists(path):
        sys.stderr.write(f"SQLite store not found at {path}\n")
        sys.exit(3)
    # read-only so we never risk mutating the live store
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)
    conn.row_factory = sqlite3.Row
    return conn


def _rows(sql, params=()):
    conn = _db()
    try:
        return [dict(r) for r in conn.execute(sql, params).fetchall()]
    finally:
        conn.close()


# ── redaction ────────────────────────────────────────────────────────────────


def _redact(obj):
    """Recursively mask secret-ish fields so we never print plaintext keys."""
    SECRET = ("api_key", "apikey", "token", "secret", "password", "authorization")
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            if any(s in k.lower() for s in SECRET) and isinstance(v, str) and v:
                out[k] = v[:4] + "…REDACTED" if len(v) > 4 else "REDACTED"
            else:
                out[k] = _redact(v)
        return out
    if isinstance(obj, list):
        return [_redact(x) for x in obj]
    return obj


def _print(obj):
    print(json.dumps(_redact(obj), ensure_ascii=False, indent=2))


# ── commands ─────────────────────────────────────────────────────────────────


def cmd_discover(argv):
    _print(discover())


def cmd_health(argv):
    code, body = api_get("/health")
    print(f"HTTP {code}")
    _print(body)


def cmd_get(argv):
    if not argv:
        sys.exit("usage: get <path>")
    code, body = api_get(argv[0])
    print(f"HTTP {code}")
    _print(body)


def _conv_list(limit=50):
    code, body = api_get(f"/api/conversations?limit={limit}")
    data = _unwrap(body) or {}
    if isinstance(data, dict):
        return data.get("conversations") or data.get("items") or []
    return data if isinstance(data, list) else []


def cmd_conversations(argv):
    limit = _opt_int(argv, "--limit", 50)
    convs = _conv_list(limit)
    rows = []
    for c in convs:
        rows.append({
            "id": c.get("id"),
            "name": c.get("name"),
            "type": c.get("type"),        # engine: acp/aionrs/gemini/...
            "status": c.get("status"),
        })
    print(f"{len(rows)} conversations")
    _print(rows)


def cmd_conversation(argv):
    if not argv:
        sys.exit("usage: conversation <id>")
    cid = argv[0]
    code, body = api_get(f"/api/conversations/{cid}")
    d = _unwrap(body) or {}
    if not d or d.get("id") is None:
        _print({
            "error": "conversation not found",
            "id": cid,
            "hint": ("No conversation with this id in the live backend. It may have "
                     "been deleted, or the id is wrong — check `conversations` for the "
                     "current list."),
        })
        return
    runtime = d.get("runtime", {})
    # engine-agnostic stuck-detection hint
    stuck_hint = None
    if runtime.get("is_processing") and runtime.get("task_status") == "running":
        stuck_hint = ("state=running & is_processing=true — if this has not changed "
                      "across repeated checks, the turn may be stuck. Compare turn_id "
                      "over time and check recent errors + logs.")
    errs = _rows(
        "SELECT msg_id, type, substr(content,1,300) AS content, created_at "
        "FROM messages WHERE conversation_id=? AND status='error' "
        "ORDER BY created_at DESC LIMIT 5", (cid,))
    _print({
        "id": d.get("id"),
        "name": d.get("name"),
        "type": d.get("type"),
        "status": d.get("status"),
        "runtime": runtime,
        "stuck_hint": stuck_hint,
        "recent_errors": errs,
    })


def cmd_messages(argv):
    if not argv:
        sys.exit("usage: messages <id> [--limit N] [--errors]")
    cid = argv[0]
    limit = _opt_int(argv, "--limit", 30)
    where = "conversation_id=?"
    params = [cid]
    if "--errors" in argv:
        where += " AND status='error'"
    rows = _rows(
        f"SELECT msg_id, type, status, position, substr(content,1,500) AS content, "
        f"created_at FROM messages WHERE {where} "
        f"ORDER BY created_at DESC LIMIT ?", (*params, limit))
    print(f"{len(rows)} messages for {cid}")
    _print(rows)


def cmd_providers(argv):
    code, body = api_get("/api/providers")
    data = _unwrap(body) or []
    out = []
    for p in data:
        mh = p.get("model_health") or {}
        unhealthy = {m: h for m, h in mh.items()
                     if isinstance(h, dict) and h.get("status") != "healthy"}
        out.append({
            "id": p.get("id"),
            "name": p.get("name"),
            "platform": p.get("platform"),
            "base_url": p.get("base_url"),
            "enabled": p.get("enabled"),
            "models": p.get("models"),
            "model_health": mh,
            "unhealthy_models": unhealthy or None,
        })
    _print(out)  # api_key is redacted by _print


def cmd_mcp(argv):
    code, body = api_get("/api/mcp/servers")
    data = _unwrap(body) or []
    out = []
    for s in data:
        tools = s.get("tools") or []
        enabled = s.get("enabled")
        n = len(tools) if isinstance(tools, list) else tools
        warn = None
        if enabled and (n == 0):
            warn = "enabled but 0 tools — server may have failed to start; check logs"
        out.append({
            "id": s.get("id"),
            "name": s.get("name"),
            "enabled": enabled,
            "builtin": s.get("builtin"),
            "transport": (s.get("transport") or {}).get("type"),
            "tool_count": n,
            "warning": warn,
        })
    _print(out)


def cmd_teams(argv):
    code, body = api_get("/api/teams")
    data = _unwrap(body) or []
    out = []
    for t in data:
        members = []
        for a in t.get("assistants") or t.get("agents") or []:
            cid = a.get("conversation_id")
            runtime = None
            if cid:
                _, cb = api_get(f"/api/conversations/{cid}")
                cd = _unwrap(cb) or {}
                runtime = cd.get("runtime", {}).get("state")
            members.append({
                "name": a.get("name"),
                "role": a.get("role"),
                "backend": a.get("backend"),
                "conversation_id": cid,
                "conv_state": runtime,
            })
        out.append({"id": t.get("id"), "name": t.get("name"), "members": members})
    _print(out)


def cmd_crons(argv):
    # No REST API for crons — read AionUi's SQLite store directly.
    rows = _rows(
        "SELECT id, name, enabled, schedule_kind, schedule_value, "
        "schedule_description, last_status, last_error, "
        "next_run_at, last_run_at, run_count, retry_count "
        "FROM cron_jobs ORDER BY last_run_at DESC")
    failing = [r for r in rows if r.get("last_status") in ("error", "missed")]
    _print({"total": len(rows), "failing": failing, "all": rows})


def _log_files():
    info = discover()
    log_dir = info.get("log_dir")
    if not log_dir or not os.path.isdir(log_dir):
        # platform fallback (Electron app.getPath('logs') convention)
        log_dir = os.path.expanduser("~/Library/Logs/AionUi")
    files = sorted(glob.glob(os.path.join(log_dir, "*.aioncore.log")),
                   key=lambda f: os.path.getmtime(f), reverse=True)
    return log_dir, files


def cmd_logs(argv):
    lines = _opt_int(argv, "--lines", 80)
    conv = _opt_str(argv, "--conv")
    only_err = "--errors" in argv
    log_dir, files = _log_files()
    print(f"log_dir: {log_dir}")
    if not files:
        print("no *.aioncore.log files found")
        return
    latest = files[0]
    print(f"latest: {latest}")
    with open(latest, "r", errors="replace") as f:
        tail = f.readlines()[-(lines * 5):]  # over-read, then filter
    out = []
    for ln in tail:
        if conv and conv not in ln:
            continue
        if only_err and not re.search(r'"(ERROR|WARN)"|error|panic', ln, re.I):
            continue
        out.append(ln.rstrip())
    for ln in out[-lines:]:
        print(ln)


def cmd_overview(argv):
    info = discover()
    snap = {"backend": {"version": info["version"], "port": info["port"],
                        "log_dir": info["log_dir"], "data_dir": info["data_dir"]}}
    # health
    _, h = api_get("/health")
    snap["health"] = h
    # providers
    _, pb = api_get("/api/providers")
    provs = _unwrap(pb) or []
    bad_prov = []
    for p in provs:
        mh = p.get("model_health") or {}
        for m, hh in mh.items():
            if isinstance(hh, dict) and hh.get("status") != "healthy":
                bad_prov.append({"provider": p.get("name"), "model": m,
                                 "status": hh.get("status")})
    snap["providers"] = {"count": len(provs), "unhealthy": bad_prov}
    # mcp
    _, mb = api_get("/api/mcp/servers")
    mcps = _unwrap(mb) or []
    bad_mcp = [s.get("name") for s in mcps
               if s.get("enabled") and not (s.get("tools") or [])]
    snap["mcp"] = {"count": len(mcps), "enabled_but_no_tools": bad_mcp}
    # crons
    try:
        crons = _rows("SELECT last_status FROM cron_jobs")
        snap["crons"] = {"total": len(crons),
                         "failing": sum(1 for c in crons
                                        if c.get("last_status") in ("error", "missed"))}
    except Exception as e:
        snap["crons"] = {"_error": str(e)}
    # stuck conversations (running + processing)
    stuck = []
    for c in _conv_list(50):
        if c.get("status") == "running":
            stuck.append({"id": c.get("id"), "name": c.get("name"),
                          "type": c.get("type")})
    snap["running_conversations"] = stuck
    _print(snap)


# ── arg helpers ──────────────────────────────────────────────────────────────


def _opt_int(argv, flag, default):
    if flag in argv:
        i = argv.index(flag)
        if i + 1 < len(argv):
            try:
                return int(argv[i + 1])
            except ValueError:
                pass
    return default


def _opt_str(argv, flag, default=None):
    if flag in argv:
        i = argv.index(flag)
        if i + 1 < len(argv):
            return argv[i + 1]
    return default


COMMANDS = {
    "discover": cmd_discover,
    "health": cmd_health,
    "get": cmd_get,
    "conversations": cmd_conversations,
    "conversation": cmd_conversation,
    "messages": cmd_messages,
    "providers": cmd_providers,
    "mcp": cmd_mcp,
    "teams": cmd_teams,
    "crons": cmd_crons,
    "logs": cmd_logs,
    "overview": cmd_overview,
}


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        print(__doc__)
        sys.exit(0 if len(sys.argv) < 2 else 2)
    COMMANDS[sys.argv[1]](sys.argv[2:])


if __name__ == "__main__":
    main()
