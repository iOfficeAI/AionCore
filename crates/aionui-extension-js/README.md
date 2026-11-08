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

The owner's explicit wait guarantees require a live Tokio runtime. The separate
Linux early parent-death guard is described below; complete runtime shutdown,
kernel-uninterruptible waits and native interpreter vulnerabilities are **not**
covered. Cleanup deliberately awaits reaping without an independent deadline; the
two-second work budget is not an unconditional bound on total return time.
The initial client increment did not include parent-death protection or source-file reading;
the independent additions below do not establish authorization. Curated provenance, immutable source acquisition and
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

## Linux Core early parent-death guard

The experimental Core private entry now requires host-owned
`WORKMATE_EXTENSION_JS_PARENT_PID` metadata. The fixed-executable client sets it
itself after clearing the environment; it is not a plugin option or authorization
token. Before stdin/JS, the helper validates its parent PID, installs Linux
`PR_SET_PDEATHSIG(SIGKILL)`, then rechecks the expected parent. Missing/invalid
metadata, a parent mismatch or a failed kernel operation refuses execution.
The standalone stdio example remains an unsupervised manual experiment; it does
not supply this Core process-lifecycle guard. Core probe scripts supply their
own parent PID explicitly.

This protects this direct helper process, not descendants or the hosting
application's complete runtime-shutdown lifecycle. Linux ties the signal to the
creating **thread**: that thread's exit may terminate the helper even while other
threads in the parent process remain alive. This is not an OS sandbox or a
process-only availability guarantee. The PID checks cover parent-process
replacement before/during installation; they do not prove every thread-death or
PID-namespace interleaving. Bounded pipes, cancellation and explicit wait remain
necessary. A dead parent cannot reap its child; normal OS reparenting/reaping
still applies.

The manual parent-death probe keeps a separate stdin writer open while killing
the intermediary parent, preventing EOF from falsely appearing to prove this
guard. The probe makes only its own Python process a Linux child subreaper so it
can adopt and wait for these test helpers. It adds no Core fixture mode.

Run the actual Core parent-death check after building the opt-in Linux Core:

```sh
python3 crates/aionui-extension-js/tests/check_parent_death.py /path/to/enabled/aioncore
```

## Independent experimental source-byte reader

The app library exports `extension_js_source::read_bounded_source(&File, &Path)`.
It is independent of the client/dispatcher: no route invokes it, no handler is
executed, and the existing canonical resolver is unchanged. Default/non-Linux
builds immediately return `Unavailable`. The host, not this reader, must acquire
and authorize the already-open root-directory `File`.

On opt-in Linux, lexical checks reject empty, absolute, parent-traversing,
NUL-containing and colon-containing (including URL-like) paths. `openat2` pins a
relative inode with `O_PATH|O_CLOEXEC`, `RESOLVE_BENEATH|NO_SYMLINKS|NO_MAGICLINKS`
and the deliberately tighter `NO_XDEV` (including bind/submount crossings).
`fstat` must identify a regular file **before** any read-open: even a nonblocking
read-open could activate a device. The pinned fd stays alive while a fixed
host `/proc/self/fd` directory is opened and checked for procfs filesystem type.
This requires a **trusted host `/proc` mount and mount namespace with no malicious
mount substitution**, exposing this process's fd view. `fstatfs(PROC_SUPER_MAGIC)`
proves only that the directory belongs to procfs, not that it is our current
process's fd view. `O_NOFOLLOW` protects only the final path component. A
privileged host could substitute another real procfs fd directory; reopening its
numeric entry could activate a wrong target before the later identity comparison.
This privileged mount-control threat is outside an ordinary handler-directory
writer's scope; root trust acquisition also remains the host's responsibility.
Under that host precondition, only the host-generated decimal fd magiclink is
followed, never a caller-provided proc path. The read fd is rechecked as regular with matching device/inode, then that
same fd supplies at most 65536 bytes plus one overflow-detection byte. Oversized
or non-UTF-8 source is rejected. All errors have fixed non-sensitive categories.
Missing/unsupported/denied `openat2`, unavailable proc access, a directory whose
filesystem type is not procfs, and other detected failures are closed; there is
no canonicalize/original-path fallback. This does not detect or reject every
untrusted procfs substitution or establish ownership of a procfs fd view.

This is synchronous finite-**bytes** reading, **not** a guaranteed I/O wall-clock
bound. Filesystems can stall; content can change in place. The held inode/root
checks protect path replacement, not immutable contents or hardlink provenance.
No actual side-effecting device is opened in tests. Tests cover regular/nested
files, byte/UTF-8 boundaries, invalid lexical paths, symlinks, directories/FIFO,
and deterministic atomic replacement after pinning and replacement of the root
pathname. Unavailable-openat2 tests only exercise errno mapping, not injected
syscall failures; proc-unavailable/non-procfs fault injection is not tested.
Privileged mount substitution is not mitigated or tested by this increment.
No process-global seccomp, environment switch or production test hook is added.

Registry `LoadedExtension` contains manifest, directory, `Local|Appdata|Env`
source classification and state; that is not a curated handler-byte attestation.
The per-user accessor overlays enabled state on a global installation registry;
it does not establish installation owner isolation. Curated-install integrity
binds the manifest only, not handler bytes. A successful read **never** authorizes
execution or proves curation, user ownership or immutable content. Hardlinks,
native-engine vulnerabilities, OS sandboxing, runtime shutdown and HTTP/user/
plan-safe integration gaps remain. The parent PID/creating-thread semantics
above are unchanged. This increment does not complete M9/E13.

Focused checks:

```sh
cargo test -p aionui-app --lib --no-default-features --features experimental-extension-js extension_js_source::
cargo test -p aionui-app --lib extension_js_source::
```

## Conditional integrity of one source snapshot

The independent app API
`extension_js_checked_source::read_checked_source(&File, &Path, expected_sri)`
returns `IntegrityCheckedSource` only if the source snapshot matches a given
expected digest. This is **conditional integrity**, not a trusted-digest lookup,
execution permit, curation assertion, installation receipt or owner/root proof.
The host must independently acquire the correct trusted digest-to-handler
binding, trustworthy already-open root and trusted current-process `/proc` view.
An attacker supplying both source and its own hash can satisfy equality; that
establishes no external trust. Registry labels and signature keys are not
execution authorization. There is currently **no handler digest field in the
signed manifest contract**, so this API does not establish signed-manifest to
handler-byte binding. It reads no unknown JSON field or new receipt format.

Default/non-Linux builds return `Unavailable` immediately regardless of inputs.
Enabled Linux checks expected-hash format **before file IO**: exactly 95 bytes,
lowercase `sha512-`, standard base64 alphabet, 86 digest characters followed by
`==`, and zero canonical padding bits in the final data sextet (A/Q/g/w). No
trimming, algorithm aliases, URL-safe alphabet or case folding is accepted.
Format validation uses bounded byte checks without allocating or decoding the
caller-supplied hash. It then calls existing `read_bounded_source` **exactly
once**, computes existing public `sri_sha512` over that owned UTF-8 String's exact
bytes, and compares case-sensitively. A format-valid different digest is an
`IntegrityMismatch`; malformed metadata is `InvalidIntegrity`; existing reader
failures retain their original fixed category inside `Reader(SourceError)`.
No source, path or digest values are added to errors/logs.

The successful type has private owned String storage, borrowing `as_str()` and
consuming `into_source()` getters, with no unchecked constructor, Default,
Deserialize or raw-source Debug formatter. A future separately authorized caller
can transfer `into_source()` to the existing client's `invoke`; nothing invokes
it automatically here. Consumers must use the returned snapshot, not reread the
path. UTF-8 bytes including BOM/CRLF remain exact. Changing/replacing the file
later cannot change this snapshot; this does not prove that a file was immutable
while being read, or establish hardlink provenance. An empty source may match an
empty-source digest without being an executable handler.

All reader assumptions and checks remain unchanged: 65536-byte UTF-8 limit,
O_PATH/REG-before-read-open, symlink/mount-crossing rejection, fixed numeric fd
reopen and REG/dev/ino check, trusted Host `/proc` mount with no malicious mount
substitution. Procfs filesystem type alone does **not** authenticate the process
fd view; a privileged mount substitution could activate a wrong target before
identity comparison. This is finite-**bytes** synchronous reading, not an I/O
wall-clock bound. No new syscall/proc/mount fault injection or real side-effect
device opening is claimed. Host authorization, curation, user isolation, handler
provenance, OS sandboxing, native risks, runtime and HTTP/plan-safe gaps remain.
No resolver, installer, gate, registry, route, permission or manifest fields are
changed. This increment does not complete M9/E13.

Focused tests cover exact UTF-8/CRLF/BOM and snapshot transfer; malformed digest
format/length/alphabet/padding (including all final-sextet possibilities), content
tampering and digest case mismatch, empty/64 KiB/+1/invalid-UTF-8 source, owned
snapshots consumed after both replacement and in-place writes, reader error
preservation and symlink/FIFO rejection. Invalid hashes paired with missing/FIFO
handlers or a wrong-kind root demonstrate format-error precedence, not syscall
fault injection. Disabled tests check immediate Unavailable. Non-Linux support
is cfg-defined but is not claimed cross-compiled by Linux checks.

```sh
cargo test -p aionui-app --lib --no-default-features --features experimental-extension-js extension_js_checked_source::
cargo test -p aionui-app --lib extension_js_checked_source::
```
