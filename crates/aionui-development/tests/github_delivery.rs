use std::path::Path;
use std::sync::{Arc, Mutex};

use aionui_development::{
    DeliveryProvider, DeliveryProviderRegistry, DeliveryProviderSnapshot, ProviderCiCheck, ProviderPullRequest,
    ProviderTag,
};
use async_trait::async_trait;

#[derive(Default)]
struct ContractProvider {
    name: &'static str,
    calls: Mutex<Vec<String>>,
    tags: Mutex<Vec<(String, String)>>,
}

impl ContractProvider {
    fn named(name: &'static str) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }
}

#[async_trait]
impl DeliveryProvider for ContractProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn preflight(&self, _repository: &Path) -> Result<(), String> {
        self.calls.lock().unwrap().push("preflight".into());
        Ok(())
    }

    async fn push(&self, _repository: &Path, branch: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("push:{branch}"));
        Ok(())
    }

    async fn ensure_pull_request(
        &self,
        _repository: &Path,
        head: &str,
        base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<ProviderPullRequest, String> {
        self.calls.lock().unwrap().push(format!("change:{head}:{base}"));
        Ok(ProviderPullRequest {
            number: 17,
            url: format!("https://{}.example/changes/17", self.name),
            status: "open".into(),
            review_status: "approved".into(),
        })
    }

    async fn synchronize(&self, _repository: &Path, number: i64) -> Result<DeliveryProviderSnapshot, String> {
        self.calls.lock().unwrap().push(format!("sync:{number}"));
        Ok(DeliveryProviderSnapshot {
            pull_request: ProviderPullRequest {
                number,
                url: format!("https://{}.example/changes/{number}", self.name),
                status: "open".into(),
                review_status: "approved".into(),
            },
            checks: vec![ProviderCiCheck {
                id: "unit".into(),
                name: "unit".into(),
                status: "passed".into(),
                details_url: None,
                summary: None,
            }],
            review_comments: vec![],
        })
    }

    async fn merge(&self, _repository: &Path, number: i64) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("merge:{number}"));
        Ok(())
    }

    async fn ensure_tag(&self, _repository: &Path, tag: &str, commit: &str) -> Result<ProviderTag, String> {
        let mut tags = self.tags.lock().unwrap();
        if !tags.iter().any(|existing| existing.0 == tag) {
            tags.push((tag.into(), commit.into()));
        }
        Ok(ProviderTag {
            name: tag.into(),
            commit_sha: commit.into(),
            remote_url: Some(format!("https://{}.example/tags/{tag}", self.name)),
        })
    }
}

async fn assert_provider_contract(provider: Arc<ContractProvider>) {
    let registry = DeliveryProviderRegistry::new(provider.clone());
    let selected = registry.get(provider.name()).unwrap();
    let repository = Path::new(".");
    selected.preflight(repository).await.unwrap();
    selected.push(repository, "aion/run/contract").await.unwrap();
    let change = selected
        .ensure_pull_request(repository, "aion/run/contract", "main", "Contract", "Evidence")
        .await
        .unwrap();
    let snapshot = selected.synchronize(repository, change.number).await.unwrap();
    assert_eq!(snapshot.checks[0].status, "passed");
    selected.merge(repository, change.number).await.unwrap();
    let first = selected.ensure_tag(repository, "v1.2.3", "abc123").await.unwrap();
    let second = selected.ensure_tag(repository, "v1.2.3", "abc123").await.unwrap();
    assert_eq!(first, second);
    assert_eq!(provider.tags.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn github_implements_the_delivery_contract() {
    assert_provider_contract(Arc::new(ContractProvider::named("github"))).await;
}

#[test]
fn repository_urls_select_only_github() {
    let registry = DeliveryProviderRegistry::new(Arc::new(ContractProvider::named("github")));

    assert_eq!(
        registry
            .name_for_repository(Some("git@github.com:acme/app.git"))
            .unwrap(),
        "github"
    );
    let unsupported = registry
        .name_for_repository(Some("https://example.invalid/acme/app.git"))
        .unwrap_err();
    assert_eq!(
        unsupported.to_string(),
        "Invalid development request: repository host is not a supported GitHub provider"
    );
}
