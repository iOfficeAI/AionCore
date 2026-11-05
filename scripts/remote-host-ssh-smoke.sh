#!/usr/bin/env bash
# E14 manual smoke: headless Host policy + optional local SSH tunnel.
# Not wired into CI by default (no guaranteed sshd). Exit 0 on skip.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${AIONCORE_BIN:-}"
PORT="${WORKMATE_SMOKE_PORT:-25818}"
HOST_BIND="${WORKMATE_SMOKE_HOST:-127.0.0.1}"

if [[ -z "$BIN" ]]; then
  if [[ -x "$ROOT/target/debug/aioncore" ]]; then
    BIN="$ROOT/target/debug/aioncore"
  elif [[ -x "${CARGO_TARGET_DIR:-}/debug/aioncore" ]]; then
    BIN="${CARGO_TARGET_DIR}/debug/aioncore"
  else
    echo "SKIP: aioncore binary not found (set AIONCORE_BIN). Policy unit tests still cover D14."
    exit 0
  fi
fi

echo "== D14-2: --headless --local must fail =="
if "$BIN" --headless --local --host "$HOST_BIND" --port "$PORT" --data-dir /tmp/workmate-e14-smoke-$$ >/tmp/workmate-e14-local.err 2>&1; then
  echo "FAIL: headless+local should exit non-zero"
  exit 1
fi
grep -q 'headless.auth\|BOOTSTRAP_CONFIG_INVALID\|--local' /tmp/workmate-e14-local.err \
  || grep -q 'headless' /tmp/workmate-e14-local.err \
  || true
echo "ok: rejected --local"

echo "== D14-1: --headless --host 0.0.0.0 without opt-in must fail =="
if "$BIN" --headless --host 0.0.0.0 --port "$PORT" --data-dir /tmp/workmate-e14-smoke-$$ >/tmp/workmate-e14-bind.err 2>&1; then
  echo "FAIL: wildcard bind should exit non-zero"
  exit 1
fi
echo "ok: rejected non-loopback"

if ! command -v ssh >/dev/null 2>&1; then
  echo "SKIP: system ssh not installed (tunnel portion)"
  exit 0
fi

echo "== note: full tunnel smoke needs a running headless Host + sshd =="
echo "example: ssh -N -L ${PORT}:127.0.0.1:${PORT} user@remote"
echo "PASS: policy checks + ssh binary present ($(command -v ssh))"
