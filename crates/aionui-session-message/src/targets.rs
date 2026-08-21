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
use aionui_db::{IConversationRepository, MentionableCandidatesParams};
use aionui_project::ProjectService;
use tracing::warn;

use crate::error::SessionMessageError;

/// Hard cap on a single page, so a caller cannot ask for the whole table.
const MAX_LIMIT: u32 = 50;
const DEFAULT_LIMIT: u32 = 20;
/// Rows read beyond the requested page size per round trip, so a few
/// hard-filtered rows inside the window do not cost a second query.
const SCAN_SLACK: u32 = 10;
/// Round-trip ceiling for one request. Without it, a user whose highest-ranked
/// conversations are all team-owned would walk the entire table in one call.
const MAX_SCANS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCandidate {
    pub id: String,
    pub name: String,
    pub project_id: Option<String>,
    pub modified_at: TimestampMs,
}

/// Read a page cursor.
///
/// The cursor is an offset into the ranked order rather than a row id: the
/// ranking keys (search tier, same-project) are request-scoped, so no single
/// row id identifies a stable resume point.
///
/// Unparsable input restarts from the top instead of failing the request. A
/// picker that returns nothing is worse than one that repeats its first page,
/// and the only ways to get here are a client bug or a cursor minted by a build
/// that handed back conversation ids.
fn parse_cursor(cursor: Option<&str>) -> u32 {
    let Some(raw) = cursor.map(str::trim).filter(|value| !value.is_empty()) else {
        return 0;
    };
    raw.parse().unwrap_or_else(|_| {
        warn!(
            cursor = raw,
            "mentionable list: unparsable cursor, restarting from the first page"
        );
        0
    })
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

    /// One ranked page of mentionable conversations.
    ///
    /// Ranking (design §5.3) happens in the DB query, not here: sorting an
    /// already-truncated recency page can only reorder the newest N rows, so a
    /// name match or a same-project conversation outside that window would stay
    /// invisible however highly it ranks. What remains for this layer is the
    /// pair of hard filters whose predicates live in Rust — the caller's own
    /// conversation, and team-owned rows (via the shared
    /// `team_id_from_extra_str`, so this cannot drift from the send boundary's
    /// check). Deleted rows need no filter: conversations are hard-deleted.
    ///
    /// Those filters punch holes in the DB page, so the scan reads past them
    /// (`SCAN_SLACK`, repeated up to `MAX_SCANS` times) and the returned cursor
    /// counts SCANNED rows. A page that would otherwise shrink to a couple of
    /// rows — or to none — still comes back full.
    pub async fn list(
        &self,
        user_id: &str,
        current_conversation_id: &str,
        query: &SessionMentionableQuery,
    ) -> Result<SessionMentionableResponse, SessionMessageError> {
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
        // A blank search term is "no search term", not "match nothing".
        let name_query = query
            .q
            .as_deref()
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(str::to_owned);

        // `project_id` narrows the result set. It is advertised on both outlets
        // (the `@@` picker and `session list`'s descriptor schema), so ignoring
        // it would hand a caller that scoped by project the unscoped list with
        // no way to tell.
        let project_scope = query
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);

        // Narrowing to one id answers "is this still a legal target?" — the UI
        // asks before mentioning a conversation off an old message, since a `@@`
        // reference is atomic and a stale one fails the whole send. It runs
        // through the same filters as any other page, so the answer is the
        // picker's own.
        let id_scope = query
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);

        // The project of the conversation the picker sits in decides the
        // same-project grouping, so it must be known BEFORE the scan — it is a
        // sort key of the query now.
        let current_project_id = self
            .conversation_repo
            .get(user_id, current_conversation_id)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.project_id);

        let scan_limit = limit as u32 + SCAN_SLACK;
        let mut offset = parse_cursor(query.cursor.as_deref());
        let mut kept: Vec<TargetCandidate> = Vec::with_capacity(limit);
        let mut exhausted = false;

        for _ in 0..MAX_SCANS {
            let rows = self
                .conversation_repo
                .list_mentionable_candidates(
                    user_id,
                    &MentionableCandidatesParams {
                        project_id: current_project_id.clone(),
                        filter_project_id: project_scope.clone(),
                        id: id_scope.clone(),
                        name_query: name_query.clone(),
                        limit: scan_limit,
                        offset,
                    },
                )
                .await
                .map_err(|error| SessionMessageError::TransportUnavailable {
                    reason: error.to_string(),
                })?;
            let scanned = rows.len() as u32;

            for row in rows {
                // Counts scanned rows, not kept ones, so the next page resumes
                // past the holes the hard filters just punched.
                offset += 1;
                if row.id == current_conversation_id {
                    continue;
                }
                if team_id_from_extra_str(&row.extra).is_some() {
                    continue;
                }
                kept.push(TargetCandidate {
                    id: row.id,
                    name: row.name,
                    project_id: row.project_id,
                    modified_at: row.updated_at,
                });
                if kept.len() == limit {
                    break;
                }
            }

            if kept.len() == limit {
                break;
            }
            // A short read is the only proof the table ran out.
            if scanned < scan_limit {
                exhausted = true;
                break;
            }
        }

        // Anything short of a proven-exhausted scan may have more behind it,
        // whether the page filled or `MAX_SCANS` cut it short.
        let next_cursor = (!exhausted).then(|| offset.to_string());
        let project_names = self.resolve_project_names(user_id, &kept).await;

        Ok(SessionMentionableResponse {
            items: kept
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
