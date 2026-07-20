use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Child;

use crate::spawn::kill_process_tree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessLeaseSpec {
    pub lease_id: String,
    pub environment_id: String,
    pub timeout: Duration,
}

impl ProcessLeaseSpec {
    pub fn new(lease_id: impl Into<String>, environment_id: impl Into<String>, timeout: Duration) -> Self {
        Self {
            lease_id: lease_id.into(),
            environment_id: environment_id.into(),
            timeout,
        }
    }
}

#[derive(Debug)]
struct ProcessLeaseState {
    accepts_work: AtomicBool,
    terminal: AtomicBool,
    last_heartbeat_at: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct ProcessLease {
    spec: ProcessLeaseSpec,
    started_at: Instant,
    state: Arc<ProcessLeaseState>,
}

impl ProcessLease {
    fn new(spec: ProcessLeaseSpec) -> Self {
        Self {
            spec,
            started_at: Instant::now(),
            state: Arc::new(ProcessLeaseState {
                accepts_work: AtomicBool::new(true),
                terminal: AtomicBool::new(false),
                last_heartbeat_at: AtomicU64::new(epoch_millis()),
            }),
        }
    }

    pub fn lease_id(&self) -> &str {
        &self.spec.lease_id
    }

    pub fn environment_id(&self) -> &str {
        &self.spec.environment_id
    }

    pub fn accepts_work(&self) -> bool {
        self.state.accepts_work.load(Ordering::Acquire)
    }

    pub fn is_terminal(&self) -> bool {
        self.state.terminal.load(Ordering::Acquire)
    }

    pub fn stop_accepting_work(&self) {
        self.state.accepts_work.store(false, Ordering::Release);
    }

    pub fn heartbeat(&self) {
        if !self.is_terminal() {
            self.state.last_heartbeat_at.store(epoch_millis(), Ordering::Release);
        }
    }

    pub fn last_heartbeat_at(&self) -> u64 {
        self.state.last_heartbeat_at.load(Ordering::Acquire)
    }

    pub fn is_expired(&self) -> bool {
        self.started_at.elapsed() >= self.spec.timeout
    }

    fn remaining(&self) -> Duration {
        self.spec.timeout.saturating_sub(self.started_at.elapsed())
    }

    fn finish(&self) {
        self.stop_accepting_work();
        self.state.terminal.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct LeaseExit {
    pub status: Option<std::process::ExitStatus>,
    pub timed_out: bool,
}

#[derive(Debug)]
pub struct LeasedChild {
    child: Child,
    lease: ProcessLease,
}

impl LeasedChild {
    pub(crate) fn new(child: Child, spec: ProcessLeaseSpec) -> Self {
        Self {
            child,
            lease: ProcessLease::new(spec),
        }
    }

    pub fn lease(&self) -> &ProcessLease {
        &self.lease
    }

    pub async fn wait_with_timeout(&mut self) -> io::Result<LeaseExit> {
        match tokio::time::timeout(self.lease.remaining(), self.child.wait()).await {
            Ok(status) => {
                let status = status?;
                self.lease.finish();
                Ok(LeaseExit {
                    status: Some(status),
                    timed_out: false,
                })
            }
            Err(_) => {
                self.terminate_tree().await?;
                Ok(LeaseExit {
                    status: None,
                    timed_out: true,
                })
            }
        }
    }

    pub async fn terminate_tree(&mut self) -> io::Result<()> {
        self.lease.stop_accepting_work();
        let result = kill_process_tree(&mut self.child).await;
        self.lease.finish();
        result
    }
}

impl Deref for LeasedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for LeasedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
