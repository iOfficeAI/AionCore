//! Shared `opencode serve` process pool (PR: single-process opencode mode).
//!
//! Motivation: the legacy path spawns one `opencode acp` process PER
//! conversation, and each of those boots its own embedded HTTP server. N
//! concurrent opencode conversations therefore cost N bun processes (hundreds
//! of MB each). The `opencode serve` HTTP/SSE API, in contrast, is designed
//! to host an arbitrary number of sessions across arbitrary working
//! directories concurrently (one `location.directory` per session), so ALL
//! opencode conversations in one AionUi instance attach to ONE shared server.
//!
//! Lifecycle (mirrors what OpenChamber's VSCode extension does):
//! 1. Discovery — a registry file under the OS temp dir remembers the last
//!    spawned server (`port`/`password`/`pid`/`program`). On first use after an
//!    AionUi restart we probe it and adopt it if it answers `/api/health`.
//! 2. Adopt-foreign — if any server already answers `/api/health` on the
//!    registry port without auth, it is adopted READ-ONLY (never killed).
//! 3. Spawn-if-absent — bind port 0 to let the OS pick a free port (an
//!    explicit `--port` also defeats any user-pinned `server.port` in
//!    `~/.config/opencode/opencode.json`), spawn
//!    `opencode serve --hostname 127.0.0.1 --port N` with a self-generated
//!    `OPENCODE_SERVER_PASSWORD`, poll `/api/health` until ready.
//! 4. Reference counting — every attached conversation holds a
//!    [`ServerLease`]; the last lease dropping tears down OUR server (never a
//!    foreign one). A background watchdog marks the entry dead when the health
//!    probe fails repeatedly so attached sessions learn about a crash via
//!    their `watch` receiver and emit `Detached`.
//!
//! Known limitation (documented in the PR): the shared process cannot carry
//! per-conversation `AIONUI_CONVERSATION_ID` env — it is stripped from the
//! spawn env so helper-CLI features fail loudly instead of misattributing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, watch};

/// A resident shared server. Cheap to clone; all fields describe one
/// `opencode serve` process the pool either owns or adopted.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub base_url: String,
    /// Basic-auth password we generated (own servers), or `None` for a
    /// foreign server that serves unauthenticated.
    pub password: Option<String>,
    pub pid: Option<u32>,
    /// True when the server was NOT spawned by this pool (never killed).
    pub external: bool,
}

/// What the pool mutates per registry key.
struct Entry {
    info: ServerInfo,
    refs: usize,
    alive_tx: watch::Sender<bool>,
    /// Abort handle for the watchdog task (so teardown can stop it).
    watchdog: tokio::task::JoinHandle<()>,
}

/// Registry key. One shared server per program binary — in practice a single
/// entry; keyed so two different opencode builds don't fight over one slot.
type PoolKey = String;

#[derive(Default)]
struct PoolState {
    entries: HashMap<PoolKey, Entry>,
}

/// Process-wide pool. AionUi runs one aioncore, so global state is fine;
/// cross-process sharing is handled via the registry file + health probe.
static POOL: std::sync::OnceLock<Arc<ServerPool>> = std::sync::OnceLock::new();

pub fn global_pool() -> Arc<ServerPool> {
    POOL.get_or_init(|| Arc::new(ServerPool::default())).clone()
}

#[derive(Default)]
pub struct ServerPool {
    state: Mutex<PoolState>,
}

/// RAII lease: one attached conversation. Cloning a lease would double-count,
/// so callers share `Arc<ServerLease>` instead.
pub struct ServerLease {
    pub info: ServerInfo,
    pool: Arc<ServerPool>,
    key: PoolKey,
}

impl Clone for ServerLease {
    fn clone(&self) -> Self {
        // Only ever reached through Arc; still keep accounting honest.
        self.pool.clone_lease(&self.key);
        Self {
            info: self.info.clone(),
            pool: self.pool.clone(),
            key: self.key.clone(),
        }
    }
}

impl Drop for ServerLease {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let key = self.key.clone();
        // Release is async (pool mutex) and MAY end in killing the server, so
        // it must survive the caller's runtime going away: a lease dropped
        // during runtime shutdown (test teardown, task aborts) would silently
        // lose an `Handle::spawn` and leak the process. A detached OS thread
        // with a private current-thread runtime makes the last-lease teardown
        // unconditional. (No lock is held across Drop; this is fire-and-forget
        // and a lost decrement would still self-heal via the watchdog.)
        std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                rt.block_on(pool.release(&key));
            }
        });
    }
}

/// Registry file location: `%TEMP%/aionui-opencode-shared/<hash>.json`.
fn registry_path(program: &Path) -> PathBuf {
    let digest = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        program.to_string_lossy().to_lowercase().hash(&mut h);
        format!("{:016x}", h.finish())
    };
    std::env::temp_dir()
        .join("aionui-opencode-shared")
        .join(format!("{digest}.json"))
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RegistryRecord {
    port: u16,
    password: String,
    pid: u32,
    program: String,
}

impl ServerPool {
    /// Current live server info for `program` if the pool already hosts one
    /// (same key as `acquire`: the lowercased program path). Used by the
    /// shared-server tests to PROVE the one-process claim — two sessions must
    /// observe the same base_url; handy for diagnostics too.
    pub async fn peek(self: &Arc<Self>, program: &Path) -> Option<ServerInfo> {
        let key = program.to_string_lossy().to_lowercase();
        let state = self.state.lock().await;
        state.entries.get(&key).map(|e| e.info.clone())
    }

    /// Acquire (adopt-or-spawn) the shared server for `program` and hold a
    /// lease for it. `spawn_env` entries are EXTRA environment on top of the
    /// inherited env; `AIONUI_CONVERSATION_ID` is always stripped (a shared
    /// process must not impersonate one conversation).
    pub async fn acquire(self: &Arc<Self>, program: &Path, spawn_env: &[(String, String)]) -> Result<Acquired, String> {
        let key = program.to_string_lossy().to_lowercase();
        loop {
            let action = {
                let mut st = self.state.lock().await;
                match st.entries.get_mut(&key) {
                    Some(e) if e.info.external => {
                        // Re-validating an adopted foreign server on every
                        // acquire is cheap; drop it if it stopped answering.
                        if health_ok(&e.info).await {
                            e.refs += 1;
                            Some(Ok(mk_acquire(e, self, &key)))
                        } else {
                            let old = st.entries.remove(&key);
                            if let Some(old) = old {
                                old.watchdog.abort();
                            }
                            None
                        }
                    }
                    Some(e) => {
                        // Own server: trust liveness unless the watchdog said
                        // dead; then reap and fall through to re-discover.
                        if *e.alive_tx.borrow() && health_ok(&e.info).await {
                            e.refs += 1;
                            Some(Ok(mk_acquire(e, self, &key)))
                        } else {
                            let old = st.entries.remove(&key);
                            if let Some(old) = old {
                                old.watchdog.abort();
                            }
                            None
                        }
                    }
                    None => None,
                }
            };
            if let Some(result) = action {
                return result;
            }
            // Not resident: discovery → adopt → spawn, all under the lock so
            // racing callers queue instead of double-spawning.
            let mut st = self.state.lock().await;
            if st.entries.contains_key(&key) {
                continue; // someone else just installed it while we unlocked
            }
            let info = discover_or_spawn(program, spawn_env).await?;
            let (alive_tx, _rx) = watch::channel(true);
            let watchdog = spawn_watchdog(info.clone(), alive_tx.clone());
            st.entries.insert(
                key.clone(),
                Entry {
                    info: info.clone(),
                    refs: 1,
                    alive_tx,
                    watchdog,
                },
            );
            let e = st.entries.get_mut(&key).expect("just inserted");
            return Ok(mk_acquire(e, self, &key));
        }
    }

    fn clone_lease(self: &Arc<Self>, key: &PoolKey) {
        let pool = self.clone();
        let key = key.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Some(e) = pool.state.lock().await.entries.get_mut(&key) {
                    e.refs += 1;
                }
            });
        }
    }

    async fn release(self: &Arc<Self>, key: &PoolKey) {
        let victim = {
            let mut st = self.state.lock().await;
            let Some(e) = st.entries.get_mut(key) else { return };
            e.refs = e.refs.saturating_sub(1);
            if e.refs == 0 && !e.info.external {
                st.entries.remove(key)
            } else {
                None
            }
        };
        if let Some(e) = victim {
            e.watchdog.abort();
            let _ = e.alive_tx.send(false);
            if let Some(pid) = e.info.pid {
                kill_pid(pid);
            }
            let _ = std::fs::remove_file(registry_path(Path::new(key)));
            tracing::info!(
                port = e.info.base_url,
                "opencode shared server stopped (last lease dropped)"
            );
        }
    }
}

/// What a successful acquire hands to the backend: the lease plus a liveness
/// watch (transitions to `false` when the server dies while attached — the
/// backend turns that into `SessionEvent::Detached`).
pub struct Acquired {
    pub lease: Arc<ServerLease>,
    pub alive: watch::Receiver<bool>,
}

fn mk_acquire(e: &mut Entry, pool: &Arc<ServerPool>, key: &PoolKey) -> Acquired {
    let _ = e.refs; // already incremented by caller
    Acquired {
        lease: Arc::new(ServerLease {
            info: e.info.clone(),
            pool: pool.clone(),
            key: key.clone(),
        }),
        alive: e.alive_tx.subscribe(),
    }
}

/// Try the registry, then a silent adopt of any already-running server, then
/// spawn our own. Never touches a server whose password we do not know.
async fn discover_or_spawn(program: &Path, spawn_env: &[(String, String)]) -> Result<ServerInfo, String> {
    let reg = registry_path(program);
    // 1) Our own previous server (registry carries the password we gave it).
    let mut hint_port: Option<u16> = None;
    if let Ok(text) = std::fs::read_to_string(&reg)
        && let Ok(rec) = serde_json::from_str::<RegistryRecord>(&text)
    {
        // A parseable record is stale unless it matches our program AND the
        // health probe passes — anything else is removed before spawn.
        if rec.program.to_lowercase() == program.to_string_lossy().to_lowercase() {
            hint_port = Some(rec.port);
            let info = ServerInfo {
                base_url: format!("http://127.0.0.1:{}", rec.port),
                password: Some(rec.password.clone()),
                pid: Some(rec.pid),
                external: false,
            };
            if health_ok(&info).await {
                tracing::info!(port = rec.port, "opencode shared server adopted from registry");
                return Ok(info);
            }
        }
        let _ = std::fs::remove_file(&reg);
    }
    // 2) A server whose password we don't know — only adopt if it serves
    //    UNAUTHENTICATED (user-launched `opencode serve` without a password):
    //    the registry port (another AionUi may have taken the slot) and the
    //    opencode default 4096. Adopted foreign servers are never killed.
    let mut ports: Vec<u16> = Vec::new();
    if let Some(p) = hint_port {
        ports.push(p);
    }
    ports.push(4096);
    for port in ports {
        let info = ServerInfo {
            base_url: format!("http://127.0.0.1:{port}"),
            password: None,
            pid: None,
            external: true,
        };
        if health_ok(&info).await {
            tracing::info!(port, "foreign auth-free opencode server adopted (read-only)");
            return Ok(info);
        }
    }
    // 3) Cold start: spawn our own, authenticated.
    spawn_server(program, spawn_env, &reg).await
}

async fn spawn_server(program: &Path, spawn_env: &[(String, String)], reg: &Path) -> Result<ServerInfo, String> {
    let port = reserve_free_port().await?;
    let password = generate_password();
    // npm-installed opencode ships a `.cmd` shim on Windows, which CreateProcess
    // cannot launch directly. Same plan as `aionui-runtime::spawn`'s private
    // `shell_wrap_windows_script`: `cmd /d /c <program> [args…]`. The pid recorded
    // below is then cmd.exe's — `taskkill /T /F` kills the whole tree, and
    // liveness is proven by the health probe, not the pid.
    let mut pre_args: Vec<std::ffi::OsString> = Vec::new();
    #[cfg(windows)]
    {
        let ext = program.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat") {
            pre_args.push("/d".into());
            pre_args.push("/c".into());
            pre_args.push(program.as_os_str().to_owned());
        }
    }
    let spawn_program: &std::ffi::OsStr = {
        #[cfg(windows)]
        {
            let ext = program.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat") {
                std::ffi::OsStr::new("cmd")
            } else {
                program.as_os_str()
            }
        }
        #[cfg(not(windows))]
        {
            program.as_os_str()
        }
    };
    let mut cmd = tokio::process::Command::new(spawn_program);
    cmd.args(&pre_args)
        .args(["serve", "--hostname", "127.0.0.1", "--port", &port.to_string()])
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (k, v) in spawn_env {
        if k == "AIONUI_CONVERSATION_ID" || k == "OPENCODE_SERVER_PASSWORD" {
            continue; // shared process: no per-conversation identity, own auth
        }
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        // tokio::process::Command exposes creation_flags as an inherent
        // method on Windows — no CommandExt trait import needed.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("spawn `{} serve`: {e}", program.display()))?;
    let pid = child.id();
    let info = ServerInfo {
        base_url: format!("http://127.0.0.1:{port}"),
        password: Some(password.clone()),
        pid,
        external: false,
    };
    // Readiness: poll /api/health up to ~90s (bun cold start can be slow on
    // Windows AV scanners, and bundled/distribution exe wrappers are slower
    // still). Timeouts here surface as a startup failure the same way the ACP
    // handshake timeout would.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if health_ok(&info).await {
            break;
        }
        if std::time::Instant::now() > deadline {
            if let Some(pid) = pid {
                kill_pid(pid);
            }
            return Err(format!(
                "opencode serve on port {port} did not become healthy within 90s \
                 (is `{}` a `serve`-capable opencode ≥ 1.18?)",
                program.display()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    // Persist BEFORE handing out the lease: a crash right after must still
    // find (and clean) the record. Detach the child so dropping the Command
    // does not kill it (kill_on_drop defaults to false).
    let rec = RegistryRecord {
        port,
        password,
        pid: pid.unwrap_or(0),
        program: program.to_string_lossy().into_owned(),
    };
    if let Some(dir) = reg.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(reg, serde_json::to_string(&rec).unwrap_or_default());
    tracing::info!(port, ?pid, "opencode shared server ready");
    Ok(info)
}

async fn reserve_free_port() -> Result<u16, String> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("reserve free port: {e}"))?;
    let port = l.local_addr().map_err(|e| format!("local addr: {e}"))?.port();
    drop(l); // release so `serve --port` can bind it
    Ok(port)
}

fn generate_password() -> String {
    // getrandom is a direct dep of this crate (used by the ACP auth tokens).
    let mut a = [0u8; 16];
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut a);
    let _ = getrandom::getrandom(&mut b);
    format!("{}{}", hex(&a), hex(&b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn auth_header(info: &ServerInfo) -> Option<String> {
    info.password.as_ref().map(|pw| {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{pw}"))
        )
    })
}

async fn health_ok(info: &ServerInfo) -> bool {
    health_ok_with(&shared_client(), info).await
}

async fn health_ok_with(client: &reqwest::Client, info: &ServerInfo) -> bool {
    let mut req = client.get(format!("{}/api/health", info.base_url));
    if let Some(auth) = auth_header(info) {
        req = req.header(reqwest::header::AUTHORIZATION, auth);
    }
    match req.send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Shared HTTP client (connection pooling across all sessions). Long-ish
/// timeouts are per-request at the call sites; SSE uses its own no-timeout
/// client inside the backend pump.
pub fn shared_client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client")
        })
        .clone()
}

static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn spawn_watchdog(info: ServerInfo, alive_tx: watch::Sender<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut misses = 0u32;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            if health_ok(&info).await {
                misses = 0;
                continue;
            }
            misses += 1;
            if misses >= 2 {
                tracing::warn!(base = %info.base_url, "opencode shared server lost (health failed)");
                let _ = alive_tx.send(false);
                return;
            }
        }
    })
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
