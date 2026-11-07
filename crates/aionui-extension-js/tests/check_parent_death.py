#!/usr/bin/env python3
"""Linux opt-in Core direct-helper parent-death test; not a sandbox/runtime-shutdown test."""
import argparse
import ctypes
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("core")
options = parser.parse_args()
if sys.platform != "linux" or not __debug__:
    raise SystemExit("Requires Linux and Python without -O")
core = str(Path(options.core).resolve(strict=True))
key = "WORKMATE_EXTENSION_JS_PARENT_PID"
request = b'{"source":"export function run(){return null}","input":null}'
normal = subprocess.run([core, "--internal-extension-js"], input=request,
    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=3, env={key: str(os.getpid())})
assert normal.returncode == 0 and json.loads(normal.stdout) == {"ok": True, "result": None}, normal.stdout
for value, expected in [(None, "supervision_required"), ("invalid", "supervision_required"),
        ("", "supervision_required"), ("0", "supervision_required"),
        ("1", "supervision_required"), ("-1", "supervision_required"),
        ("999999999999999999999999", "supervision_required"),
        (b"\xff", "supervision_required"), (str(os.getpid()+1), "parent_exited")]:
    env = {} if value is None else {key: value}
    child = subprocess.Popen([core, "--internal-extension-js"], stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    try:
        child.wait(timeout=3)  # Keep stdin OPEN: rejection must precede reading.
        stdout, _ = child.communicate(timeout=1)
        assert child.returncode == 2 and json.loads(stdout) == {"ok": False, "error": expected}, stdout
    finally:
        if child.poll() is None:
            child.kill()
            child.communicate(timeout=1)

# Test process ONLY. No machine-wide setting and no production fixture mode.
libc = ctypes.CDLL(None, use_errno=True)
PR_SET_CHILD_SUBREAPER = 36
libc.prctl.argtypes = [ctypes.c_int]
libc.prctl.restype = ctypes.c_int
assert libc.prctl(PR_SET_CHILD_SUBREAPER, ctypes.c_ulong(1), ctypes.c_ulong(0),
    ctypes.c_ulong(0), ctypes.c_ulong(0)) == 0, ctypes.get_errno()
intermediary = r'''
import os, subprocess, sys
child = subprocess.Popen([sys.argv[1], "--internal-extension-js"], stdin=int(sys.argv[2]),
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    env={"WORKMATE_EXTENSION_JS_PARENT_PID": str(os.getpid())})
print(child.pid, flush=True)
sys.stdin.buffer.read()
'''
def wait_adopted(pid, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            waited, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            # Adoption may lag observed parent exit; ECHILD is not an exit status.
            waited = 0
        if waited == pid:
            return status
        time.sleep(0.005)
    raise TimeoutError("helper was not adopted and reaped within the test budget")

counts = {"signal": 0, "early_reject": 0}
for delay in [0.0] * 8 + [0.1] * 4:
    read_fd, write_fd = os.pipe()
    parent = None
    pid = None
    reaped = False
    try:
        parent = subprocess.Popen([sys.executable, "-u", "-c", intermediary, core, str(read_fd)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, pass_fds=(read_fd,))
        os.close(read_fd)
        read_fd = None
        assert select.select([parent.stdout], [], [], 3)[0], "intermediary did not report helper PID"
        line = parent.stdout.readline(32)
        assert line.endswith(b"\n") and line.strip().isdigit(), line
        pid = int(line)
        if delay:
            time.sleep(delay)
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        assert state != "Z", "helper exited before parent kill"
        parent.kill()
        parent.wait(timeout=3)
        status = wait_adopted(pid, 3)
        reaped = True
        # write_fd is deliberately still open here: neither EOF nor a broken
        # output pipe may substitute for the parent-death guard in this test.
        if os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL:
            counts["signal"] += 1
        else:
            assert delay == 0 and os.WIFEXITED(status) and os.WEXITSTATUS(status) == 2, status
            counts["early_reject"] += 1
        assert not Path(f"/proc/{pid}").exists(), "helper must be reaped, not a zombie"
    finally:
        try:
            if parent is not None:
                if parent.poll() is None:
                    parent.kill()
                parent.wait(timeout=3)
            if pid is not None and not reaped:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                wait_adopted(pid, 3)
        finally:
            # Even failed adoption/cleanup must close the independent writer.
            if read_fd is not None:
                os.close(read_fd)
            os.close(write_fd)
            if parent is not None:
                for stream in (parent.stdin, parent.stdout, parent.stderr):
                    if stream is not None:
                        stream.close()
print(f"PASS: parent guard metadata preflight/refusals; 12 independent-writer parent-kill cases {counts}; adopted helpers waited")
