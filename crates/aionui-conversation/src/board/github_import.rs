//! M8 E6-7: read-only GitHub Issue import into the board backlog (D6-6).
//!
//! - GitHub only, manual trigger, no scheduled sync.
//! - Idempotent by `external_ref = github:owner/name#N`: a re-import refreshes
//!   title / body / labels only and never touches `board_status`, assignee,
//!   rank or priority.
//! - Nothing is ever written back to GitHub. The fetch goes through a
//!   [`GithubIssueSource`] port; the app implements it with the user's own
//!   `gh` CLI login (`gh issue list`, a read-only command).

use std::sync::Arc;

use aionui_api_types::{
    CreateTaskSessionRequest, ImportGithubIssuesRequest, ImportGithubIssuesResponse, ImportedIssueItem,
    ImportedIssueOutcome, TaskBoardChange, TaskBoardStatus, TaskSessionMode, TaskSessionStatus,
    UpdateTaskSessionRequest,
};
use aionui_common::now_ms;
use aionui_db::{AppendTaskCommentParams, UpdateTaskBoardParams};

use super::{EVENT_TASK_COMMENT_CREATED, comment_response, item_response};
use crate::error::ConversationError;
use crate::service::ConversationService;

pub const DEFAULT_IMPORT_LIMIT: u32 = 30;
pub const MAX_IMPORT_LIMIT: u32 = 100;
const MAX_QUERY_CHARS: usize = 256;
const MAX_TITLE_CHARS: usize = 200;
/// Leaves room for the source line under the 20 000-char objective cap.
const MAX_BODY_CHARS: usize = 19_000;
/// `agent_type` recorded on imported items; the board never starts an
/// unassigned backlog item, and assignment picks the real runner.
pub const IMPORTED_AGENT_TYPE: &str = "acp";

/// One open issue as returned by the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GithubIssueSourceError {
    #[error("GitHub CLI (gh) is not installed")]
    CliMissing,
    #[error("GitHub CLI (gh) is not signed in")]
    NotAuthenticated,
    #[error("GitHub repository not found or not accessible: {0}")]
    RepoNotFound(String),
    #[error("GitHub request timed out")]
    Timeout,
    #[error("{0}")]
    Failed(String),
}

impl From<GithubIssueSourceError> for ConversationError {
    fn from(error: GithubIssueSourceError) -> Self {
        // Stable leading codes let clients localize the message. The
        // gateway / timeout text stays server-side (`ApiError::public_message`
        // hides it); the redacted detail is logged by the caller.
        match error {
            GithubIssueSourceError::CliMissing => ConversationError::Unprocessable {
                reason: "github_cli_missing: install the GitHub CLI (gh) and run `gh auth login`".into(),
            },
            GithubIssueSourceError::NotAuthenticated => ConversationError::Unprocessable {
                reason: "github_not_authenticated: run `gh auth login` first".into(),
            },
            GithubIssueSourceError::RepoNotFound(repo) => ConversationError::NotFoundReason {
                reason: format!("github_repo_not_found: {repo}"),
            },
            GithubIssueSourceError::Timeout => ConversationError::Timeout {
                reason: "github_import_timeout: gh issue list timed out".into(),
            },
            GithubIssueSourceError::Failed(message) => ConversationError::BadGateway {
                reason: format!("github_import_failed: {message}"),
            },
        }
    }
}

/// Read-only access to a repository's open issues.
#[async_trait::async_trait]
pub trait GithubIssueSource: Send + Sync {
    /// Open issues (never pull requests) of `repo` (`owner/name`), newest
    /// first, optionally narrowed by GitHub search qualifiers.
    async fn list_open_issues(
        &self,
        repo: &str,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<GithubIssue>, GithubIssueSourceError>;
}

/// Serializes imports so two concurrent imports of the same issue cannot
/// both create an item (the unique index would reject the second ref).
static IMPORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn owner_ok(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `owner/name`, `https://github.com/owner/name(.git)(/)` → lowercase
/// `owner/name` (GitHub names are case-insensitive).
pub fn normalize_repo(input: &str) -> Result<String, ConversationError> {
    let trimmed = input.trim();
    let path = ["https://github.com/", "http://github.com/", "github.com/"]
        .iter()
        .find_map(|prefix| {
            trimmed
                .get(..prefix.len())
                .filter(|head| head.eq_ignore_ascii_case(prefix))
                .map(|_| &trimmed[prefix.len()..])
        })
        .unwrap_or(trimmed);
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) if owner_ok(owner) && name_ok(name) => {
            Ok(format!("{}/{}", owner.to_ascii_lowercase(), name.to_ascii_lowercase()))
        }
        _ => Err(ConversationError::bad_request(
            "repo must be a GitHub repository as owner/name",
        )),
    }
}

fn normalize_query(query: Option<&str>) -> Result<Option<String>, ConversationError> {
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(None);
    };
    if query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control) {
        return Err(ConversationError::bad_request(format!(
            "query must be at most {MAX_QUERY_CHARS} characters on one line"
        )));
    }
    Ok(Some(query.to_owned()))
}

pub fn external_ref(repo: &str, number: u64) -> String {
    format!("github:{repo}#{number}")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn item_title(issue: &GithubIssue) -> String {
    let title = issue.title.trim();
    let title = if title.is_empty() {
        format!("Issue #{}", issue.number)
    } else {
        title.to_owned()
    };
    truncate(&title, MAX_TITLE_CHARS)
}

fn item_objective(issue: &GithubIssue) -> String {
    let body = truncate(issue.body.trim(), MAX_BODY_CHARS);
    if body.is_empty() {
        format!("GitHub: {}", issue.url)
    } else {
        format!("{body}\n\nGitHub: {}", issue.url)
    }
}

fn item_labels(issue: &GithubIssue) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for label in issue.labels.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        if !labels.iter().any(|existing| existing == label) {
            labels.push(label.to_owned());
        }
    }
    labels
}

impl ConversationService {
    pub fn with_github_issue_source(&self, source: Arc<dyn GithubIssueSource>) {
        if let Ok(mut guard) = self.github_issue_source.write() {
            *guard = Some(source);
        }
    }

    fn github_issue_source(&self) -> Result<Arc<dyn GithubIssueSource>, ConversationError> {
        self.github_issue_source
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| ConversationError::internal("GitHub issue import is not configured"))
    }

    /// `POST /api/task-sessions/import/github`.
    pub async fn import_github_issues(
        &self,
        user_id: &str,
        request: ImportGithubIssuesRequest,
    ) -> Result<ImportGithubIssuesResponse, ConversationError> {
        let repo = normalize_repo(&request.repo)?;
        let query = normalize_query(request.query.as_deref())?;
        let limit = request.limit.unwrap_or(DEFAULT_IMPORT_LIMIT);
        if !(1..=MAX_IMPORT_LIMIT).contains(&limit) {
            return Err(ConversationError::bad_request(format!(
                "limit must be between 1 and {MAX_IMPORT_LIMIT}"
            )));
        }
        let board = self.task_board_repo()?;
        let sessions = self.task_session_repo()?;
        let source = self.github_issue_source()?;
        let issues = source
            .list_open_issues(&repo, query.as_deref(), limit)
            .await
            .inspect_err(|error| tracing::warn!(%repo, %error, "GitHub issue import failed"))?;

        let _guard = IMPORT_LOCK.lock().await;
        let mut response = ImportGithubIssuesResponse {
            repo: repo.clone(),
            created: 0,
            updated: 0,
            unchanged: 0,
            items: Vec::with_capacity(issues.len()),
        };
        for issue in issues {
            let reference = external_ref(&repo, issue.number);
            let title = item_title(&issue);
            let objective = item_objective(&issue);
            let labels = item_labels(&issue);
            let (task_session_id, outcome) = match board.find_by_external_ref(user_id, &reference).await? {
                Some(row) => {
                    let current_labels: Vec<String> = serde_json::from_str(&row.labels).unwrap_or_default();
                    let session = sessions.get(user_id, &row.id).await?;
                    // The objective is frozen once an artifact exists
                    // (`update_task_session`); refresh what is still editable.
                    let objective_editable = sessions.list_artifacts(user_id, &row.id).await?.is_empty();
                    let title_changed = row.title != title;
                    let objective_changed =
                        objective_editable && session.as_ref().is_some_and(|session| session.objective != objective);
                    let labels_changed = current_labels != labels;
                    if title_changed || objective_changed {
                        self.update_task_session(
                            user_id,
                            &row.id,
                            UpdateTaskSessionRequest {
                                title: title_changed.then(|| title.clone()),
                                objective: objective_changed.then(|| objective.clone()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    }
                    if labels_changed {
                        board
                            .update_board(
                                user_id,
                                &row.id,
                                &UpdateTaskBoardParams {
                                    labels: Some(&labels),
                                    updated_at: now_ms(),
                                    ..Default::default()
                                },
                            )
                            .await?;
                    }
                    let outcome = if title_changed || objective_changed || labels_changed {
                        ImportedIssueOutcome::Updated
                    } else {
                        ImportedIssueOutcome::Unchanged
                    };
                    (row.id, outcome)
                }
                None => {
                    let session = self
                        .create_task_session(
                            user_id,
                            CreateTaskSessionRequest {
                                title: title.clone(),
                                project_id: None,
                                conversation_id: None,
                                mode: TaskSessionMode::Agent,
                                objective: objective.clone(),
                                acceptance_criteria: Vec::new(),
                                status: TaskSessionStatus::Draft,
                                agent_type: IMPORTED_AGENT_TYPE.to_owned(),
                                agent_session_id: None,
                            },
                        )
                        .await?;
                    board
                        .set_external_ref(user_id, &session.id, &reference, now_ms())
                        .await?;
                    if !labels.is_empty() {
                        board
                            .update_board(
                                user_id,
                                &session.id,
                                &UpdateTaskBoardParams {
                                    labels: Some(&labels),
                                    updated_at: now_ms(),
                                    ..Default::default()
                                },
                            )
                            .await?;
                    }
                    self.record_import_origin(user_id, &session.id, &repo, &issue).await;
                    (session.id, ImportedIssueOutcome::Created)
                }
            };
            match outcome {
                ImportedIssueOutcome::Created => response.created += 1,
                ImportedIssueOutcome::Updated => response.updated += 1,
                ImportedIssueOutcome::Unchanged => response.unchanged += 1,
            }
            if outcome != ImportedIssueOutcome::Unchanged
                && let Some(row) = board.get_board_item(user_id, &task_session_id).await?
            {
                let from = TaskBoardStatus::parse(&row.board_status).unwrap_or(TaskBoardStatus::Backlog);
                let item = item_response(row)?;
                self.publish_board_updated(user_id, TaskBoardChange::Import, from, &item);
            }
            response.items.push(ImportedIssueItem {
                external_ref: reference,
                task_session_id,
                number: issue.number,
                title,
                url: issue.url,
                outcome,
            });
        }
        tracing::info!(
            repo = %response.repo,
            created = response.created,
            updated = response.updated,
            unchanged = response.unchanged,
            "GitHub issues imported"
        );
        Ok(response)
    }

    async fn record_import_origin(&self, user_id: &str, task_session_id: &str, repo: &str, issue: &GithubIssue) {
        let body = format!(
            "从 GitHub Issue {repo}#{} 只读导入（不会回写 GitHub）：{}",
            issue.number, issue.url
        );
        let Ok(board) = self.task_board_repo() else {
            return;
        };
        match board
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id,
                author_kind: "system",
                author_id: "github-import",
                kind: "comment",
                body: &body,
                created_at: now_ms(),
            })
            .await
        {
            Ok(comment) => self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment_response(comment)),
            Err(error) => tracing::warn!(task_session_id, %error, "failed to record GitHub import origin"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_forms_normalize_to_lowercase_owner_name() {
        for input in [
            "Suoak/AionCore",
            " suoak/aioncore ",
            "https://github.com/Suoak/AionCore",
            "https://github.com/suoak/AionCore.git",
            "github.com/suoak/aioncore/",
        ] {
            assert_eq!(normalize_repo(input).unwrap(), "suoak/aioncore", "{input}");
        }
    }

    #[test]
    fn invalid_repos_are_rejected() {
        for input in [
            "",
            "suoak",
            "suoak/aioncore/issues",
            "-x/y",
            "a/..",
            "a/b c",
            "https://gitlab.com/a/b",
            "a/b;rm",
            "--repo/x",
        ] {
            assert!(normalize_repo(input).is_err(), "{input}");
        }
    }

    #[test]
    fn query_is_trimmed_and_bounded() {
        assert_eq!(normalize_query(Some("  ")).unwrap(), None);
        assert_eq!(
            normalize_query(Some(" label:bug ")).unwrap().as_deref(),
            Some("label:bug")
        );
        assert!(normalize_query(Some("a\nb")).is_err());
        assert!(normalize_query(Some(&"x".repeat(MAX_QUERY_CHARS + 1))).is_err());
    }

    #[test]
    fn issue_fields_map_to_work_item_fields() {
        let issue = GithubIssue {
            number: 7,
            title: "  ".into(),
            body: "".into(),
            labels: vec![" bug ".into(), "bug".into(), "".into(), "ui".into()],
            url: "https://github.com/a/b/issues/7".into(),
        };
        assert_eq!(item_title(&issue), "Issue #7");
        assert_eq!(item_objective(&issue), "GitHub: https://github.com/a/b/issues/7");
        assert_eq!(item_labels(&issue), vec!["bug".to_owned(), "ui".to_owned()]);
        let long = GithubIssue {
            title: "t".repeat(300),
            body: "b".repeat(30_000),
            ..issue
        };
        assert_eq!(item_title(&long).chars().count(), MAX_TITLE_CHARS);
        assert!(item_objective(&long).chars().count() <= 20_000);
        assert_eq!(external_ref("a/b", 7), "github:a/b#7");
    }
}
