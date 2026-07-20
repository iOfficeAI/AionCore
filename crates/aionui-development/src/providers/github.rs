use std::path::Path;

use aionui_runtime::Builder;
use async_trait::async_trait;
use git2::Repository;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::delivery::{
    DeliveryProvider, DeliveryProviderSnapshot, ProviderCiCheck, ProviderPullRequest, ProviderReviewComment,
    ProviderTag, validate_tag_name,
};

#[derive(Debug, Clone, Default)]
pub struct GitHubCliDeliveryProvider;

#[async_trait]
impl DeliveryProvider for GitHubCliDeliveryProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn preflight(&self, repository: &Path) -> Result<(), String> {
        Repository::open(repository).map_err(|error| format!("Git repository unavailable: {error}"))?;
        checked_output("gh", &["auth", "status"], repository).await.map(|_| ())
    }

    async fn push(&self, repository: &Path, branch: &str) -> Result<(), String> {
        validate_branch(branch)?;
        checked_output("git", &["push", "--set-upstream", "origin", branch], repository)
            .await
            .map(|_| ())
    }

    async fn ensure_pull_request(
        &self,
        repository: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ProviderPullRequest, String> {
        let existing: Value = serde_json::from_slice(
            &checked_output(
                "gh",
                &[
                    "pr",
                    "list",
                    "--head",
                    head,
                    "--base",
                    base,
                    "--state",
                    "open",
                    "--json",
                    "number,url,state,reviewDecision",
                ],
                repository,
            )
            .await?,
        )
        .map_err(|error| format!("cannot parse GitHub pull request list: {error}"))?;
        if let Some(value) = existing.as_array().and_then(|values| values.first()) {
            return parse_pull_request(value);
        }

        checked_output(
            "gh",
            &[
                "pr", "create", "--base", base, "--head", head, "--title", title, "--body", body,
            ],
            repository,
        )
        .await?;
        let value: Value = serde_json::from_slice(
            &checked_output(
                "gh",
                &["pr", "view", head, "--json", "number,url,state,reviewDecision"],
                repository,
            )
            .await?,
        )
        .map_err(|error| format!("cannot parse created GitHub pull request: {error}"))?;
        parse_pull_request(&value)
    }

    async fn synchronize(&self, repository: &Path, number: i64) -> Result<DeliveryProviderSnapshot, String> {
        let number = number.to_string();
        let value: Value = serde_json::from_slice(
            &checked_output(
                "gh",
                &[
                    "pr",
                    "view",
                    &number,
                    "--json",
                    "number,url,state,reviewDecision,statusCheckRollup,comments,reviews",
                ],
                repository,
            )
            .await?,
        )
        .map_err(|error| format!("cannot parse GitHub pull request status: {error}"))?;
        let pull_request = parse_pull_request(&value)?;
        let checks = value
            .get("statusCheckRollup")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(parse_ci_check)
            .collect();
        Ok(DeliveryProviderSnapshot {
            pull_request,
            checks,
            review_comments: parse_review_comments(&value),
        })
    }

    async fn merge(&self, repository: &Path, number: i64) -> Result<(), String> {
        checked_output("gh", &["pr", "merge", &number.to_string(), "--merge"], repository)
            .await
            .map(|_| ())
    }

    async fn ensure_tag(&self, repository: &Path, tag: &str, commit: &str) -> Result<ProviderTag, String> {
        validate_tag_name(tag).map_err(|error| error.to_string())?;
        {
            let handle = Repository::open(repository).map_err(|error| error.to_string())?;
            let object = handle.revparse_single(commit).map_err(|error| error.to_string())?;
            if let Ok(existing) = handle.revparse_single(&format!("refs/tags/{tag}")) {
                if existing.id() != object.id() {
                    return Err("tag already points to a different commit".into());
                }
            } else {
                handle
                    .tag_lightweight(tag, &object, false)
                    .map_err(|error| error.to_string())?;
            }
        }
        checked_output("git", &["push", "origin", &format!("refs/tags/{tag}")], repository).await?;
        Ok(ProviderTag {
            name: tag.into(),
            commit_sha: commit.into(),
            remote_url: None,
        })
    }
}

async fn checked_output(program: &str, arguments: &[&str], directory: &Path) -> Result<Vec<u8>, String> {
    let mut command = Builder::clean_cli(program);
    command.args(arguments).current_dir(directory);
    let output = command.output().await.map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn parse_pull_request(value: &Value) -> Result<ProviderPullRequest, String> {
    let number = value
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| "GitHub response has no pull request number".to_string())?;
    Ok(ProviderPullRequest {
        number,
        url: value.get("url").and_then(Value::as_str).unwrap_or_default().into(),
        status: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("OPEN")
            .to_ascii_lowercase(),
        review_status: match value.get("reviewDecision").and_then(Value::as_str).unwrap_or_default() {
            "APPROVED" => "approved",
            "CHANGES_REQUESTED" => "changes_requested",
            _ => "pending",
        }
        .into(),
    })
}

fn parse_ci_check(value: &Value) -> ProviderCiCheck {
    let name = value
        .get("name")
        .or_else(|| value.get("workflowName"))
        .and_then(Value::as_str)
        .unwrap_or("GitHub check")
        .to_owned();
    let details_url = value
        .get("detailsUrl")
        .or_else(|| value.get("targetUrl"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let raw_status = value
        .get("conclusion")
        .and_then(Value::as_str)
        .filter(|status| !status.is_empty())
        .or_else(|| value.get("status").and_then(Value::as_str))
        .unwrap_or("QUEUED");
    let digest = Sha256::digest(format!("{name}:{}", details_url.as_deref().unwrap_or_default()).as_bytes());
    ProviderCiCheck {
        id: format!("github-{}", hex_prefix(&digest, 24)),
        name,
        status: normalize_check_status(raw_status),
        details_url,
        summary: value.get("description").and_then(Value::as_str).map(str::to_owned),
    }
}

fn parse_review_comments(value: &Value) -> Vec<ProviderReviewComment> {
    value
        .get("comments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(value.get("reviews").and_then(Value::as_array).into_iter().flatten())
        .filter_map(|comment| {
            let body = comment.get("body").and_then(Value::as_str)?.trim();
            if body.is_empty() {
                return None;
            }
            let id = comment.get("id").and_then(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| value.as_i64().map(|id| id.to_string()))
            })?;
            Some(ProviderReviewComment {
                id,
                body: body.into(),
                url: comment.get("url").and_then(Value::as_str).map(str::to_owned),
                author: comment
                    .get("author")
                    .and_then(|author| author.get("login"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                resolved: comment.get("isResolved").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

fn normalize_check_status(status: &str) -> String {
    match status.to_ascii_lowercase().as_str() {
        "success" | "passed" => "passed",
        "failure" | "failed" | "error" | "timed_out" | "action_required" => "failed",
        "cancelled" => "cancelled",
        "skipped" | "neutral" => "skipped",
        "in_progress" | "pending" => "in_progress",
        _ => "queued",
    }
    .into()
}

fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty()
        || matches!(branch, "main" | "master")
        || branch.contains("..")
        || branch.chars().any(|value| value.is_whitespace() || value.is_control())
    {
        return Err("unsafe delivery branch".into());
    }
    Ok(())
}

fn hex_prefix(bytes: &[u8], length: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| format!("{byte:02x}").chars().collect::<Vec<_>>())
        .take(length)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_ci_check, parse_pull_request, parse_review_comments};

    #[test]
    fn parses_typed_github_delivery_snapshot() {
        let value = json!({
            "number": 42,
            "url": "https://github.example/pull/42",
            "state": "OPEN",
            "reviewDecision": "CHANGES_REQUESTED",
            "comments": [{
                "id": "IC_1",
                "body": "Please add a regression test",
                "url": "https://github.example/pull/42#comment-1",
                "author": {"login": "reviewer"}
            }]
        });
        let pull_request = parse_pull_request(&value).unwrap();
        assert_eq!(pull_request.number, 42);
        assert_eq!(pull_request.review_status, "changes_requested");
        let comments = parse_review_comments(&value);
        assert_eq!(comments.len(), 1);
        assert!(!comments[0].resolved);
        assert_eq!(
            parse_ci_check(&json!({"name": "unit", "conclusion": "SUCCESS"})).status,
            "passed"
        );
    }
}
