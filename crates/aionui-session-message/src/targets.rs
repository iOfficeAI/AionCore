//! The single service implementation behind BOTH mentionable routes.
//!
//! `GET /api/session-messages/mentionable` (user auth, for the `@@` picker) and
//! `GET /api/runtime/session-messages/targets` (runtime token, for the agent's
//! `session list`) differ ONLY in their auth channel. Filtering and ranking
//! live here so the two cannot drift.

use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::{SessionMentionTarget, SessionMentionableQuery, SessionMentionableResponse};
use aionui_common::TimestampMs;
use aionui_conversation::session_mentions::team_id_from_extra_str;
use aionui_db::{ConversationFilters, IConversationRepository};
use aionui_project::ProjectService;
use tracing::warn;

use crate::error::SessionMessageError;

/// Hard cap on a single page, so a caller cannot ask for the whole table.
const MAX_LIMIT: u32 = 50;
const DEFAULT_LIMIT: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCandidate {
    pub id: String,
    pub name: String,
    pub project_id: Option<String>,
    pub modified_at: TimestampMs,
}

/// Match tier for a search term. Lower sorts first.
fn match_tier(name: &str, q: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let q = q.to_lowercase();
    if name.starts_with(&q) {
        Some(0)
    } else if name.contains(&q) {
        Some(1)
    } else {
        None
    }
}

/// Rank per spec §5.3.
///
/// No search term → same project, then `modified_at` desc.
/// With a search term → prefix before contains; within a tier, same project,
/// then `modified_at` desc.
///
/// `pinned` deliberately does not participate and is not even carried on
/// `TargetCandidate`: cross-session collaboration almost always happens inside
/// one project, and pinned expresses "I look at this often", not "this is
/// related to the thing at hand".
pub fn rank_targets(
    current_project_id: Option<&str>,
    q: Option<&str>,
    rows: Vec<TargetCandidate>,
) -> Vec<TargetCandidate> {
    let query = q.map(str::trim).filter(|value| !value.is_empty());
    let mut scored: Vec<(u8, u8, i64, TargetCandidate)> = rows
        .into_iter()
        .filter_map(|candidate| {
            let tier = match query {
                Some(q) => match_tier(&candidate.name, q)?,
                None => 0,
            };
            let same_project = match (current_project_id, candidate.project_id.as_deref()) {
                (Some(current), Some(candidate_project)) if current == candidate_project => 0,
                _ => 1,
            };
            Some((tier, same_project, -candidate.modified_at, candidate))
        })
        .collect();
    scored.sort_by_key(|(tier, same_project, negated_modified_at, _)| (*tier, *same_project, *negated_modified_at));
    scored.into_iter().map(|(_, _, _, candidate)| candidate).collect()
}

pub struct MentionableTargets {
    conversation_repo: Arc<dyn IConversationRepository>,
    project_service: Arc<ProjectService>,
}

impl MentionableTargets {
    pub fn new(conversation_repo: Arc<dyn IConversationRepository>, project_service: Arc<ProjectService>) -> Self {
        Self {
            conversation_repo,
            project_service,
        }
    }

    /// Hard filters (spec §5.3): exclude the current conversation, exclude
    /// team-owned conversations, exclude deleted ones (the repo already scopes
    /// by user and skips deleted rows).
    pub async fn list(
        &self,
        user_id: &str,
        current_conversation_id: &str,
        query: &SessionMentionableQuery,
    ) -> Result<SessionMentionableResponse, SessionMessageError> {
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let filters = ConversationFilters {
            cursor: query.cursor.clone(),
            limit,
            source: None,
            cron_job_id: None,
            pinned: None,
        };
        let page = self
            .conversation_repo
            .list_paginated(user_id, &filters)
            .await
            .map_err(|error| SessionMessageError::TransportUnavailable {
                reason: error.to_string(),
            })?;

        // `PaginatedResult` is `{ items, total, has_more }` — there is NO
        // cursor field. The cursor IS the last row's id (see
        // `ConversationFilters::cursor`: "the ID of the last conversation from
        // the previous page"). Take it from the DB page order, BEFORE the
        // filtering and ranking below: a page whose last row happens to be a
        // team conversation (hard-filtered) would otherwise lose its cursor and
        // the picker would page over the same rows forever.
        let next_cursor = page
            .has_more
            .then(|| page.items.last().map(|row| row.id.clone()))
            .flatten();

        let current_project_id = self
            .conversation_repo
            .get(user_id, current_conversation_id)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.project_id);

        let candidates: Vec<TargetCandidate> = page
            .items
            .into_iter()
            .filter(|row| row.id != current_conversation_id)
            .filter(|row| team_id_from_extra_str(&row.extra).is_none())
            .map(|row| TargetCandidate {
                id: row.id,
                name: row.name,
                project_id: row.project_id,
                modified_at: row.updated_at,
            })
            .collect();

        let ranked = rank_targets(current_project_id.as_deref(), query.q.as_deref(), candidates);
        let project_names = self.resolve_project_names(user_id, &ranked).await;

        Ok(SessionMentionableResponse {
            items: ranked
                .into_iter()
                .map(|candidate| SessionMentionTarget {
                    project: candidate
                        .project_id
                        .as_deref()
                        .and_then(|id| project_names.get(id).cloned()),
                    id: candidate.id,
                    name: candidate.name,
                    modified_at: candidate.modified_at,
                })
                .collect(),
            next_cursor,
        })
    }

    /// Project names for the picker's secondary line (spec §5.4). Best effort:
    /// a project that cannot be read yields no name rather than failing the
    /// whole list — the picker degrades to name + time, which is still usable.
    async fn resolve_project_names(&self, user_id: &str, candidates: &[TargetCandidate]) -> HashMap<String, String> {
        let mut names = HashMap::new();
        for project_id in candidates.iter().filter_map(|c| c.project_id.as_deref()) {
            if names.contains_key(project_id) {
                continue;
            }
            match self.project_service.get_project(user_id, project_id).await {
                Ok(detail) => {
                    // `ProjectDetail` carries `name` directly.
                    names.insert(project_id.to_owned(), detail.name);
                }
                Err(error) => {
                    warn!(
                        project_id,
                        error = %error,
                        "mentionable list: project name lookup failed; row degrades to no project label"
                    );
                }
            }
        }
        names
    }
}

#[cfg(test)]
#[path = "targets_test.rs"]
mod targets_test;
