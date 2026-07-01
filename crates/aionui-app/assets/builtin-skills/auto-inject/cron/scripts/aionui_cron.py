#!/usr/bin/env python3
"""
AionUi cron helper.

Discovers the running aioncore REST API and manages cron jobs for the current
conversation using AIONUI_CONVERSATION_ID and AIONUI_USER_ID from the agent
runtime environment.
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request


def _candidate_ports():
    ports = []
    seen = set()

    def add(port):
        if port not in seen:
            seen.add(port)
            ports.append(port)

    try:
        out = subprocess.run(
            ["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-c", "aioncore"],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout
        for line in out.splitlines():
            for token in line.split():
                if token.startswith("127.0.0.1:"):
                    try:
                        add(int(token.split(":")[1]))
                    except ValueError:
                        pass
    except Exception:
        pass

    if not ports:
        try:
            out = subprocess.run(["netstat", "-ano"], capture_output=True, text=True, timeout=5).stdout
            for line in out.splitlines():
                if "LISTENING" in line and "127.0.0.1:" in line:
                    try:
                        add(int(line.split("127.0.0.1:")[1].split()[0]))
                    except (IndexError, ValueError):
                        pass
        except Exception:
            pass

    add(25808)  # aioncore default port
    add(13400)  # legacy documented helper fallback
    return ports


def _normalize_base_url(base):
    return base.rstrip("/")


def _configured_base_url():
    value = os.environ.get("AIONUI_BASE_URL", "").strip()
    return _normalize_base_url(value) if value else None


def _request_json(method, url, body=None, headers=None, timeout=2):
    data = None if body is None else json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json", **(headers or {})},
        method=method.upper(),
    )
    response = urllib.request.urlopen(req, timeout=timeout)
    raw = response.read()
    if not raw:
        return {}
    return json.loads(raw)


def _probe_health(base):
    try:
        _request_json("get", f"{base}/health")
        return True
    except Exception:
        pass

    try:
        return _request_json("get", f"{base}/api/assistants").get("success") is True
    except Exception:
        return False


def _probe_conversation_cron(base, headers):
    for attempt in range(3):
        try:
            _request_json("get", f"{base}/api/internal/conversation-cron/list", headers=headers)
            return True
        except urllib.error.HTTPError as error:
            return error.code != 404
        except Exception:
            if attempt == 2:
                return False
            time.sleep(0.05)


def _probe_base(base, headers=None):
    if headers:
        return _probe_conversation_cron(base, headers)
    return _probe_health(base)


def discover(headers=None):
    configured = _configured_base_url()
    if configured:
        if _probe_base(configured, headers):
            return configured
        raise SystemExit(f"AionUi backend not found at AIONUI_BASE_URL: {configured}")

    for port in _candidate_ports():
        base = f"http://127.0.0.1:{port}"
        if _probe_base(base, headers):
            return base
    raise SystemExit("AionUi backend not found. Is the app running?")


def _env_value(name):
    value = os.environ.get(name, "").strip()
    return value or None


def _required_value(name, value):
    if not value:
        raise SystemExit(f"Missing required environment variable: {name}")
    return value


def _required_env(name):
    return _required_value(name, _env_value(name))


def _request(method, path, body, headers):
    base = discover(headers)
    try:
        return _request_json(method, base + path, body, headers, timeout=15)
    except urllib.error.HTTPError as error:
        raise SystemExit(f"HTTP {error.code}: {error.read().decode()}")


def _read_stdin_payload(command):
    raw = sys.stdin.read()
    if not raw.strip():
        raise SystemExit(f"{command} requires a JSON payload on stdin")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as error:
        raise SystemExit(f"Invalid JSON payload on stdin: {error}") from error


def create(payload):
    headers = _conversation_headers()
    return _request(
        "post",
        "/api/internal/conversation-cron/create",
        payload,
        headers,
    )


def list_jobs():
    return _request("get", "/api/internal/conversation-cron/list", None, _conversation_headers())


def update(job_id, payload):
    return _request(
        "put",
        f"/api/internal/conversation-cron/jobs/{job_id}",
        payload,
        _conversation_headers(),
    )


def _conversation_headers():
    conversation_id = _required_env("AIONUI_CONVERSATION_ID")
    user_id = _required_env("AIONUI_USER_ID")
    return {
        "x-aionui-conversation-id": conversation_id,
        "x-aionui-user-id": user_id,
    }


def build_parser():
    parser = argparse.ArgumentParser(description="Manage AionUi cron jobs for the current conversation.")
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("discover", help="print the discovered aioncore base URL")
    subparsers.add_parser("list", help="list cron jobs for the current conversation")
    subparsers.add_parser("create", help="create a cron job from a JSON payload on stdin")
    update_parser = subparsers.add_parser("update", help="update a cron job from a JSON payload on stdin")
    update_parser.add_argument("--job-id", required=True, help="cron job id to update")
    return parser


def main(argv):
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command == "discover":
        print(discover())
        return 0
    if args.command == "create":
        print(json.dumps(create(_read_stdin_payload("create")), ensure_ascii=False, indent=2))
        return 0
    if args.command == "list":
        print(json.dumps(list_jobs(), ensure_ascii=False, indent=2))
        return 0
    if args.command == "update":
        print(json.dumps(update(args.job_id, _read_stdin_payload("update")), ensure_ascii=False, indent=2))
        return 0
    parser.print_usage()
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
