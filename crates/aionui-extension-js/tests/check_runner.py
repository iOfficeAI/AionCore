#!/usr/bin/env python3
"""Linux feasibility probe only; not production or cross-platform acceptance."""
import argparse
import json
import os
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("runner")
parser.add_argument("--core", action="store_true", help="invoke the Core private helper entry")
options = parser.parse_args()
if not __debug__:
    raise SystemExit("Run without Python -O")
runner = str(Path(options.runner).resolve())
command = [runner] + (["--internal-extension-js"] if options.core else [])
# Pass only host-owned metadata, never an inherited plugin environment.
child_env = {"WORKMATE_EXTENSION_JS_PARENT_PID": str(os.getpid())} if options.core else {}
cases = [
    ("async JSON", "export async function run(x) { return {sum: x.a + x.b}; }", {"a": 2, "b": 3}, {"sum": 5}),
    ("sync JSON", "export function run(x) { return x; }", [1, None, True], [1, None, True]),
    ("host APIs absent", "export function run() { return [typeof process,typeof require,typeof fetch,typeof std,typeof os,typeof setTimeout]; }", None, ["undefined"] * 6),
    ("throw", "export function run() { throw Error('example'); }", None, None),
    ("missing export", "export const other = 1;", None, None),
    ("static import", "import * as os from 'os'; export function run() { return 1; }", None, None),
    ("dynamic import", "export async function run() { return await import('std'); }", None, None),
    ("pending promise", "export function run() { return new Promise(() => {}); }", None, None),
    ("sync loop", "export function run() { while(true) {} }", None, None),
    ("module loop", "while(true) {} export function run() { return 1; }", None, None),
    ("microtask loop", "export async function run() { while(true) { await Promise.resolve(); } }", None, None),
    ("serialization loop", "export function run() { return {toJSON() { while(true) {} }}; }", None, None),
    ("memory bound", "export function run() { const a=[]; while(true) a.push(new Array(100000).fill('x')); }", None, None),
    ("recursion", "export function run() { function f() { return f(); } return f(); }", None, None),
    ("output bound", "export function run() { return 'x'.repeat(100000); }", None, None),
    ("non JSON", "export function run() { return 1n; }", None, None),
    ("cycle", "export function run() { const x={}; x.x=x; return x; }", None, None),
]
codes = {
    "throw": "run_error", "Atomics blocking forbidden": "run_error", "missing export": "run_missing",
    "static import": "module_error", "dynamic import": "js_exception",
    "pending promise": "promise_pending", "sync loop": "deadline",
    "module loop": "deadline", "microtask loop": "job_limit",
    "serialization loop": "deadline", "memory bound": "run_error",
    "recursion": "run_error", "output bound": "output_limit",
    "non JSON": "serialization_error", "cycle": "serialization_error",
}
cases += [
    ("shared-memory API inventory", "export function run(){return [typeof SharedArrayBuffer,typeof Atomics.wait,typeof Atomics.waitAsync]}", None, ["function","function","undefined"]),
    ("Atomics blocking forbidden", "export function run(){return Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0,1000)}", None, None),
    ("thenable", "export function run() { return {then(resolve) { resolve({assimilated:true}); }}; }", None, {"assimilated": True}),
    ("replaced global Promise", "globalThis.Promise=undefined; export async function run(x) { await 0; return x; }", {"captured": True}, {"captured": True}),
    ("target-only background rejection policy", "export async function run() { Promise.resolve().then(()=>{throw Error('background')}); await 0; return {target:true}; }", None, {"target": True}),
]
checks = 0
for name, source, value, expected in cases:
    request = json.dumps({"source": source, "input": value}).encode()
    start = time.monotonic()
    # subprocess.run kills and waits for its child on timeout; no abandoned worker.
    response = subprocess.run(command, input=request, capture_output=True, timeout=3, env=child_env)
    assert response.returncode == 0, (name, response.returncode, response.stderr[:1000])
    assert len(response.stdout) <= 65536 + 256, (name, "unbounded protocol output")
    payload = json.loads(response.stdout)
    if expected is not None:
        assert payload == {"ok": True, "result": expected}, (name, payload)
    else:
        if name == "microtask loop":
            # Either budget may win on slower hosts; both are explicit bounded outcomes.
            assert payload in ({"ok":False,"error":"job_limit"}, {"ok":False,"error":"deadline"}), (name,payload)
        else:
            assert payload == {"ok": False, "error": codes[name]}, (name, payload)
    if name == "Atomics blocking forbidden":
        assert time.monotonic()-start < 0.8, "Atomics.wait did not reject promptly"
    checks += 1
    print(f"PASS {name}: {time.monotonic()-start:.3f}s {payload.get('error', 'result')}", flush=True)
def raw_case(name, request, error):
    result = subprocess.run(command, input=request, capture_output=True, timeout=3, env=child_env)
    assert result.returncode == 0, (name, result.stderr[:1000])
    assert json.loads(result.stdout) == {"ok": False, "error": error}, (name, result.stdout[:1000])
    print("PASS " + name, flush=True)

for name, request, error in [
    ("invalid JSON request", b"not-json", "invalid_request"),
    ("missing input", b'{"source":"export function run(){}"}', "invalid_request"),
    ("request bytes limit", b" " * (256*1024+1), "request_limit"),
    ("source bytes limit", json.dumps({"source":" " * (64*1024+1),"input":None}).encode(), "source_limit"),
    ("input bytes limit", json.dumps({"source":"export function run(x){return x}","input":"x" * (64*1024+1)}).encode(), "input_limit"),
]:
    raw_case(name, request, error)
    checks += 1

# A helper blocked on open stdin cannot hit the JS interrupt: exercise the real
# parent kill+wait fallback, without claiming production cancellation is wired.
child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=child_env)
try:
    try:
        child.wait(timeout=0.1)
        raise AssertionError("helper unexpectedly completed with stdin still open")
    except subprocess.TimeoutExpired:
        child.kill()
        child.communicate(timeout=1)
        assert child.returncode is not None and child.returncode != 0
finally:
    if child.poll() is None:
        child.kill()
        child.communicate(timeout=1)
checks += 1
print("PASS parent wall-time kill and wait for stdin-blocked helper", flush=True)

def isolated(index):
    request=json.dumps({"source":"export async function run(x){globalThis.stash=x;await 0;return globalThis.stash;}","input":{"request":index}}).encode()
    result=subprocess.run(command,input=request,capture_output=True,timeout=3,env=child_env)
    assert result.returncode == 0 and json.loads(result.stdout)=={"ok":True,"result":{"request":index}}, (index,result.stdout[:1000])
with ThreadPoolExecutor(max_workers=4) as pool:
    list(pool.map(isolated, range(4)))
checks += 4
print("PASS four concurrent independent request processes", flush=True)
print(f"PASS: {checks} isolated Linux JS probe checks; not a production sandbox")
