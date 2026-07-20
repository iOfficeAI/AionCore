use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::{DynamicAgentProbe, DynamicProbeFailure, DynamicProbeSession, DynamicProbeSessionFactory};
use aionui_api_types::{AgentProbeErrorCategory, AgentProbeStatus, AgentProbeStep};

struct HealthyFactory;

struct HealthySession;

#[async_trait::async_trait]
impl DynamicProbeSessionFactory for HealthyFactory {
    async fn spawn(&self, _agent_id: &str) -> Result<Box<dyn DynamicProbeSession>, DynamicProbeFailure> {
        Ok(Box::new(HealthySession))
    }
}

#[async_trait::async_trait]
impl DynamicProbeSession for HealthySession {
    async fn initialize(&mut self) -> Result<(), DynamicProbeFailure> {
        Ok(())
    }

    async fn models(&mut self) -> Result<Vec<String>, DynamicProbeFailure> {
        Ok(vec!["model-a".into(), "model-b".into()])
    }

    async fn minimal_prompt(&mut self) -> Result<(), DynamicProbeFailure> {
        Ok(())
    }

    async fn cancel(&mut self) -> Result<(), DynamicProbeFailure> {
        Ok(())
    }

    async fn resume(&mut self) -> Result<(), DynamicProbeFailure> {
        Err(DynamicProbeFailure::unsupported("session/resume is not advertised"))
    }
}

#[tokio::test]
async fn probe_records_all_steps_in_order_and_only_observed_models() {
    let probe = DynamicAgentProbe::new(Arc::new(HealthyFactory));

    let result = probe.run("codex").await;

    assert_eq!(result.agent_id, "codex");
    assert_eq!(result.available_models, vec!["model-a", "model-b"]);
    assert!(result.checked_at > 0);
    assert_eq!(
        result.steps.iter().map(|step| step.step).collect::<Vec<_>>(),
        vec![
            AgentProbeStep::Spawn,
            AgentProbeStep::Initialize,
            AgentProbeStep::Models,
            AgentProbeStep::MinimalPrompt,
            AgentProbeStep::Cancel,
            AgentProbeStep::Resume,
        ]
    );
    assert!(result.steps.iter().all(|step| step.started_at > 0));
    assert!(result.steps.iter().all(|step| step.duration_ms >= 0));
    assert_eq!(result.steps[4].status, AgentProbeStatus::Passed);
    assert_eq!(result.steps[5].status, AgentProbeStatus::Unsupported);
    assert_eq!(result.steps[5].error_category, Some(AgentProbeErrorCategory::Protocol));
    assert!(result.is_usable());
}

struct AuthFailureFactory;

#[async_trait::async_trait]
impl DynamicProbeSessionFactory for AuthFailureFactory {
    async fn spawn(&self, _agent_id: &str) -> Result<Box<dyn DynamicProbeSession>, DynamicProbeFailure> {
        Err(DynamicProbeFailure::new(
            AgentProbeErrorCategory::Authentication,
            "token sk-live-secret was rejected",
        ))
    }
}

#[tokio::test]
async fn probe_normalizes_failure_and_redacts_provider_output() {
    let probe = DynamicAgentProbe::new(Arc::new(AuthFailureFactory));

    let result = probe.run("claude").await;

    assert!(result.available_models.is_empty());
    assert_eq!(result.steps.len(), 1);
    assert_eq!(result.steps[0].step, AgentProbeStep::Spawn);
    assert_eq!(result.steps[0].status, AgentProbeStatus::Failed);
    assert_eq!(
        result.steps[0].error_category,
        Some(AgentProbeErrorCategory::Authentication)
    );
    assert_eq!(
        result.steps[0].error_message.as_deref(),
        Some("provider authentication failed")
    );
    assert!(!result.is_usable());
}

struct SlowFactory;

#[async_trait::async_trait]
impl DynamicProbeSessionFactory for SlowFactory {
    async fn spawn(&self, _agent_id: &str) -> Result<Box<dyn DynamicProbeSession>, DynamicProbeFailure> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(Box::new(HealthySession))
    }
}

#[tokio::test]
async fn probe_classifies_step_timeout_without_leaking_adapter_output() {
    let probe = DynamicAgentProbe::new(Arc::new(SlowFactory)).with_step_timeout(Duration::from_millis(5));

    let result = probe.run("slow-agent").await;

    assert_eq!(result.steps.len(), 1);
    assert_eq!(result.steps[0].step, AgentProbeStep::Spawn);
    assert_eq!(result.steps[0].status, AgentProbeStatus::TimedOut);
    assert_eq!(result.steps[0].error_category, Some(AgentProbeErrorCategory::Timeout));
    assert_eq!(
        result.steps[0].error_message.as_deref(),
        Some("the Agent operation timed out")
    );
    assert!(!result.is_usable());
}

struct RejectedModelFactory;

struct RejectedModelSession;

#[async_trait::async_trait]
impl DynamicProbeSessionFactory for RejectedModelFactory {
    async fn spawn(&self, _agent_id: &str) -> Result<Box<dyn DynamicProbeSession>, DynamicProbeFailure> {
        Ok(Box::new(RejectedModelSession))
    }
}

#[async_trait::async_trait]
impl DynamicProbeSession for RejectedModelSession {
    async fn initialize(&mut self) -> Result<(), DynamicProbeFailure> {
        Ok(())
    }

    async fn models(&mut self) -> Result<Vec<String>, DynamicProbeFailure> {
        Err(DynamicProbeFailure::new(
            AgentProbeErrorCategory::ModelRejected,
            "provider rejected model private-model-name",
        ))
    }

    async fn minimal_prompt(&mut self) -> Result<(), DynamicProbeFailure> {
        unreachable!("model rejection must stop the probe")
    }

    async fn cancel(&mut self) -> Result<(), DynamicProbeFailure> {
        unreachable!("model rejection must stop the probe")
    }

    async fn resume(&mut self) -> Result<(), DynamicProbeFailure> {
        unreachable!("model rejection must stop the probe")
    }
}

#[tokio::test]
async fn probe_classifies_rejected_model_and_exposes_no_unverified_models() {
    let probe = DynamicAgentProbe::new(Arc::new(RejectedModelFactory));

    let result = probe.run("codex").await;

    assert!(result.available_models.is_empty());
    assert_eq!(result.steps.len(), 3);
    assert_eq!(result.steps[2].step, AgentProbeStep::Models);
    assert_eq!(result.steps[2].status, AgentProbeStatus::Failed);
    assert_eq!(
        result.steps[2].error_category,
        Some(AgentProbeErrorCategory::ModelRejected)
    );
    assert_eq!(
        result.steps[2].error_message.as_deref(),
        Some("the selected model was rejected")
    );
}
