# Experimental pure-JSON extension computation helper

This is an **explicitly opt-in Linux foundation**, not a wired plugin dispatcher,
OS sandbox, cross-platform release, or completed M9/E13 feature. Production extension
routes and their fail-closed plan-safe, user-ownership and path gates are unchanged.

## Build and verify

```sh
cargo build -p aionui-extension-js --features engine --example stdio
python3 crates/aionui-extension-js/tests/check_runner.py target/debug/examples/stdio
cargo build -p aionui-app --features experimental-extension-js
python3 crates/aionui-extension-js/tests/check_runner.py target/debug/aioncore --core
python3 crates/aionui-extension-js/tests/check_startup.py target/debug/aioncore --enabled
```

Use the actual target directory when `CARGO_TARGET_DIR` is set. The app's private
`--internal-extension-js` mode must be the only argument. It dispatches before
clap, managed resources, PATH enhancement, database/logging and Tokio startup.
Disabled or non-Linux builds immediately return `runner_unavailable` with exit 2,
without reading stdin. Extra arguments fail similarly. No default app/channel
feature enables the engine. `--all-features` explicitly enables it on Linux.

## Experimental protocol and budgets

One UTF-8 JSON stdin request: `{"source":"export async function run(input) { return input; }","input":null}`.
One stdout response: `{"ok":true,"result":null}` or a fixed error code with `ok:false`.
Execution failures are protocol responses; malformed helper invocation or an
unavailable engine exits 2. Broken output exits 1. No exception getter, message or
stack is read to format errors.

- Request at most 256 KiB; source and encoded input at most 64 KiB each.
- Result JSON at most 64 KiB, checked after serialization/allocation.
- Engine heap 32 MiB and engine stack 256 KiB; these do not bound Rust buffers,
  native compilation, process RSS or native-engine vulnerabilities.
- A cumulative 250 ms interrupt deadline stays installed across module evaluation,
  invocation, Promise work and serialization; at most 10,000 Promise jobs. Engine
  interrupts are periodic callbacks, not preemptive wall-clock enforcement for all
  native/C operations, parsing, allocation or runtime destruction. A parent total
  timeout remains mandatory.
- No imports, including dynamic imports; both resolver and loader deny all.
- SharedArrayBuffer and Atomics are standard intrinsics. The pinned engine defaults
  to nonblocking mode: Atomics.wait must promptly reject; a finite-wait regression
  verifies this rather than relying on the absence of host I/O. Do not enable the
  engine's native can-block setting. This version has no Atomics.waitAsync.
- Standard ECMAScript intrinsics only. No process, require, fetch, files, network,
  environment, timers, threads, FFI, std/os library or host APIs are installed.
- Native Promise resolution is captured before module evaluation; synchronous,
  async and thenable results are supported. A pending Promise with no available
  jobs fails instead of busy waiting. Non-JSON/cyclic output fails.

The experimental acceptance policy follows the **target Promise only**. An
unrelated background rejection does not invalidate its successful result, and
`Ctx::execute_pending_job` does not expose all unrelated job errors. This is not
an assurance that every queued job succeeded. No long-lived JS state is shared.

## Required before production route use

A parent must use its fixed executable/helper mode and bounded source/arguments,
clear nonessential environment, enforce bounded pipes and wall-clock timeout,
kill and wait on timeout/cancellation, and handle parent death. Stdin can block
before JS execution: the JS deadline alone is not sufficient. The Python harness
checks kill/wait for a stdin-blocked helper, not a production Core supervisor.

The authenticated parent, not this helper, must resolve curated/enabled/user-owned
extensions, canonical containment of regular handler files, authoritative plan-safe
policy and any host capability. None of those production integrations is added here.
Never expose this source-taking protocol as an unauthenticated HTTP endpoint.

Tests verify four independent request processes, not authenticated user isolation.
Memory/recursion tests currently prove `run_error` only, not a specific OOM/stack
cause. This layer is capability restriction for curated code, not an OS security
boundary against arbitrary malicious/native exploits. Windows/macOS and all
release architectures still require separate support and acceptance work.

## Native dependency scope

The optional, Linux-only `rquickjs` 0.14.0 dependency uses only its loader feature;
no allocator/rust-alloc/dyn-load/futures feature is requested. Memory-limit behavior
requires keeping custom allocator features disabled if dependency features change.
rquickjs and its QuickJS-NG implementation carry MIT licenses; preserve their
upstream [license notices](THIRD-PARTY-NOTICES.md) when distributing linked experimental builds. Default
builds do not compile or link the engine; its optional packages still appear in
Cargo.lock. No workflow or release configuration is changed.

## Internal parent-client increment

The app library now contains `extension_js_client::invoke`, compiled to an
immediate `Unavailable` response unless the app's Linux experimental feature is
enabled. Its production executable is always `current_exe()` with the sole
private helper flag; no executable, argv, environment, cwd, user or handler path
is accepted from the caller. This is **not wired to extension routes**.

The internal client sets a two-second work-stage deadline from call entry, concurrent
bounded stdin/stdout/stderr work (256 KiB protocol stdout, 8 KiB stderr), and an
independent child owner. Explicit cancellation, closing the cancellation channel,
or dropping the calling future asks that owner to kill and wait. All pipe
futures close before cleanup; group-kill failures still attempt direct kill/wait.
Input additionally has depth 64 and 32768-node limits and bounded serialization.
A single strict JSON response is required; success includes an explicitly present
`result` (including null), with a separately bounded 64 KiB serialized result.

These guarantees require a live Tokio runtime. Parent death, runtime shutdown,
kernel-uninterruptible waits and native interpreter vulnerabilities are **not**
covered. Cleanup deliberately awaits reaping without an independent deadline; the
two-second work budget is not an unconditional bound on total return time.
There is no process-parent-death signal or source-file loader in this increment. Authorization, curated provenance, immutable source acquisition and
production wiring remain separate work. The public fixed-current-exe entry
requires a host with Core's private helper flag. A separate test-only re-exec host exercises that public entry and the real
JS helper implementation, without adding production fixture modes or an
executable override; this host is not the packaged Core application.
Manual ignored process tests separately use a test-only `AIONCORE_TEST_BINARY` to exercise
the same owner against the actual built Core; they do not override the public
production executable selection. Unreachable fault injection (blocked writer,
oversized helper stderr, runtime teardown) is not claimed as process-tested.

Run the public entry test with:

```sh
cargo test -p aionui-app --no-default-features --features experimental-extension-js --test extension_js_client_process
```

To run the ignored owner tests, first build and retain separate enabled/disabled
Core binaries, then point the two **test-only** variables at those snapshots:

```sh
AIONCORE_TEST_BINARY=/path/to/enabled/aioncore \
AIONCORE_TEST_DEFAULT_BINARY=/path/to/disabled/aioncore \
cargo test -p aionui-app --lib --no-default-features --features experimental-extension-js extension_js_client:: -- --include-ignored --test-threads=1
```
