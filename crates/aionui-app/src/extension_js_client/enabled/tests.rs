use super::*;
use serde_json::json;

#[test]
fn bounded_request_and_strict_response() {
    assert!(request(&"a".repeat(JSON_MAX + 1), &Value::Null).is_err());
    assert!(request("export function run(){}", &json!("x".repeat(JSON_MAX))).is_err());
    assert!(request(&"\x01".repeat(JSON_MAX), &Value::Null).is_err());
    let mut nested = Value::Null;
    for _ in 0..66 {
        nested = json!([nested]);
    }
    assert!(request("", &nested).is_err());
    assert_eq!(decode(br#"{"ok":true,"result":null}"#, true, Some(0)), Ok(Value::Null));
    assert_eq!(
        decode(br#"{"ok":false,"error":"deadline"}"#, true, Some(0)),
        Err(HelperError::Execution("deadline".into()))
    );
    assert_eq!(
        decode(br#"{"ok":false,"error":"runner_unavailable"}"#, false, Some(2)),
        Err(HelperError::Unavailable)
    );
    for error in ["supervision_required", "parent_exited", "parent_guard_unavailable"] {
        let frame = serde_json::to_vec(&json!({"ok":false,"error":error})).unwrap();
        assert_eq!(decode(&frame, false, Some(2)), Err(HelperError::ParentGuard));
    }
    for response in [
        br#"{"ok":true}"#.as_slice(),
        br#"{"ok":false,"result":1}"#,
        br#"{"ok":true,"ok":true,"result":1}"#,
        br#"{"ok":true,"result":1,"extra":1}"#,
        br#"{"ok":true,"result":1} {}"#,
        br#"{"ok":false,"error":"<arbitrary error>"}"#,
    ] {
        assert_eq!(
            decode(response, true, Some(0)),
            Err(HelperError::Protocol),
            "{response:?}"
        );
    }
    assert_eq!(
        decode(br#"{"ok":true,"result":1}"#, false, Some(1)),
        Err(HelperError::ChildExit)
    );
}

#[tokio::test]
async fn bounded_pipe_readers() {
    assert_eq!(read_bounded(&b"abcd"[..], 4).await.unwrap(), b"abcd");
    assert_eq!(read_bounded(&b"abcde"[..], 4).await, Err(HelperError::OutputLimit));
}

#[tokio::test]
async fn already_cancelled_never_spawns() {
    let (sender, cancel) = oneshot::channel();
    drop(sender);
    assert_eq!(
        invoke("export function run(){return 1}".into(), Value::Null, cancel).await,
        Err(HelperError::Cancelled)
    );
}

// These tests spawn ONLY the actual previously built Core helper. The test-only
// path is not an override of the public fixed-current-exe client API.
fn core_child() -> Child {
    core_child_from_env("AIONCORE_TEST_BINARY")
}
fn core_child_from_env(variable: &str) -> Child {
    let exe = std::env::var_os(variable).expect("set the test-only variable to the actual Core binary");
    let mut command = Builder::clean_cli(exe);
    command
        .env_clear()
        .env(PARENT_PID_ENV, std::process::id().to_string())
        .arg("--internal-extension-js")
        .stdin(Stdio::piped());
    command.spawn().unwrap()
}
async fn gone(pid: u32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("child must be dead AND reaped, not a zombie");
}
async fn call(source: &str, input: Value) -> Result<Value, HelperError> {
    let child = core_child();
    let pid = child.id().unwrap();
    let (_keep, cancel) = oneshot::channel();
    let result = wait_owned(
        child,
        request(source, &input).unwrap(),
        cancel,
        Instant::now() + CALL_BUDGET,
    )
    .await;
    gone(pid).await;
    result
}

#[tokio::test]
#[ignore = "requires AIONCORE_TEST_BINARY built with experimental-extension-js"]
async fn actual_core_json_null_failure_and_concurrency() {
    assert_eq!(
        call(
            "export async function run(i){await Promise.resolve();return {value:i}}",
            json!(37)
        )
        .await,
        Ok(json!({"value":37}))
    );
    assert_eq!(
        call("export function run(){return null}", Value::Null).await,
        Ok(Value::Null)
    );
    assert_eq!(
        call("export function run(){throw 'no'}", Value::Null).await,
        Err(HelperError::Execution("run_error".into()))
    );
    let results = tokio::join!(
        call("export function run(i){return i}", json!("one")),
        call("export async function run(i){return i}", json!("two")),
    );
    assert_eq!(results, (Ok(json!("one")), Ok(json!("two"))));
}

#[tokio::test]
#[ignore = "requires AIONCORE_TEST_BINARY built with experimental-extension-js"]
async fn actual_core_cancel_and_drop_reap() {
    for drop_caller in [false, true] {
        let child = core_child();
        let pid = child.id().unwrap();
        let (sender, cancel) = oneshot::channel();
        let call = tokio::spawn(wait_owned(
            child,
            request("export function run(){while(true){}}", &Value::Null).unwrap(),
            cancel,
            Instant::now() + CALL_BUDGET,
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        if drop_caller {
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
        } else {
            sender.send(()).unwrap();
            assert_eq!(call.await.unwrap(), Err(HelperError::Cancelled));
        }
        gone(pid).await;
    }
}

#[tokio::test]
#[ignore = "requires AIONCORE_TEST_BINARY built with experimental-extension-js"]
async fn actual_core_wall_timeout_reaps() {
    let child = core_child();
    let pid = child.id().unwrap();
    let (_keep, cancel) = oneshot::channel();
    let start = Instant::now();
    assert_eq!(
        wait_owned(
            child,
            request("export function run(){while(true){}}", &Value::Null).unwrap(),
            cancel,
            start + Duration::from_millis(30)
        )
        .await,
        Err(HelperError::Timeout)
    );
    gone(pid).await;
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
#[ignore = "requires AIONCORE_TEST_BINARY built with experimental-extension-js"]
async fn actual_core_missing_pipe_failure_reaps() {
    let child = core_child();
    let pid = child.id().unwrap();
    let (_keep, cancel) = oneshot::channel();
    let (_alive, abandoned) = oneshot::channel();
    // Test a missing pipe as a structural I/O failure, not as a blocked writer.
    let mut child = child;
    let stdin = child.stdin.take().unwrap();
    let result = supervise(
        child,
        vec![],
        cancel,
        abandoned,
        Instant::now() + Duration::from_millis(30),
    )
    .await;
    drop(stdin);
    // Missing-pipe validation itself returns Io, so this is not a blocked writer
    // test. Leave unreachable fault injection out rather than fake its evidence.
    assert_eq!(result, Err(HelperError::Io));
    gone(pid).await;
}

#[tokio::test]
#[ignore = "requires AIONCORE_TEST_DEFAULT_BINARY built without experimental-extension-js"]
async fn actual_default_core_unavailable_and_early_stdin_close_reaps() {
    // Larger than the usual pipe capacity: the disabled Core exits without
    // reading stdin. BrokenPipe must not race away its Unavailable response.
    let source = format!("/*{}*/export function run(){{return 1}}", "a".repeat(60 * 1024));
    let request = request(&source, &Value::Null).unwrap();
    for _ in 0..8 {
        let child = core_child_from_env("AIONCORE_TEST_DEFAULT_BINARY");
        let pid = child.id().unwrap();
        let (_keep, cancel) = oneshot::channel();
        assert_eq!(
            wait_owned(child, request.clone(), cancel, Instant::now() + CALL_BUDGET).await,
            Err(HelperError::Unavailable)
        );
        gone(pid).await;
    }
}
