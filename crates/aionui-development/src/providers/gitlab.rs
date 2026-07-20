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
pub struct GitLabCliDeliveryProvider;

#[async_trait]
impl DeliveryProvider for GitLabCliDeliveryProvider {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    async fn preflight(&self, repository: &Path) -> Result<(), String> {
        Repository::open(repository).map_err(|error| format!("Git repository unavailable: {error}"))?;
        checked_output("glab", &["auth", "status"], repository)
            .await
            .map(|_| ())
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
                "glab",
                &[
                    "mr",
                    "list",
                    "--source-branch",
                    head,
                    "--target-branch",
                    base,
                    "--state",
                    "opened",
                    "--output",
                    "json",
                ],
                repository,
            )
            .await?,
        )
        .map_err(|error| format!("cannot parse GitLab merge request list: {error}"))?;
        if let Some(value) = existing.as_array().and_then(|values| values.first()) {
            return parse_merge_request(value);
        }

        checked_output(
            "glab",
            &[
                "mr",
                "create",
                "--source-branch",
                head,
                "--target-branch",
                base,
                "--title",
                title,
                "--description",
                body,
                "--yes",
            ],
            repository,
        )
        .await?;

        let value: Value = serde_json::from_slice(
            &checked_output("glab", &["mr", "view", head, "--output", "json"], repository).await?,
        )
        .map_err(|error| format!("cannot parse created GitLab merge request: {error}"))?;
        parse_merge_request(&value)
    }

    async fn synchronize(&self, repository: &Path, number: i64) -> Result<DeliveryProviderSnapshot, String> {
        let number = number.to_string();
        let value: Value = serde_json::from_slice(
            &checked_output("glab", &["mr", "view", &number, "--output", "json"], repository).await?,
        )
        .map_err(|error| format!("cannot parse GitLab merge request status: {error}"))?;
        let pull_request = parse_merge_request(&value)?;
        let checks = if let Some(pipelines) = value.get("pipelines").and_then(Value::as_array) {
            pipelines.iter().map(parse_pipeline).collect()
        } else {
            value.get("head_pipeline").into_iter().map(parse_pipeline).collect()
        };
        let review_comments = parse_review_comments(&value);
        Ok(DeliveryProviderSnapshot {
            pull_request,
            checks,
            review_comments,
        })
    }

    async fn merge(&self, repository: &Path, number: i64) -> Result<(), String> {
        checked_output("glab", &["mr", "merge", &number.to_string(), "--yes"], repository)
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

fn parse_merge_request(value: &Value) -> Result<ProviderPullRequest, String> {
    let number = value
        .get("iid")
        .or_else(|| value.get("number"))
        .and_then(Value::as_i64)
        .ok_or_else(|| "GitLab response has no merge request IID".to_owned())?;
    let approved = value
        .get("approved")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| value.get("approval_status").and_then(Value::as_str) == Some("approved"));
    Ok(ProviderPullRequest {
        number,
        url: value
            .get("web_url")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        status: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("opened")
            .to_ascii_lowercase(),
        review_status: if approved { "approved" } else { "pending" }.into(),
    })
}

fn parse_pipeline(value: &Value) -> ProviderCiCheck {
    let name = value
        .get("name")
        .or_else(|| value.get("ref"))
        .and_then(Value::as_str)
        .unwrap_or("GitLab pipeline")
        .to_owned();
    let url = value.get("web_url").and_then(Value::as_str).map(str::to_owned);
    let raw_status = value.get("status").and_then(Value::as_str).unwrap_or("pending");
    let status = match raw_status {
        "success" => "passed",
        "failed" | "canceled" => "failed",
        "running" => "in_progress",
        _ => "queued",
    };
    let source = format!(
        "{}:{}",
        value.get("id").and_then(Value::as_i64).unwrap_or_default(),
        name
    );
    let digest = Sha256::digest(source.as_bytes());
    ProviderCiCheck {
        id: format!("gitlab-{}", hex_prefix(&digest, 24)),
        name,
        status: status.into(),
        details_url: url,
        summary: value.get("detailed_status").and_then(Value::as_str).map(str::to_owned),
    }
}

fn parse_review_comments(value: &Value) -> Vec<ProviderReviewComment> {
    value
        .get("discussions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|discussion| {
            let resolved = discussion.get("resolved").and_then(Value::as_bool).unwrap_or(false);
            discussion
                .get("notes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |note| {
                    if note.get("system").and_then(Value::as_bool).unwrap_or(false) {
                        return None;
                    }
                    let body = note.get("body").and_then(Value::as_str)?.trim();
                    let id = note.get("id")?.as_i64()?.to_string();
                    Some(ProviderReviewComment {
                        id,
                        body: body.into(),
                        url: note.get("web_url").and_then(Value::as_str).map(str::to_owned),
                        author: note
                            .get("author")
                            .and_then(|author| author.get("username"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        resolved,
                    })
                })
        })
        .collect()
}

fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty()
        || branch == "main"
        || branch == "master"
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

    use super::{parse_merge_request, parse_pipeline, parse_review_comments};

    #[test]
    fn parses_typed_gitlab_delivery_snapshot() {
        let value = json!({
            "iid": 17,
            "web_url": "https://gitlab.example/merge_requests/17",
            "state": "opened",
            "approval_status": "approved",
            "discussions": [{
                "resolved": false,
                "notes": [{
                    "id": 91,
                    "body": "Handle the failure path",
                    "web_url": "https://gitlab.example/merge_requests/17#note_91",
                    "author": {"username": "reviewer"},
                    "system": false
                }]
            }]
        });
        let merge_request = parse_merge_request(&value).unwrap();
        assert_eq!(merge_request.number, 17);
        assert_eq!(merge_request.review_status, "approved");
        let comments = parse_review_comments(&value);
        assert_eq!(comments.len(), 1);
        assert!(!comments[0].resolved);
        assert_eq!(
            parse_pipeline(&json!({"id": 1, "name": "test", "status": "success"})).status,
            "passed"
        );
    }
}
