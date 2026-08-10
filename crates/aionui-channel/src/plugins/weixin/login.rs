use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::error::ChannelError;

use super::api::WeixinApi;
use super::types::{QrCodeData, QrCodeStatusData, SseDoneEvent, SseErrorEvent, SseQrEvent};

/// Default base URL for the iLink Bot login API.
const LOGIN_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

/// Polling interval for checking QR code scan status.
const QR_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Maximum time to wait for QR code scan before timeout.
const QR_LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Minimum delay between QR login starts for one AionUI owner.
const QR_LOGIN_MIN_START_INTERVAL: Duration = Duration::from_secs(10);

/// Why a new per-owner QR login could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeixinLoginStartError {
    /// The same owner already has a live QR login task.
    InProgress,
    /// The previous task ended too recently to start another external flow.
    RateLimited { retry_after: Duration },
}

#[derive(Debug)]
struct LoginSlot {
    active_generation: Option<u64>,
    generation: u64,
    last_started: Instant,
}

/// Coordinates QR login tasks so one owner cannot accumulate external polling
/// loops by opening multiple SSE connections.
#[derive(Debug)]
pub struct WeixinLoginCoordinator {
    slots: Mutex<HashMap<String, LoginSlot>>,
    min_start_interval: Duration,
}

impl Default for WeixinLoginCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl WeixinLoginCoordinator {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            min_start_interval: QR_LOGIN_MIN_START_INTERVAL,
        }
    }

    /// Start a QR login task for one authenticated AionUI owner.
    pub fn start(
        self: &Arc<Self>,
        owner_user_id: &str,
    ) -> Result<mpsc::Receiver<WeixinLoginEvent>, WeixinLoginStartError> {
        let permit = self.acquire(owner_user_id)?;
        Ok(spawn_login_stream(Some(permit)))
    }

    fn acquire(self: &Arc<Self>, owner_user_id: &str) -> Result<LoginPermit, WeixinLoginStartError> {
        let now = Instant::now();
        let retention = self.min_start_interval.saturating_mul(2);
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.retain(|_, slot| {
            slot.active_generation.is_some() || now.saturating_duration_since(slot.last_started) < retention
        });

        if let Some(slot) = slots.get_mut(owner_user_id) {
            if slot.active_generation.is_some() {
                return Err(WeixinLoginStartError::InProgress);
            }

            let elapsed = now.saturating_duration_since(slot.last_started);
            if elapsed < self.min_start_interval {
                return Err(WeixinLoginStartError::RateLimited {
                    retry_after: self.min_start_interval - elapsed,
                });
            }

            slot.generation = slot.generation.wrapping_add(1);
            slot.active_generation = Some(slot.generation);
            slot.last_started = now;
            return Ok(LoginPermit {
                coordinator: Arc::downgrade(self),
                owner_user_id: owner_user_id.to_owned(),
                generation: slot.generation,
            });
        }

        slots.insert(
            owner_user_id.to_owned(),
            LoginSlot {
                active_generation: Some(1),
                generation: 1,
                last_started: now,
            },
        );
        Ok(LoginPermit {
            coordinator: Arc::downgrade(self),
            owner_user_id: owner_user_id.to_owned(),
            generation: 1,
        })
    }

    fn release(&self, owner_user_id: &str, generation: u64) {
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slots.get_mut(owner_user_id)
            && slot.active_generation == Some(generation)
        {
            slot.active_generation = None;
        }
    }

    #[cfg(test)]
    fn with_min_start_interval(min_start_interval: Duration) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            min_start_interval,
        }
    }

    #[cfg(test)]
    fn start_with_api(
        self: &Arc<Self>,
        owner_user_id: &str,
        api: Arc<dyn WeixinLoginApi>,
        poll_interval: Duration,
        login_timeout: Duration,
    ) -> Result<mpsc::Receiver<WeixinLoginEvent>, WeixinLoginStartError> {
        let permit = self.acquire(owner_user_id)?;
        Ok(spawn_login_stream_with_api(
            api,
            Some(permit),
            poll_interval,
            login_timeout,
        ))
    }
}

#[derive(Debug)]
struct LoginPermit {
    coordinator: Weak<WeixinLoginCoordinator>,
    owner_user_id: String,
    generation: u64,
}

impl Drop for LoginPermit {
    fn drop(&mut self) {
        if let Some(coordinator) = self.coordinator.upgrade() {
            coordinator.release(&self.owner_user_id, self.generation);
        }
    }
}

/// SSE event emitted during the WeChat QR code login flow.
#[derive(Debug, Clone)]
pub enum WeixinLoginEvent {
    /// QR code ticket data — frontend renders this as a QR image.
    Qr(String),
    /// User scanned the QR code.
    Scanned,
    /// Login successful — returns credentials for `channel.enable-plugin`.
    Done {
        account_id: String,
        bot_token: String,
        base_url: String,
    },
    /// Login failed with an error message.
    Error(String),
}

impl WeixinLoginEvent {
    /// SSE event name string.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Qr(_) => "qr",
            Self::Scanned => "scanned",
            Self::Done { .. } => "done",
            Self::Error(_) => "error",
        }
    }

    /// Serialize the event payload as JSON.
    pub fn to_json_data(&self) -> String {
        match self {
            Self::Qr(ticket) => serde_json::to_string(&SseQrEvent {
                qrcode_data: ticket.clone(),
            })
            .unwrap_or_default(),
            Self::Scanned => "{}".into(),
            Self::Done {
                account_id,
                bot_token,
                base_url,
            } => serde_json::to_string(&SseDoneEvent {
                account_id: account_id.clone(),
                bot_token: bot_token.clone(),
                base_url: base_url.clone(),
            })
            .unwrap_or_default(),
            Self::Error(message) => serde_json::to_string(&SseErrorEvent {
                message: message.clone(),
            })
            .unwrap_or_default(),
        }
    }
}

/// Start the WeChat QR code login flow, returning a channel of SSE events.
pub fn weixin_login_stream() -> mpsc::Receiver<WeixinLoginEvent> {
    spawn_login_stream(None)
}

fn spawn_login_stream(permit: Option<LoginPermit>) -> mpsc::Receiver<WeixinLoginEvent> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let _permit = permit;
        login_flow(tx).await;
    });
    rx
}

/// Internal login flow that drives the SSE event sequence.
async fn login_flow(tx: mpsc::Sender<WeixinLoginEvent>) {
    let client = match Client::builder().timeout(Duration::from_secs(40)).build() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(WeixinLoginEvent::Error(format!("HTTP client init failed: {e}")))
                .await;
            return;
        }
    };

    let api: Arc<dyn WeixinLoginApi> = Arc::new(WeixinApi::new(client, LOGIN_BASE_URL, ""));
    login_flow_with_api(tx, api, QR_POLL_INTERVAL, QR_LOGIN_TIMEOUT).await;
}

#[async_trait::async_trait]
trait WeixinLoginApi: Send + Sync {
    async fn get_bot_qrcode(&self) -> Result<QrCodeData, ChannelError>;
    async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrCodeStatusData, ChannelError>;
}

#[async_trait::async_trait]
impl WeixinLoginApi for WeixinApi {
    async fn get_bot_qrcode(&self) -> Result<QrCodeData, ChannelError> {
        WeixinApi::get_bot_qrcode(self).await
    }

    async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrCodeStatusData, ChannelError> {
        WeixinApi::get_qrcode_status(self, qrcode).await
    }
}

#[cfg(test)]
fn spawn_login_stream_with_api(
    api: Arc<dyn WeixinLoginApi>,
    permit: Option<LoginPermit>,
    poll_interval: Duration,
    login_timeout: Duration,
) -> mpsc::Receiver<WeixinLoginEvent> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let _permit = permit;
        login_flow_with_api(tx, api, poll_interval, login_timeout).await;
    });
    rx
}

async fn login_flow_with_api(
    tx: mpsc::Sender<WeixinLoginEvent>,
    api: Arc<dyn WeixinLoginApi>,
    poll_interval: Duration,
    login_timeout: Duration,
) {
    // Step 1: Fetch QR code
    let qr_result = tokio::select! {
        _ = tx.closed() => {
            debug!("WeChat QR login SSE consumer disconnected before QR fetch completed");
            return;
        }
        result = api.get_bot_qrcode() => result,
    };
    let qr_data = match qr_result {
        Ok(data) => data,
        Err(e) => {
            error!(error = %e, "Failed to fetch WeChat QR code");
            let _ = tx
                .send(WeixinLoginEvent::Error(format!("Failed to fetch QR code: {e}")))
                .await;
            return;
        }
    };

    let ticket = match qr_data.qrcode {
        Some(t) if !t.is_empty() => t,
        _ => {
            let _ = tx
                .send(WeixinLoginEvent::Error("QR code response missing ticket".into()))
                .await;
            return;
        }
    };

    let qr_content = match qr_data.qrcode_img_content {
        Some(ref url) if !url.is_empty() => url.clone(),
        _ => {
            let _ = tx
                .send(WeixinLoginEvent::Error(
                    "QR code response missing qrcode_img_content".into(),
                ))
                .await;
            return;
        }
    };

    info!("WeChat QR code generated, waiting for scan");
    if tx.send(WeixinLoginEvent::Qr(qr_content)).await.is_err() {
        return;
    }

    // Step 2: Poll for scan status
    let deadline = tokio::time::Instant::now() + login_timeout;
    let mut scanned_sent = false;

    loop {
        if tokio::time::Instant::now() >= deadline {
            let _ = tx.send(WeixinLoginEvent::Error("QR code login timeout".into())).await;
            return;
        }

        tokio::select! {
            _ = tx.closed() => {
                debug!("WeChat QR login SSE consumer disconnected while waiting to poll");
                return;
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }

        let status_result = tokio::select! {
            _ = tx.closed() => {
                debug!("WeChat QR login SSE consumer disconnected during status poll");
                return;
            }
            result = api.get_qrcode_status(&ticket) => result,
        };
        match status_result {
            Ok(status) => {
                let state = status.status.as_deref().unwrap_or("wait");
                debug!(status = state, "WeChat QR code status");

                match state {
                    // NOTE: The API returns "scaned" (missing an 'n') — this is intentional.
                    "scaned" if !scanned_sent => {
                        scanned_sent = true;
                        if tx.send(WeixinLoginEvent::Scanned).await.is_err() {
                            return;
                        }
                    }
                    "confirmed" => {
                        let account_id = status.ilink_bot_id.unwrap_or_default();
                        let bot_token = status.bot_token.unwrap_or_default();
                        let base_url = status.baseurl.unwrap_or_else(|| LOGIN_BASE_URL.into());

                        info!(
                            account_id = %account_id,
                            "WeChat QR code login confirmed"
                        );
                        let _ = tx
                            .send(WeixinLoginEvent::Done {
                                account_id,
                                bot_token,
                                base_url,
                            })
                            .await;
                        return;
                    }
                    "expired" => {
                        let _ = tx.send(WeixinLoginEvent::Error("QR code expired".into())).await;
                        return;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                // Timeout on long-poll is expected — treat as "wait" and retry
                let err_str = e.to_string();
                if err_str.contains("timed out") || err_str.contains("Timeout") {
                    debug!("QR status poll timeout, retrying");
                    continue;
                }
                error!(error = %e, "Failed to poll QR code status");
                let _ = tx
                    .send(WeixinLoginEvent::Error(format!("Status poll failed: {e}")))
                    .await;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;
    use tokio::time::timeout;

    use super::*;

    struct PendingPollApi {
        poll_calls: AtomicUsize,
        poll_started: Notify,
    }

    impl PendingPollApi {
        fn new() -> Self {
            Self {
                poll_calls: AtomicUsize::new(0),
                poll_started: Notify::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl WeixinLoginApi for PendingPollApi {
        async fn get_bot_qrcode(&self) -> Result<QrCodeData, ChannelError> {
            Ok(QrCodeData {
                qrcode: Some("ticket-1".into()),
                qrcode_img_content: Some("qr-content".into()),
            })
        }

        async fn get_qrcode_status(&self, _qrcode: &str) -> Result<QrCodeStatusData, ChannelError> {
            self.poll_calls.fetch_add(1, Ordering::SeqCst);
            self.poll_started.notify_one();
            std::future::pending().await
        }
    }

    #[test]
    fn login_event_names() {
        assert_eq!(WeixinLoginEvent::Qr("t".into()).event_name(), "qr");
        assert_eq!(WeixinLoginEvent::Scanned.event_name(), "scanned");
        assert_eq!(
            WeixinLoginEvent::Done {
                account_id: "a".into(),
                bot_token: "b".into(),
                base_url: "c".into(),
            }
            .event_name(),
            "done"
        );
        assert_eq!(WeixinLoginEvent::Error("err".into()).event_name(), "error");
    }

    #[test]
    fn login_event_qr_json() {
        let evt = WeixinLoginEvent::Qr("ticket_123".into());
        let json = evt.to_json_data();
        assert!(json.contains("qrcodeData"));
        assert!(json.contains("ticket_123"));
    }

    #[test]
    fn login_event_scanned_json() {
        let evt = WeixinLoginEvent::Scanned;
        assert_eq!(evt.to_json_data(), "{}");
    }

    #[test]
    fn login_event_done_json() {
        let evt = WeixinLoginEvent::Done {
            account_id: "acc_1".into(),
            bot_token: "tok_1".into(),
            base_url: "https://ilinkai.weixin.qq.com".into(),
        };
        let json = evt.to_json_data();
        assert!(json.contains("accountId"));
        assert!(json.contains("acc_1"));
        assert!(json.contains("botToken"));
        assert!(json.contains("tok_1"));
        assert!(json.contains("baseUrl"));
    }

    #[test]
    fn login_event_error_json() {
        let evt = WeixinLoginEvent::Error("timeout".into());
        let json = evt.to_json_data();
        assert!(json.contains(r#""message":"timeout"#));
    }

    #[test]
    fn default_constants() {
        assert_eq!(QR_POLL_INTERVAL, Duration::from_secs(2));
        assert_eq!(QR_LOGIN_TIMEOUT, Duration::from_secs(300));
        assert_eq!(QR_LOGIN_MIN_START_INTERVAL, Duration::from_secs(10));
    }

    #[test]
    fn coordinator_is_single_flight_per_owner_without_blocking_other_owners() {
        let coordinator = Arc::new(WeixinLoginCoordinator::with_min_start_interval(Duration::ZERO));
        let owner_a = coordinator.acquire("owner-a").unwrap();

        assert_eq!(
            coordinator.acquire("owner-a").unwrap_err(),
            WeixinLoginStartError::InProgress,
        );
        let owner_b = coordinator.acquire("owner-b").unwrap();

        drop(owner_a);
        drop(owner_b);
    }

    #[test]
    fn coordinator_rate_limits_immediate_restart_after_completed_login() {
        let min_interval = Duration::from_secs(10);
        let coordinator = Arc::new(WeixinLoginCoordinator::with_min_start_interval(min_interval));
        let permit = coordinator.acquire("owner-a").unwrap();
        drop(permit);

        let error = coordinator.acquire("owner-a").unwrap_err();
        assert!(matches!(
            error,
            WeixinLoginStartError::RateLimited { retry_after }
                if retry_after > Duration::ZERO && retry_after <= min_interval
        ));
    }

    #[tokio::test]
    async fn dropping_sse_receiver_cancels_in_flight_poll_and_releases_owner() {
        let coordinator = Arc::new(WeixinLoginCoordinator::with_min_start_interval(Duration::ZERO));
        let api = Arc::new(PendingPollApi::new());
        let mut receiver = coordinator
            .start_with_api("owner-a", api.clone(), Duration::ZERO, Duration::from_secs(30))
            .unwrap();

        assert!(matches!(receiver.recv().await, Some(WeixinLoginEvent::Qr(value)) if value == "qr-content"));
        timeout(Duration::from_secs(1), api.poll_started.notified())
            .await
            .expect("status poll should start");
        assert_eq!(
            coordinator
                .start_with_api("owner-a", api.clone(), Duration::ZERO, Duration::from_secs(30),)
                .unwrap_err(),
            WeixinLoginStartError::InProgress,
        );

        drop(receiver);

        let replacement_permit = timeout(Duration::from_secs(1), async {
            loop {
                match coordinator.acquire("owner-a") {
                    Ok(permit) => break permit,
                    Err(WeixinLoginStartError::InProgress) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected restart error: {error:?}"),
                }
            }
        })
        .await
        .expect("disconnected login task should release its owner slot");
        assert_eq!(api.poll_calls.load(Ordering::SeqCst), 1);
        drop(replacement_permit);
    }
}
