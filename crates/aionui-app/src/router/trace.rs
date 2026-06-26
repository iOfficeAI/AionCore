//! HTTP request access-log layer.

use std::time::Instant;

use aionui_common::{ApiErrorLogContext, generate_short_id};
use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;

const REQUEST_ID_HEADER: &str = "x-request-id";
const MAX_QUERY_KEYS: usize = 16;

pub(super) fn with_access_log(router: Router) -> Router {
    router.layer(middleware::from_fn(access_log))
}

async fn access_log(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query_keys = query_keys(request.uri().query());
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(generate_short_id);

    let response = next.run(request).await;
    let status = response.status().as_u16();
    let latency_ms = started.elapsed().as_millis() as u64;
    let error_context = response.extensions().get::<ApiErrorLogContext>();
    let error_code = error_context.map(|context| context.code).unwrap_or("");
    let error_message = error_context.map(|context| context.message.as_str()).unwrap_or("");

    if status >= 500 {
        tracing::error!(
            request_id = %request_id,
            method = %method,
            path = %path,
            query_keys = %query_keys,
            status,
            latency_ms,
            error_code,
            error_message,
            "http response"
        );
    } else if status >= 400 {
        tracing::warn!(
            request_id = %request_id,
            method = %method,
            path = %path,
            query_keys = %query_keys,
            status,
            latency_ms,
            error_code,
            error_message,
            "http response"
        );
    } else {
        tracing::info!(
            request_id = %request_id,
            method = %method,
            path = %path,
            query_keys = %query_keys,
            status,
            latency_ms,
            "http response"
        );
    }

    response
}

fn query_keys(query: Option<&str>) -> String {
    query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key).trim();
            (!key.is_empty()).then_some(key)
        })
        .take(MAX_QUERY_KEYS)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_keys_omit_values() {
        assert_eq!(
            query_keys(Some("path=/Users/alice/project&token=secret&flag&empty=")),
            "path,token,flag,empty"
        );
    }
}
