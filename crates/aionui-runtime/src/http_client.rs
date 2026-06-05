use std::time::Duration;

const DEFAULT_RUNTIME_USER_AGENT: &str = concat!("aioncore/", env!("CARGO_PKG_VERSION"));

pub fn build_http_client(connect_timeout: Duration, timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .user_agent(DEFAULT_RUNTIME_USER_AGENT)
        .build()
        .map_err(|error| format!("build http client: {error}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::build_http_client;

    #[test]
    fn build_http_client_succeeds_with_runtime_defaults() {
        let _client = build_http_client(Duration::from_secs(1), Duration::from_secs(1)).expect("client");
    }
}
