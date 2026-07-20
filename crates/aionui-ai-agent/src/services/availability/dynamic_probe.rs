use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use aionui_api_types::{
    AgentDynamicProbeResult, AgentProbeErrorCategory, AgentProbeStatus, AgentProbeStep, AgentProbeStepResult,
};
use aionui_common::now_ms;

/// A normalized failure safe to expose outside the Agent adapter boundary.
#[derive(Debug, Clone)]
pub struct DynamicProbeFailure {
    pub category: AgentProbeErrorCategory,
    pub status: AgentProbeStatus,
    pub message: String,
}

impl DynamicProbeFailure {
    pub fn new(category: AgentProbeErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            status: AgentProbeStatus::Failed,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            category: AgentProbeErrorCategory::Protocol,
            status: AgentProbeStatus::Unsupported,
            message: message.into(),
        }
    }

    pub fn timed_out(message: impl Into<String>) -> Self {
        Self {
            category: AgentProbeErrorCategory::Timeout,
            status: AgentProbeStatus::TimedOut,
            message: message.into(),
        }
    }
}

/// Adapter boundary used by dynamic preflight. Implementations own their
/// throwaway process/session and must clean it up on drop.
#[async_trait::async_trait]
pub trait DynamicProbeSession: Send {
    async fn initialize(&mut self) -> Result<(), DynamicProbeFailure>;
    async fn models(&mut self) -> Result<Vec<String>, DynamicProbeFailure>;
    async fn minimal_prompt(&mut self) -> Result<(), DynamicProbeFailure>;
    async fn cancel(&mut self) -> Result<(), DynamicProbeFailure>;
    async fn resume(&mut self) -> Result<(), DynamicProbeFailure>;
}

#[async_trait::async_trait]
pub trait DynamicProbeSessionFactory: Send + Sync {
    async fn spawn(&self, agent_id: &str) -> Result<Box<dyn DynamicProbeSession>, DynamicProbeFailure>;
}

#[derive(Clone)]
pub struct DynamicAgentProbe {
    factory: Arc<dyn DynamicProbeSessionFactory>,
    step_timeout: Duration,
}

impl DynamicAgentProbe {
    pub fn new(factory: Arc<dyn DynamicProbeSessionFactory>) -> Self {
        Self {
            factory,
            step_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_step_timeout(mut self, step_timeout: Duration) -> Self {
        self.step_timeout = step_timeout;
        self
    }

    pub async fn run(&self, agent_id: &str) -> AgentDynamicProbeResult {
        let checked_at = now_ms();
        let mut steps = Vec::with_capacity(6);
        let spawn_started = now_ms();
        let spawn_timer = Instant::now();
        let mut session = match tokio::time::timeout(self.step_timeout, self.factory.spawn(agent_id)).await {
            Err(_) => {
                steps.push(failed(
                    AgentProbeStep::Spawn,
                    spawn_started,
                    spawn_timer,
                    DynamicProbeFailure::timed_out("Agent spawn timed out"),
                ));
                return result(agent_id, checked_at, steps, Vec::new());
            }
            Ok(session) => match session {
                Ok(session) => {
                    steps.push(passed(AgentProbeStep::Spawn, spawn_started, spawn_timer));
                    session
                }
                Err(error) => {
                    steps.push(failed(AgentProbeStep::Spawn, spawn_started, spawn_timer, error));
                    return result(agent_id, checked_at, steps, Vec::new());
                }
            },
        };

        if run_step(
            &mut steps,
            AgentProbeStep::Initialize,
            self.step_timeout,
            session.initialize(),
        )
        .await
        .is_err()
        {
            return result(agent_id, checked_at, steps, Vec::new());
        }

        let models_started = now_ms();
        let models_timer = Instant::now();
        let mut available_models = match tokio::time::timeout(self.step_timeout, session.models()).await {
            Ok(Ok(models)) => {
                steps.push(passed(AgentProbeStep::Models, models_started, models_timer));
                deduplicate_models(models)
            }
            Ok(Err(error)) => {
                steps.push(failed(AgentProbeStep::Models, models_started, models_timer, error));
                return result(agent_id, checked_at, steps, Vec::new());
            }
            Err(_) => {
                steps.push(failed(
                    AgentProbeStep::Models,
                    models_started,
                    models_timer,
                    DynamicProbeFailure::timed_out("Agent model discovery timed out"),
                ));
                return result(agent_id, checked_at, steps, Vec::new());
            }
        };

        if run_step(
            &mut steps,
            AgentProbeStep::MinimalPrompt,
            self.step_timeout,
            session.minimal_prompt(),
        )
        .await
        .is_err()
        {
            available_models.clear();
            return result(agent_id, checked_at, steps, available_models);
        }
        if run_step(&mut steps, AgentProbeStep::Cancel, self.step_timeout, session.cancel())
            .await
            .is_err()
        {
            available_models.clear();
            return result(agent_id, checked_at, steps, available_models);
        }

        // Resume is optional in ACP. Record unsupported explicitly without
        // making an otherwise healthy Agent unusable.
        let _ = run_step(&mut steps, AgentProbeStep::Resume, self.step_timeout, session.resume()).await;
        result(agent_id, checked_at, steps, available_models)
    }
}

async fn run_step<F>(
    steps: &mut Vec<AgentProbeStepResult>,
    step: AgentProbeStep,
    timeout: Duration,
    future: F,
) -> Result<(), ()>
where
    F: std::future::Future<Output = Result<(), DynamicProbeFailure>>,
{
    let started_at = now_ms();
    let timer = Instant::now();
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(())) => {
            steps.push(passed(step, started_at, timer));
            Ok(())
        }
        Ok(Err(error)) => {
            steps.push(failed(step, started_at, timer, error));
            Err(())
        }
        Err(_) => {
            steps.push(failed(
                step,
                started_at,
                timer,
                DynamicProbeFailure::timed_out("Agent probe step timed out"),
            ));
            Err(())
        }
    }
}

fn passed(step: AgentProbeStep, started_at: i64, timer: Instant) -> AgentProbeStepResult {
    AgentProbeStepResult {
        step,
        status: AgentProbeStatus::Passed,
        started_at,
        duration_ms: timer.elapsed().as_millis() as i64,
        error_category: None,
        error_message: None,
    }
}

fn failed(step: AgentProbeStep, started_at: i64, timer: Instant, error: DynamicProbeFailure) -> AgentProbeStepResult {
    AgentProbeStepResult {
        step,
        status: error.status,
        started_at,
        duration_ms: timer.elapsed().as_millis() as i64,
        error_category: Some(error.category),
        error_message: Some(safe_message(error.category)),
    }
}

fn safe_message(category: AgentProbeErrorCategory) -> String {
    match category {
        AgentProbeErrorCategory::Authentication => "provider authentication failed",
        AgentProbeErrorCategory::ModelRejected => "the selected model was rejected",
        AgentProbeErrorCategory::Protocol => "the Agent does not support this protocol operation",
        AgentProbeErrorCategory::Startup => "the Agent failed to start",
        AgentProbeErrorCategory::Timeout => "the Agent operation timed out",
        AgentProbeErrorCategory::RateLimited => "the provider rate limit was reached",
        AgentProbeErrorCategory::Permission => "the Agent requires a permission that was not granted",
        AgentProbeErrorCategory::RuntimeMissing => "the required Agent runtime is unavailable",
        AgentProbeErrorCategory::Unknown => "the Agent probe failed",
    }
    .to_owned()
}

fn deduplicate_models(models: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .into_iter()
        .filter(|model| !model.trim().is_empty())
        .filter(|model| seen.insert(model.clone()))
        .collect()
}

fn result(
    agent_id: &str,
    checked_at: i64,
    steps: Vec<AgentProbeStepResult>,
    available_models: Vec<String>,
) -> AgentDynamicProbeResult {
    AgentDynamicProbeResult {
        agent_id: agent_id.to_owned(),
        checked_at,
        steps,
        available_models,
    }
}
