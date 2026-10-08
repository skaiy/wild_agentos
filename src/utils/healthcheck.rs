//! `wild-agent-os-core healthcheck`: container health probe without curl.
//!
//! The runtime image has no shell and no curl, so Docker / compose health
//! checks exec the Core binary itself with the `healthcheck` argument. The
//! probe only requests `GET http://127.0.0.1:<port>/health` on the local
//! machine, bounded by [`HEALTHCHECK_TIMEOUT`], and maps the outcome to an
//! exit code (0 healthy, 1 unhealthy). It reads no secrets and never prints
//! environment variable values.

use std::time::Duration;

/// Command-line argument that selects the probe instead of starting Core.
pub const HEALTHCHECK_ARG: &str = "healthcheck";

/// Upper bound for the whole probe (connect + request + response).
pub const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);

const DEFAULT_HTTP_PORT: u16 = 8080;

/// Local health URL for the HTTP port Core listens on.
///
/// `port_var` is the raw `AGENT_OS_HTTP_PORT` value, parsed the same way as at
/// startup: a missing or unparsable value falls back to 8080.
pub fn health_url(port_var: Option<&str>) -> String {
    let port = port_var
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_HTTP_PORT);
    format!("http://127.0.0.1:{port}/health")
}

/// Requests `url` once; `Ok` only for HTTP 200.
///
/// Error strings describe the failure class only (status code, timeout,
/// connection failure); they never include request headers or environment.
pub async fn probe(url: &str, timeout: Duration) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .no_proxy()
        // Never follow redirects: only the local /health answer counts, and a
        // redirect must not send the probe to another host.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "unhealthy: failed to build HTTP client".to_string())?;
    let response = client.get(url).send().await.map_err(|error| {
        if error.is_timeout() {
            format!("unhealthy: no response within {}s", timeout.as_secs_f32())
        } else {
            "unhealthy: connection failed".to_string()
        }
    })?;
    let status = response.status();
    if status == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err(format!("unhealthy: /health returned {}", status.as_u16()))
    }
}

/// Runs the probe against the local Core and returns the process exit code.
pub async fn run() -> i32 {
    let port = std::env::var("AGENT_OS_HTTP_PORT").ok();
    match probe(&health_url(port.as_deref()), HEALTHCHECK_TIMEOUT).await {
        Ok(()) => {
            println!("healthy");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}/health")
    }

    #[test]
    fn health_url_uses_port_variable_or_default() {
        assert_eq!(health_url(None), "http://127.0.0.1:8080/health");
        assert_eq!(health_url(Some("9191")), "http://127.0.0.1:9191/health");
        assert_eq!(
            health_url(Some("not-a-port")),
            "http://127.0.0.1:8080/health"
        );
    }

    #[tokio::test]
    async fn probe_succeeds_on_200() {
        let url = serve(Router::new().route("/health", get(|| async { "ok" }))).await;
        assert_eq!(probe(&url, HEALTHCHECK_TIMEOUT).await, Ok(()));
    }

    #[tokio::test]
    async fn probe_fails_on_non_200() {
        let url = serve(Router::new().route(
            "/health",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down") }),
        ))
        .await;
        let error = probe(&url, HEALTHCHECK_TIMEOUT).await.unwrap_err();
        assert_eq!(error, "unhealthy: /health returned 503");
    }

    #[tokio::test]
    async fn probe_does_not_follow_redirects() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let target_hits = hits.clone();
        let url = serve(
            Router::new()
                .route(
                    "/health",
                    get(|| async { axum::response::Redirect::temporary("/elsewhere") }),
                )
                .route(
                    "/elsewhere",
                    get(move || {
                        let hits = target_hits.clone();
                        async move {
                            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            "ok"
                        }
                    }),
                ),
        )
        .await;
        let error = probe(&url, HEALTHCHECK_TIMEOUT).await.unwrap_err();
        assert_eq!(error, "unhealthy: /health returned 307");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn probe_fails_when_nothing_listens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let error = probe(&format!("http://{addr}/health"), HEALTHCHECK_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, "unhealthy: connection failed");
    }

    #[tokio::test]
    async fn probe_fails_on_timeout() {
        let url = serve(Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        ))
        .await;
        let started = std::time::Instant::now();
        let error = probe(&url, Duration::from_millis(300)).await.unwrap_err();
        assert!(
            error.starts_with("unhealthy: no response within"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn probe_errors_do_not_echo_environment_values() {
        let secret_like = "do-not-print-this-value";
        let url = serve(Router::new().route(
            "/health",
            get(move || async move { (StatusCode::INTERNAL_SERVER_ERROR, secret_like) }),
        ))
        .await;
        let error = probe(&url, HEALTHCHECK_TIMEOUT).await.unwrap_err();
        assert!(!error.contains(secret_like), "{error}");
        assert!(!error.contains("127.0.0.1"), "{error}");
    }
}
