use std::sync::Arc;

use aionui_db::models::DevelopmentRunRow;
use aionui_db::{IDevelopmentOperationsRepository, IDevelopmentRepository, IProjectRepository};
use serde_json::{Value, json};

use crate::{
    BudgetEvaluation, DevelopmentError, DevelopmentOperationsService, PricingService, RecordedUsageOutcome,
    UsageMeasurement,
};

#[derive(Debug, Clone)]
pub struct ObservedAgentTurnUsage {
    pub user_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub agent_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub team_id: Option<String>,
    pub slot_id: Option<String>,
    pub usage: Option<Value>,
    pub duration_ms: i64,
    pub retry_count: i64,
    pub occurred_at: i64,
}

#[derive(Debug, Clone)]
pub struct DevelopmentBudgetAdmission {
    pub run_id: String,
    pub run_status: String,
    pub evaluation: BudgetEvaluation,
}

#[derive(Clone)]
pub struct DevelopmentUsageIngestor {
    project_repo: Arc<dyn IProjectRepository>,
    development_repo: Arc<dyn IDevelopmentRepository>,
    operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
    operations: DevelopmentOperationsService,
    pricing: PricingService,
}

#[derive(Debug, Clone, Copy, Default)]
struct UsageSnapshot {
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_microunits: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ReportedUsageSnapshot {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    cost_microunits: Option<i64>,
}

impl DevelopmentUsageIngestor {
    pub fn new(
        project_repo: Arc<dyn IProjectRepository>,
        development_repo: Arc<dyn IDevelopmentRepository>,
        operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
        operations: DevelopmentOperationsService,
        pricing: PricingService,
    ) -> Self {
        Self {
            project_repo,
            development_repo,
            operations_repo,
            operations,
            pricing,
        }
    }

    pub async fn admit(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: Option<&str>,
    ) -> Result<Option<DevelopmentBudgetAdmission>, DevelopmentError> {
        let Some(project) = self.resolve_project(user_id, conversation_id, team_id).await? else {
            return Ok(None);
        };
        let runs = self.development_repo.list_runs(user_id, Some(&project.id)).await?;
        let Some(run) = select_run(&runs, team_id)? else {
            return Ok(None);
        };
        let evaluation = self
            .operations
            .evaluate_budget(user_id, &run.id, "agent_turn_admission", 0)
            .await?;
        Ok(Some(DevelopmentBudgetAdmission {
            run_id: run.id.clone(),
            run_status: run.status.clone(),
            evaluation,
        }))
    }

    pub async fn record(
        &self,
        event: ObservedAgentTurnUsage,
    ) -> Result<Option<RecordedUsageOutcome>, DevelopmentError> {
        let project = self
            .resolve_project(&event.user_id, &event.conversation_id, event.team_id.as_deref())
            .await?;
        let Some(project) = project else {
            return Ok(None);
        };

        let runs = self
            .development_repo
            .list_runs(&event.user_id, Some(&project.id))
            .await?;
        let Some(run) = select_run(&runs, event.team_id.as_deref())? else {
            return Ok(None);
        };

        let previous = self
            .operations_repo
            .latest_priced_usage_for_conversation(&event.user_id, &event.conversation_id)
            .await?
            .and_then(|row| serde_json::from_str::<Value>(&row.metadata_json).ok())
            .map(|metadata| snapshot_from_metadata(&metadata))
            .unwrap_or_default();
        let reported = reported_snapshot_from_usage(event.usage.as_ref());
        let current = merge_reported_snapshot(reported, previous);
        let delta = snapshot_delta(reported, previous);
        let context_occupancy = context_occupancy(event.usage.as_ref());

        let tasks = self.development_repo.list_tasks(&run.id).await?;
        let task_owner = event.slot_id.as_deref().or(event.agent_id.as_deref());
        let mut matching_tasks = tasks.iter().filter(|task| {
            !matches!(task.status.as_str(), "completed" | "cancelled" | "deleted")
                && task_owner.is_some_and(|owner| task.owner.as_deref() == Some(owner))
        });
        let owner_task = matching_tasks.next().filter(|_| matching_tasks.next().is_none());
        let active_tasks = tasks
            .iter()
            .filter(|task| !matches!(task.status.as_str(), "completed" | "cancelled" | "deleted"))
            .collect::<Vec<_>>();
        let task_id = owner_task
            .or_else(|| (run.execution_mode == "single" && active_tasks.len() == 1).then(|| active_tasks[0]))
            .map(|task| task.id.clone());

        let provider = non_empty_or_unknown(&event.provider);
        let model = non_empty_or_unknown(&event.model);
        let event_id = format!("{}:{}", event.conversation_id, event.turn_id);
        let outcome = self
            .pricing
            .record_observed(
                UsageMeasurement {
                    user_id: event.user_id,
                    project_id: project.id,
                    conversation_id: Some(event.conversation_id),
                    agent_id: event.agent_id,
                    task_id,
                    run_id: Some(run.id.clone()),
                    team_id: event.team_id,
                    provider,
                    model,
                    input_tokens: delta.input_tokens,
                    output_tokens: delta.output_tokens,
                    cache_read_tokens: delta.cache_read_tokens,
                    cache_write_tokens: delta.cache_write_tokens,
                    duration_ms: event.duration_ms.max(0),
                    retry_count: event.retry_count.max(0),
                    provider_reported_cost_microunits: delta.cost_microunits,
                    occurred_at: event.occurred_at,
                },
                &event_id,
                json!({
                    "turn_id": event.turn_id,
                    "observed_cumulative": {
                        "input_tokens": current.input_tokens,
                        "output_tokens": current.output_tokens,
                        "cache_read_tokens": current.cache_read_tokens,
                        "cache_write_tokens": current.cache_write_tokens,
                        "cost_microunits": current.cost_microunits,
                    },
                    "context_occupancy": context_occupancy,
                    "usage_observation_status": if event.usage.is_some() { "reported" } else { "unavailable" },
                }),
            )
            .await?;
        Ok(Some(outcome))
    }

    pub async fn pause_after_observation_failure(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: Option<&str>,
        message: &str,
    ) -> Result<Option<String>, DevelopmentError> {
        let Some(project) = self.resolve_project(user_id, conversation_id, team_id).await? else {
            return Ok(None);
        };
        let runs = self.development_repo.list_runs(user_id, Some(&project.id)).await?;
        let Some(run) = select_run(&runs, team_id)? else {
            return Ok(None);
        };
        self.operations
            .pause_after_runtime_action_failure(user_id, &run.id, message)
            .await?;
        Ok(Some(run.id.clone()))
    }

    async fn resolve_project(
        &self,
        user_id: &str,
        conversation_id: &str,
        team_id: Option<&str>,
    ) -> Result<Option<aionui_db::models::ProjectRow>, DevelopmentError> {
        if let Some(project) = self
            .project_repo
            .get_for_resource(user_id, "conversation", conversation_id)
            .await?
        {
            Ok(Some(project))
        } else if let Some(team_id) = team_id {
            self.project_repo
                .get_for_resource(user_id, "team", team_id)
                .await
                .map_err(DevelopmentError::from)
        } else {
            Ok(None)
        }
    }
}

fn non_empty_or_unknown(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "unknown".into()
    } else {
        value.into()
    }
}

fn reported_snapshot_from_usage(usage: Option<&Value>) -> ReportedUsageSnapshot {
    let Some(usage) = usage else {
        return ReportedUsageSnapshot::default();
    };
    let mut snapshot = ReportedUsageSnapshot {
        input_tokens: integer_at(usage, &["input_tokens", "inputTokens"]),
        output_tokens: integer_at(usage, &["output_tokens", "outputTokens"]),
        cache_read_tokens: integer_at(usage, &["cache_read_tokens", "cacheReadTokens"]),
        cache_write_tokens: integer_at(usage, &["cache_write_tokens", "cacheWriteTokens"]),
        cost_microunits: integer_at(usage, &["cost_microunits", "costMicrounits"]),
    };
    if snapshot.cost_microunits.is_none()
        && let Some(cost) = usage.get("cost")
        && cost
            .get("currency")
            .and_then(Value::as_str)
            .is_some_and(|currency| currency.eq_ignore_ascii_case("USD"))
        && let Some(amount) = cost.get("amount").and_then(Value::as_f64)
        && amount.is_finite()
        && amount >= 0.0
    {
        snapshot.cost_microunits = Some((amount * 1_000_000.0).round() as i64);
    }
    snapshot
}

fn merge_reported_snapshot(reported: ReportedUsageSnapshot, previous: UsageSnapshot) -> UsageSnapshot {
    UsageSnapshot {
        input_tokens: reported.input_tokens.unwrap_or(previous.input_tokens),
        output_tokens: reported.output_tokens.unwrap_or(previous.output_tokens),
        cache_read_tokens: reported.cache_read_tokens.unwrap_or(previous.cache_read_tokens),
        cache_write_tokens: reported.cache_write_tokens.unwrap_or(previous.cache_write_tokens),
        cost_microunits: reported.cost_microunits.or(previous.cost_microunits),
    }
}

fn context_occupancy(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?;
    let used = integer_at(usage, &["used"])?;
    Some(json!({
        "used": used,
        "size": integer_at(usage, &["size"]),
    }))
}

fn snapshot_from_metadata(metadata: &Value) -> UsageSnapshot {
    let value = metadata.get("observed_cumulative").unwrap_or(&Value::Null);
    UsageSnapshot {
        input_tokens: integer_at(value, &["input_tokens"]).unwrap_or_default(),
        output_tokens: integer_at(value, &["output_tokens"]).unwrap_or_default(),
        cache_read_tokens: integer_at(value, &["cache_read_tokens"]).unwrap_or_default(),
        cache_write_tokens: integer_at(value, &["cache_write_tokens"]).unwrap_or_default(),
        cost_microunits: integer_at(value, &["cost_microunits"]),
    }
}

fn integer_at(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        })
        .filter(|value| *value >= 0)
}

fn snapshot_delta(current: ReportedUsageSnapshot, previous: UsageSnapshot) -> UsageSnapshot {
    UsageSnapshot {
        input_tokens: current
            .input_tokens
            .map_or(0, |current| counter_delta(current, previous.input_tokens)),
        output_tokens: current
            .output_tokens
            .map_or(0, |current| counter_delta(current, previous.output_tokens)),
        cache_read_tokens: current
            .cache_read_tokens
            .map_or(0, |current| counter_delta(current, previous.cache_read_tokens)),
        cache_write_tokens: current
            .cache_write_tokens
            .map_or(0, |current| counter_delta(current, previous.cache_write_tokens)),
        cost_microunits: current.cost_microunits.map(|current| {
            previous
                .cost_microunits
                .map_or(current, |previous| counter_delta(current, previous))
        }),
    }
}

fn select_run<'a>(
    runs: &'a [DevelopmentRunRow],
    team_id: Option<&str>,
) -> Result<Option<&'a DevelopmentRunRow>, DevelopmentError> {
    let compatible = runs
        .iter()
        .filter(|run| match team_id {
            Some(team_id) => run.team_id.as_deref() == Some(team_id),
            None => run.team_id.is_none() && run.execution_mode == "single",
        })
        .collect::<Vec<_>>();
    let open = compatible
        .iter()
        .copied()
        .filter(|run| !matches!(run.status.as_str(), "succeeded" | "failed" | "paused" | "cancelled"))
        .collect::<Vec<_>>();
    if open.len() > 1 {
        return Err(DevelopmentError::Conflict(
            "multiple compatible development runs are active; bind the conversation to an unambiguous run".into(),
        ));
    }
    if let Some(run) = open.into_iter().next() {
        return Ok(Some(run));
    }
    Ok(compatible
        .into_iter()
        .max_by_key(|run| (run.updated_at, run.created_at, run.id.as_str()))
        .filter(|run| matches!(run.status.as_str(), "paused" | "cancelled")))
}

fn counter_delta(current: i64, previous: i64) -> i64 {
    if current >= previous {
        current - previous
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, team_id: Option<&str>, execution_mode: &str, status: &str, updated_at: i64) -> DevelopmentRunRow {
        DevelopmentRunRow {
            id: id.into(),
            user_id: "user".into(),
            project_id: "project".into(),
            team_id: team_id.map(str::to_owned),
            source_channel: None,
            source_user_id: None,
            execution_mode: execution_mode.into(),
            status: status.into(),
            request_summary: "test".into(),
            acceptance_criteria: "[]".into(),
            baseline_commit: None,
            integration_branch: None,
            started_at: Some(updated_at),
            finished_at: None,
            created_at: updated_at,
            updated_at,
        }
    }

    #[test]
    fn acp_context_occupancy_is_not_misreported_as_billable_tokens() {
        let reported = reported_snapshot_from_usage(Some(&json!({
            "used": 42,
            "size": 100,
            "cost": {"amount": 1.25, "currency": "USD"}
        })));
        assert_eq!(reported.input_tokens, None);
        assert_eq!(reported.cost_microunits, Some(1_250_000));
        assert_eq!(
            context_occupancy(Some(&json!({"used": 42, "size": 100}))),
            Some(json!({"used": 42, "size": 100}))
        );
    }

    #[test]
    fn missing_usage_preserves_the_previous_cumulative_watermark() {
        let previous = UsageSnapshot {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 5,
            cache_write_tokens: 3,
            cost_microunits: Some(50),
        };
        let missing = ReportedUsageSnapshot::default();
        assert_eq!(snapshot_delta(missing, previous).input_tokens, 0);
        let preserved = merge_reported_snapshot(missing, previous);
        let resumed = reported_snapshot_from_usage(Some(&json!({"input_tokens": 130, "output_tokens": 25})));
        let delta = snapshot_delta(resumed, preserved);
        assert_eq!(delta.input_tokens, 30);
        assert_eq!(delta.output_tokens, 5);
    }

    #[test]
    fn counter_reset_starts_a_new_observation_epoch() {
        assert_eq!(counter_delta(10, 100), 10);
        assert_eq!(counter_delta(120, 100), 20);
    }

    #[test]
    fn run_selection_never_crosses_team_or_single_boundaries() {
        let runs = vec![
            run("other-team", Some("team-b"), "team", "running", 4),
            run("single", None, "single", "running", 3),
            run("wanted-team", Some("team-a"), "team", "running", 2),
        ];
        assert_eq!(select_run(&runs, Some("team-a")).unwrap().unwrap().id, "wanted-team");
        assert_eq!(select_run(&runs, None).unwrap().unwrap().id, "single");
    }

    #[test]
    fn ambiguous_active_single_runs_fail_closed() {
        let runs = vec![
            run("single-a", None, "single", "running", 2),
            run("single-b", None, "single", "reviewing", 1),
        ];
        assert!(matches!(select_run(&runs, None), Err(DevelopmentError::Conflict(_))));
    }

    #[test]
    fn completed_historical_run_does_not_block_an_unattached_conversation() {
        let runs = vec![run("completed", None, "single", "succeeded", 1)];
        assert!(select_run(&runs, None).unwrap().is_none());
    }
}
