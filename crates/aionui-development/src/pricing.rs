use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::IDevelopmentOperationsRepository;
use aionui_db::models::{DevelopmentModelPriceRow, DevelopmentPricedUsageEventRow};
use serde::{Deserialize, Serialize};

use crate::DevelopmentError;
use crate::operations::DevelopmentOperationsService;

pub use aionui_db::UsageDimension;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPriceInput {
    pub provider: String,
    pub model: String,
    pub input_per_million_microunits: i64,
    pub output_per_million_microunits: i64,
    pub cache_read_per_million_microunits: i64,
    pub cache_write_per_million_microunits: i64,
    pub source_id: String,
    pub version: String,
    pub effective_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageMeasurement {
    pub user_id: String,
    pub project_id: String,
    pub conversation_id: Option<String>,
    pub agent_id: Option<String>,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub team_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub duration_ms: i64,
    pub retry_count: i64,
    pub provider_reported_cost_microunits: Option<i64>,
    pub occurred_at: i64,
}

#[derive(Clone)]
pub struct PricingService {
    operations: Arc<dyn IDevelopmentOperationsRepository>,
    budget: Option<DevelopmentOperationsService>,
}

impl PricingService {
    pub fn new(operations: Arc<dyn IDevelopmentOperationsRepository>) -> Self {
        Self {
            operations,
            budget: None,
        }
    }

    pub fn with_budget(mut self, budget: DevelopmentOperationsService) -> Self {
        self.budget = Some(budget);
        self
    }

    pub async fn upsert_price(&self, input: ModelPriceInput) -> Result<DevelopmentModelPriceRow, DevelopmentError> {
        validate_price(&input)?;
        let row = DevelopmentModelPriceRow {
            id: uuid::Uuid::now_v7().to_string(),
            provider: input.provider.trim().into(),
            model: input.model.trim().into(),
            input_per_million_microunits: input.input_per_million_microunits,
            output_per_million_microunits: input.output_per_million_microunits,
            cache_read_per_million_microunits: input.cache_read_per_million_microunits,
            cache_write_per_million_microunits: input.cache_write_per_million_microunits,
            source_id: input.source_id.trim().into(),
            version: input.version.trim().into(),
            effective_at: input.effective_at,
            created_at: now_ms(),
        };
        self.operations.upsert_model_price(&row).await?;
        Ok(row)
    }

    pub async fn record(
        &self,
        measurement: UsageMeasurement,
    ) -> Result<DevelopmentPricedUsageEventRow, DevelopmentError> {
        validate_measurement(&measurement)?;
        let price = self
            .operations
            .resolve_model_price(&measurement.provider, &measurement.model, measurement.occurred_at)
            .await?;
        let (cost, status, origin, source, version, effective_at, confidence) =
            if let Some(reported) = measurement.provider_reported_cost_microunits {
                (reported, "known", "provider_reported", None, None, None, "reported")
            } else if let Some(price) = price {
                (
                    estimated_cost(&measurement, &price)?,
                    "known",
                    "platform_estimated",
                    Some(price.source_id),
                    Some(price.version),
                    Some(price.effective_at),
                    "estimated",
                )
            } else {
                (0, "unknown", "unknown", None, None, None, "estimated")
            };
        let row = DevelopmentPricedUsageEventRow {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: measurement.user_id,
            project_id: measurement.project_id,
            run_id: measurement.run_id,
            task_id: measurement.task_id,
            conversation_id: measurement.conversation_id,
            agent_id: measurement.agent_id,
            team_id: measurement.team_id,
            usage_type: "agent_turn".into(),
            source: if origin == "provider_reported" {
                "provider"
            } else {
                "platform"
            }
            .into(),
            confidence: confidence.into(),
            provider: measurement.provider,
            model: measurement.model,
            input_tokens: measurement.input_tokens,
            output_tokens: measurement.output_tokens,
            cache_read_tokens: measurement.cache_read_tokens,
            cache_write_tokens: measurement.cache_write_tokens,
            cost_microunits: cost,
            cost_status: status.into(),
            cost_origin: origin.into(),
            price_source_id: source,
            price_version: version,
            price_effective_at: effective_at,
            duration_ms: measurement.duration_ms,
            retry_count: measurement.retry_count,
            metadata_json: "{}".into(),
            created_at: measurement.occurred_at,
        };
        self.operations.append_priced_usage(&row).await?;
        if let (Some(budget), Some(run_id)) = (&self.budget, row.run_id.as_deref()) {
            budget
                .evaluate_budget(&row.user_id, run_id, "usage_recorded", row.retry_count)
                .await?;
        }
        Ok(row)
    }
}

fn estimated_cost(measurement: &UsageMeasurement, price: &DevelopmentModelPriceRow) -> Result<i64, DevelopmentError> {
    let parts = [
        (measurement.input_tokens, price.input_per_million_microunits),
        (measurement.output_tokens, price.output_per_million_microunits),
        (measurement.cache_read_tokens, price.cache_read_per_million_microunits),
        (measurement.cache_write_tokens, price.cache_write_per_million_microunits),
    ];
    let total = parts.into_iter().try_fold(0_i128, |total, (tokens, rate)| {
        total.checked_add(i128::from(tokens) * i128::from(rate))
    });
    let total = total
        .and_then(|total| total.checked_add(999_999))
        .map(|total| total / 1_000_000)
        .and_then(|total| i64::try_from(total).ok())
        .ok_or_else(|| DevelopmentError::BadRequest("usage cost overflow".into()))?;
    Ok(total)
}

fn validate_price(input: &ModelPriceInput) -> Result<(), DevelopmentError> {
    if input.provider.trim().is_empty()
        || input.model.trim().is_empty()
        || input.source_id.trim().is_empty()
        || input.version.trim().is_empty()
        || [
            input.input_per_million_microunits,
            input.output_per_million_microunits,
            input.cache_read_per_million_microunits,
            input.cache_write_per_million_microunits,
        ]
        .into_iter()
        .any(|value| value < 0)
    {
        return Err(DevelopmentError::BadRequest("invalid model price".into()));
    }
    Ok(())
}

fn validate_measurement(input: &UsageMeasurement) -> Result<(), DevelopmentError> {
    if input.user_id.is_empty()
        || input.project_id.is_empty()
        || input.provider.is_empty()
        || input.model.is_empty()
        || [
            input.input_tokens,
            input.output_tokens,
            input.cache_read_tokens,
            input.cache_write_tokens,
            input.duration_ms,
            input.retry_count,
            input.provider_reported_cost_microunits.unwrap_or(0),
        ]
        .into_iter()
        .any(|value| value < 0)
    {
        return Err(DevelopmentError::BadRequest("invalid usage measurement".into()));
    }
    Ok(())
}
