use std::path::Path;
use std::time::Duration;

use aionui_api_types::{
    OfficialSkillDetail, OfficialSkillFile, OfficialSkillInstallStatus, OfficialSkillSearchQuery,
    OfficialSkillSearchResponse, OfficialSkillSummary, OfficialSkillVersionResponse,
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::warn;

const DEFAULT_REGISTRY_URL: &str = "http://10.51.134.12";
const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SkillHubClientError {
    #[error("SkillHub request timed out")]
    Timeout,
    #[error("SkillHub is unavailable")]
    Unavailable,
    #[error("SkillHub returned an invalid response")]
    InvalidResponse,
    #[error("SkillHub resource was not found")]
    NotFound,
    #[error("SkillHub package exceeds the configured size limit")]
    PackageTooLarge,
    #[error("Failed to persist the SkillHub package")]
    Io,
}

#[derive(Clone)]
pub struct SkillHubClient {
    base_url: String,
    client: reqwest::Client,
    download_client: reqwest::Client,
}

impl SkillHubClient {
    pub fn production() -> Result<Self, SkillHubClientError> {
        Self::build(DEFAULT_REGISTRY_URL, Duration::from_secs(15), Duration::from_secs(60))
    }

    fn build(
        base_url: impl Into<String>,
        request_timeout: Duration,
        download_timeout: Duration,
    ) -> Result<Self, SkillHubClientError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("csbu-workmate/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| SkillHubClientError::Unavailable)?;
        let download_client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(download_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("csbu-workmate/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| SkillHubClientError::Unavailable)?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
            download_client,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        base_url: impl Into<String>,
        request_timeout: Duration,
    ) -> Result<Self, SkillHubClientError> {
        Self::build(base_url, request_timeout, request_timeout)
    }

    pub async fn search(
        &self,
        query: &OfficialSkillSearchQuery,
    ) -> Result<OfficialSkillSearchResponse, SkillHubClientError> {
        let url = format!("{}/api/web/skills", self.base_url);
        let page = query.page.to_string();
        let size = query.size.to_string();
        let response = self
            .client
            .get(url)
            .query(&[
                ("q", query.q.as_str()),
                ("sort", query.sort.as_str()),
                ("page", page.as_str()),
                ("size", size.as_str()),
                ("namespace", "global"),
                ("visibility", "PUBLIC"),
                ("status", "ACTIVE"),
            ])
            .send()
            .await
            .map_err(|error| map_reqwest_error("search", error))?;
        let envelope: Envelope<SearchData> = decode_response(response).await?;
        Ok(OfficialSkillSearchResponse {
            items: envelope
                .data
                .items
                .into_iter()
                .filter_map(RawSkillSummary::into_public_summary)
                .collect(),
            total: envelope.data.total,
            page: envelope.data.page,
            size: envelope.data.size,
        })
    }

    pub async fn detail(&self, namespace: &str, slug: &str) -> Result<OfficialSkillDetail, SkillHubClientError> {
        let url = format!(
            "{}/api/web/skills/{}/{}",
            self.base_url,
            encode_segment(namespace),
            encode_segment(slug)
        );
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| map_reqwest_error("detail", error))?;
        let envelope: Envelope<RawSkillDetail> = decode_response(response).await?;
        envelope.data.into_public_detail()
    }

    pub async fn files(
        &self,
        namespace: &str,
        slug: &str,
        version: &str,
    ) -> Result<Vec<OfficialSkillFile>, SkillHubClientError> {
        let url = format!(
            "{}/api/web/skills/{}/{}/versions/{}/files",
            self.base_url,
            encode_segment(namespace),
            encode_segment(slug),
            encode_segment(version)
        );
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| map_reqwest_error("files", error))?;
        let envelope: Envelope<Vec<RawSkillFile>> = decode_response(response).await?;
        Ok(envelope.data.into_iter().map(Into::into).collect())
    }

    pub async fn download(
        &self,
        namespace: &str,
        slug: &str,
        version: &str,
        destination: &Path,
    ) -> Result<(), SkillHubClientError> {
        let url = format!(
            "{}/api/web/skills/{}/{}/versions/{}/download",
            self.base_url,
            encode_segment(namespace),
            encode_segment(slug),
            encode_segment(version)
        );
        let mut response = self
            .download_client
            .get(url)
            .send()
            .await
            .map_err(|error| map_reqwest_error("download", error))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SkillHubClientError::NotFound);
        }
        if !response.status().is_success() {
            return Err(SkillHubClientError::Unavailable);
        }
        if response.content_length().is_some_and(|size| size > MAX_ARCHIVE_BYTES) {
            return Err(SkillHubClientError::PackageTooLarge);
        }
        let mut file = tokio::fs::File::create(destination)
            .await
            .map_err(|_| SkillHubClientError::Io)?;
        let mut received = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| map_reqwest_error("download_stream", error))?
        {
            received = received.saturating_add(chunk.len() as u64);
            if received > MAX_ARCHIVE_BYTES {
                return Err(SkillHubClientError::PackageTooLarge);
            }
            file.write_all(&chunk).await.map_err(|_| SkillHubClientError::Io)?;
        }
        file.flush().await.map_err(|_| SkillHubClientError::Io)
    }
}

fn encode_segment(value: &str) -> String {
    value.to_owned()
}

fn map_reqwest_error(stage: &'static str, error: reqwest::Error) -> SkillHubClientError {
    warn!(
        stage = stage,
        http_status = error
            .status()
            .map_or_else(|| "unknown".to_owned(), |status| status.to_string()),
        request_id = "unknown",
        error_kind = if error.is_timeout() { "timeout" } else { "transport" },
        "SkillHub request failed"
    );
    if error.is_timeout() {
        SkillHubClientError::Timeout
    } else {
        SkillHubClientError::Unavailable
    }
}

async fn decode_response<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T, SkillHubClientError> {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        warn!(stage = "response", http_status = %status, request_id = %request_id, "SkillHub request failed");
        return Err(SkillHubClientError::NotFound);
    }
    if !response.status().is_success() {
        warn!(stage = "response", http_status = %status, request_id = %request_id, "SkillHub request failed");
        return Err(SkillHubClientError::Unavailable);
    }
    response.json().await.map_err(|_| {
        warn!(stage = "decode", http_status = %status, request_id = %request_id, "SkillHub response decoding failed");
        SkillHubClientError::InvalidResponse
    })
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchData {
    items: Vec<RawSkillSummary>,
    total: u64,
    page: u32,
    size: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVersion {
    id: i64,
    version: String,
    status: String,
}

impl From<RawVersion> for OfficialSkillVersionResponse {
    fn from(value: RawVersion) -> Self {
        Self {
            id: value.id,
            version: value.version,
            status: value.status,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSkillSummary {
    id: i64,
    slug: String,
    display_name: String,
    #[serde(default)]
    summary: String,
    visibility: String,
    status: String,
    download_count: i64,
    star_count: i64,
    namespace: String,
    updated_at: String,
    published_version: Option<RawVersion>,
}

impl RawSkillSummary {
    fn into_public_summary(self) -> Option<OfficialSkillSummary> {
        let published_version = self.published_version?;
        if self.namespace != "global"
            || self.visibility != "PUBLIC"
            || self.status != "ACTIVE"
            || published_version.status != "PUBLISHED"
        {
            return None;
        }
        Some(OfficialSkillSummary {
            id: self.id,
            namespace: self.namespace,
            slug: self.slug,
            display_name: self.display_name,
            summary: self.summary,
            download_count: self.download_count,
            star_count: self.star_count,
            updated_at: self.updated_at,
            published_version: published_version.into(),
            install_status: OfficialSkillInstallStatus::NotInstalled,
            installed_version: None,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLabel {
    slug: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSkillDetail {
    id: i64,
    slug: String,
    display_name: String,
    #[serde(default)]
    owner_display_name: String,
    #[serde(default)]
    summary: String,
    visibility: String,
    status: String,
    download_count: i64,
    star_count: i64,
    namespace: String,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    updated_at: String,
    published_version: Option<RawVersion>,
}

impl RawSkillDetail {
    fn into_public_detail(self) -> Result<OfficialSkillDetail, SkillHubClientError> {
        let summary = RawSkillSummary {
            id: self.id,
            slug: self.slug,
            display_name: self.display_name,
            summary: self.summary,
            visibility: self.visibility,
            status: self.status,
            download_count: self.download_count,
            star_count: self.star_count,
            namespace: self.namespace,
            updated_at: self.updated_at,
            published_version: self.published_version,
        }
        .into_public_summary()
        .ok_or(SkillHubClientError::NotFound)?;
        Ok(OfficialSkillDetail {
            skill: summary,
            owner_display_name: self.owner_display_name,
            labels: self.labels.into_iter().map(|label| label.slug).collect(),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSkillFile {
    id: i64,
    file_path: String,
    file_size: u64,
    #[serde(default)]
    content_type: String,
    sha256: String,
}

impl From<RawSkillFile> for OfficialSkillFile {
    fn from(value: RawSkillFile) -> Self {
        Self {
            id: value.id,
            file_path: value.file_path,
            file_size: value.file_size,
            content_type: value.content_type,
            sha256: value.sha256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};
    use serde_json::json;

    async fn fixture(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn query() -> OfficialSkillSearchQuery {
        OfficialSkillSearchQuery {
            q: String::new(),
            sort: "newest".into(),
            page: 0,
            size: 20,
        }
    }

    #[tokio::test]
    async fn search_normalizes_and_filters_non_public_skills() {
        let body = json!({
            "data": {
                "items": [
                    {
                        "id": 1, "slug": "public-skill", "displayName": "Public Skill", "summary": "Safe",
                        "visibility": "PUBLIC", "status": "ACTIVE", "downloadCount": 4, "starCount": 2,
                        "namespace": "global", "updatedAt": "2026-01-01T00:00:00Z",
                        "publishedVersion": { "id": 10, "version": "1.0", "status": "PUBLISHED" }
                    },
                    {
                        "id": 2, "slug": "private-skill", "displayName": "Private Skill", "summary": "Hidden",
                        "visibility": "PRIVATE", "status": "ACTIVE", "downloadCount": 0, "starCount": 0,
                        "namespace": "global", "updatedAt": "2026-01-01T00:00:00Z",
                        "publishedVersion": { "id": 20, "version": "1.0", "status": "PUBLISHED" }
                    }
                ],
                "total": 2, "page": 0, "size": 20
            }
        });
        let base = fixture(Router::new().route(
            "/api/web/skills",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        ))
        .await;
        let result = SkillHubClient::for_test(base, Duration::from_secs(1))
            .unwrap()
            .search(&query())
            .await
            .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].slug, "public-skill");
    }

    #[tokio::test]
    async fn search_rejects_invalid_json_and_upstream_errors() {
        let invalid = fixture(Router::new().route("/api/web/skills", get(|| async { "not json" }))).await;
        assert!(matches!(
            SkillHubClient::for_test(invalid, Duration::from_secs(1))
                .unwrap()
                .search(&query())
                .await,
            Err(SkillHubClientError::InvalidResponse)
        ));

        let failed = fixture(Router::new().route(
            "/api/web/skills",
            get(|| async { (StatusCode::BAD_GATEWAY, "secret upstream body").into_response() }),
        ))
        .await;
        assert!(matches!(
            SkillHubClient::for_test(failed, Duration::from_secs(1))
                .unwrap()
                .search(&query())
                .await,
            Err(SkillHubClientError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn search_maps_request_timeout() {
        let base = fixture(Router::new().route(
            "/api/web/skills",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Json(json!({ "data": { "items": [], "total": 0, "page": 0, "size": 20 } }))
            }),
        ))
        .await;
        assert!(matches!(
            SkillHubClient::for_test(base, Duration::from_millis(20))
                .unwrap()
                .search(&query())
                .await,
            Err(SkillHubClientError::Timeout)
        ));
    }
}
