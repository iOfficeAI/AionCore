//! Dedicated test-only re-exec host. No extra mode is added to Core's CLI.
use aionui_app::extension_js_client::{HelperError, invoke};
use serde_json::Value;
#[cfg(target_os = "linux")]
use serde_json::json;
use std::ffi::OsStr;
use std::process::ExitCode;
use tokio::sync::oneshot;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(OsStr::new("--internal-extension-js")) {
        if args.next().is_some() {
            return ExitCode::from(2);
        }
        return aionui_extension_js::run_stdio();
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check());
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "linux"))]
async fn check() {
    let (_keep, cancel) = oneshot::channel();
    assert_eq!(
        invoke(String::new(), Value::Null, cancel).await,
        Err(HelperError::Unavailable)
    );
}

#[cfg(target_os = "linux")]
async fn check() {
    use std::time::Duration;
    fn children() -> Vec<u32> {
        std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))
            .unwrap()
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect()
    }
    async fn gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("public client's child must be terminated and reaped");
    }
    let (_keep_a, cancel_a) = oneshot::channel();
    let (_keep_b, cancel_b) = oneshot::channel();
    let answers = tokio::join!(
        invoke(
            "export async function run(i){await Promise.resolve();return {input:i}}".into(),
            json!(42),
            cancel_a
        ),
        invoke("export function run(){return null}".into(), Value::Null, cancel_b),
    );
    assert_eq!(answers, (Ok(json!({"input":42})), Ok(Value::Null)));
    assert!(children().is_empty());

    // Unlike the owner-only tests, these calls enter the PUBLIC API and use its
    // fixed current_exe path. The test host's sole helper flag runs the real JS.
    for abandon in [false, true] {
        let (sender, cancel) = oneshot::channel();
        let calling = tokio::spawn(invoke(
            "export function run(){while(true){}}".into(),
            Value::Null,
            cancel,
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let pids = children();
        assert_eq!(
            pids.len(),
            1,
            "must observe the live self-exec child before cancellation"
        );
        if abandon {
            calling.abort();
            assert!(calling.await.unwrap_err().is_cancelled());
        } else {
            sender.send(()).unwrap();
            assert_eq!(calling.await.unwrap(), Err(HelperError::Cancelled));
        }
        gone(pids[0]).await;
        assert!(children().is_empty());
    }
    println!("PASS: public fixed-current-exe async/null/concurrency/cancel/drop and PID reaping (test-only host)");
}
