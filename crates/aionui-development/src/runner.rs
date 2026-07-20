use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::IDevelopmentOperationsRepository;

use crate::DevelopmentError;
use crate::executor::{
    CommandExecutionInput, CommandExecutionOutput, ManagedExecutionContext, build_execution_plan, execute_plan,
};
use crate::resources::{DevelopmentResourceController, ResourceLeaseCoordinator, ResourceLeaseInput};
use crate::secrets::{SecretAccessContext, SecretReferenceRequest, SecretService};
use zeroize::Zeroize;

#[derive(Debug, Clone)]
pub struct RunnerContext {
    pub user_id: String,
    pub project_id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub gate_id: Option<String>,
}

#[derive(Clone)]
pub struct DevelopmentRunner {
    repo: Arc<dyn IDevelopmentOperationsRepository>,
    resources: ResourceLeaseCoordinator,
    controller: Arc<dyn DevelopmentResourceController>,
    secrets: Option<SecretService>,
}

impl DevelopmentRunner {
    pub fn new(
        repo: Arc<dyn IDevelopmentOperationsRepository>,
        resources: ResourceLeaseCoordinator,
        controller: Arc<dyn DevelopmentResourceController>,
    ) -> Self {
        Self {
            repo,
            resources,
            controller,
            secrets: None,
        }
    }

    pub fn with_secrets(mut self, secrets: SecretService) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn resources(&self) -> &ResourceLeaseCoordinator {
        &self.resources
    }

    pub async fn cleanup_run(&self, user_id: &str, run_id: &str) -> Result<(), DevelopmentError> {
        self.resources
            .cancel_run(user_id, run_id, self.controller.as_ref())
            .await?;
        Ok(())
    }

    pub async fn execute(
        &self,
        mut input: CommandExecutionInput<'_>,
        context: &RunnerContext,
    ) -> Result<CommandExecutionOutput, DevelopmentError> {
        let timeout_seconds = input.timeout_seconds;
        let plan = build_execution_plan(&input)?;
        input.environment.values_mut().for_each(Zeroize::zeroize);
        self.bind_environment(context, &plan.environment_id, &plan.isolation_mode)
            .await?;
        let mut planned_leases = Vec::new();
        for resource in &plan.resources {
            planned_leases.push(
                self.resources
                    .create(ResourceLeaseInput {
                        user_id: context.user_id.clone(),
                        project_id: context.project_id.clone(),
                        run_id: context.run_id.clone(),
                        task_id: context.task_id.clone(),
                        turn_id: context.turn_id.clone(),
                        gate_id: context.gate_id.clone(),
                        environment_id: plan.environment_id.clone(),
                        environment_kind: plan.isolation_mode.clone(),
                        resource_kind: resource.resource_kind.clone(),
                        resource_identifier: resource.resource_identifier.clone(),
                        cleanup_order: resource.cleanup_order,
                        ttl_ms: timeout_seconds.max(1).saturating_mul(1000),
                    })
                    .await?,
            );
        }
        let managed = ManagedExecutionContext {
            user_id: &context.user_id,
            project_id: &context.project_id,
            run_id: &context.run_id,
            task_id: context.task_id.as_deref(),
            turn_id: context.turn_id.as_deref(),
            gate_id: context.gate_id.as_deref(),
            resources: &self.resources,
        };
        let result = execute_plan(&plan, timeout_seconds, Some(&managed)).await;
        let cleanup_result = match &result {
            Ok(output) => output.status.as_str(),
            Err(_) => "runner_error",
        };
        for lease in planned_leases {
            if lease.resource_kind != "service" {
                self.resources.complete(&lease.id, cleanup_result).await?;
            }
        }
        result
    }

    pub async fn execute_with_secret_references(
        &self,
        mut input: CommandExecutionInput<'_>,
        context: &RunnerContext,
        access: &SecretAccessContext,
        requests: &[SecretReferenceRequest],
    ) -> Result<CommandExecutionOutput, DevelopmentError> {
        let service = self
            .secrets
            .as_ref()
            .ok_or_else(|| DevelopmentError::Conflict("Secret materialization is unavailable".into()))?;
        let materialized = service.materialize(&context.user_id, access, requests).await?;
        for (key, value) in materialized.values() {
            if input.environment.insert(key.clone(), value.clone()).is_some() {
                input.environment.values_mut().for_each(Zeroize::zeroize);
                return Err(DevelopmentError::BadRequest(
                    "Secret environment key conflicts with an existing value".into(),
                ));
            }
        }
        self.execute(input, context).await
    }

    async fn bind_environment(
        &self,
        context: &RunnerContext,
        environment_id: &str,
        environment_kind: &str,
    ) -> Result<(), DevelopmentError> {
        let mut entities = vec![("run", context.run_id.as_str())];
        if let Some(task_id) = context.task_id.as_deref() {
            entities.push(("task", task_id));
        }
        if let Some(turn_id) = context.turn_id.as_deref() {
            entities.push(("turn", turn_id));
        }
        if let Some(gate_id) = context.gate_id.as_deref() {
            entities.push(("gate", gate_id));
        }
        let now = now_ms();
        for (entity_type, entity_id) in entities {
            self.repo
                .bind_execution_environment(entity_type, entity_id, environment_id, environment_kind, now)
                .await?;
        }
        Ok(())
    }
}
