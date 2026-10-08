//! Outbound guard for the provider probe routes (#267 / #303).
//!
//! `POST /api/v1/providers/models` takes a caller-supplied `base_url` and
//! `POST /api/v1/models/test` contacts a saved provider endpoint. Both go
//! through the same rules as catalog MCP outbound (#262):
//!
//! - the URL must be absolute `http`/`https` without user credentials;
//! - `PROVIDER_OUTBOUND_ALLOWED_ORIGINS` (comma-separated origins), when set,
//!   is an exact allowlist: any other origin is refused. A listed origin may
//!   resolve to private/loopback addresses (e.g. an internal gateway);
//! - without the allowlist only public addresses are allowed (no loopback,
//!   RFC 1918, CGNAT, link-local, ULA, multicast, ...); with
//!   `AGENTOS_AUTH_STRICT=true` the allowlist is required and nothing is
//!   allowed while it is unset;
//! - link-local / cloud metadata, unspecified, multicast and broadcast are
//!   never allowed, even for a listed origin;
//! - the host is resolved once, every answer is checked, and the request is
//!   pinned to the vetted answer with proxies disabled (no DNS rebinding);
//! - redirects are not followed, and response bodies are size-capped.
//!
//! A refused target is answered before any connection is made, with a fixed
//! body that never echoes the URL, the host, or a credential.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use serde_json::json;

use super::mcp::{blocked_outbound_ip, never_permitted_outbound_ip};

/// Exact origin allowlist for provider probes, e.g.
/// `https://api.example.com,http://gateway.internal:3000`.
pub(crate) const PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV: &str = "PROVIDER_OUTBOUND_ALLOWED_ORIGINS";
/// Upper bound for one probe response body.
pub(crate) const PROVIDER_PROBE_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const PROVIDER_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a probe target was refused. Never carries the URL.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProviderOutboundError {
    /// Malformed URL, user credentials, origin not allowed, or a blocked
    /// address. Answered with [`not_allowed_response`].
    NotAllowed,
    /// The host did not resolve. Reported like any other network failure.
    Unresolved,
}

/// A vetted probe target: the host and the addresses it is pinned to.
#[derive(Debug)]
pub(crate) struct VettedProviderTarget {
    host: String,
    addresses: Vec<SocketAddr>,
}

/// Fixed 400 for a refused probe target.
pub(crate) fn not_allowed_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "provider_outbound_not_allowed",
            "message": "provider endpoint is not an allowed outbound destination",
        })),
    )
        .into_response()
}

fn origin_of(url: &reqwest::Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

/// `None` when the allowlist is unset or blank; an invalid entry fails closed by
/// yielding an empty set (nothing allowed).
fn configured_allowed_origins() -> Option<HashSet<String>> {
    let configured = std::env::var(PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV).ok()?;
    if configured.trim().is_empty() {
        return None;
    }
    let mut origins = HashSet::new();
    for entry in configured
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        match reqwest::Url::parse(entry).ok().as_ref().and_then(origin_of) {
            Some(origin) => {
                origins.insert(origin);
            }
            None => return Some(HashSet::new()),
        }
    }
    Some(origins)
}

/// Vet `url` (the exact URL about to be requested) and resolve it once.
pub(crate) async fn vet_provider_url(
    url: &str,
) -> Result<VettedProviderTarget, ProviderOutboundError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ProviderOutboundError::NotAllowed)?;
    let origin = origin_of(&parsed).ok_or(ProviderOutboundError::NotAllowed)?;
    let allowed = configured_allowed_origins();
    let origin_listed = match &allowed {
        Some(origins) if origins.contains(&origin) => true,
        Some(_) => return Err(ProviderOutboundError::NotAllowed),
        // Strict deployments must name their provider origins explicitly.
        None if std::env::var("AGENTOS_AUTH_STRICT").as_deref() == Ok("true") => {
            return Err(ProviderOutboundError::NotAllowed)
        }
        None => false,
    };
    let host = parsed
        .host_str()
        .ok_or(ProviderOutboundError::NotAllowed)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = parsed
        .port_or_known_default()
        .ok_or(ProviderOutboundError::NotAllowed)?;
    let addresses: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| ProviderOutboundError::Unresolved)?
            .collect(),
    };
    if addresses.is_empty() {
        return Err(ProviderOutboundError::Unresolved);
    }
    // Every answer must pass; one bad answer refuses the whole target.
    let refused = addresses.iter().any(|address| {
        let ip = address.ip();
        address.port() != port
            || never_permitted_outbound_ip(ip)
            || (blocked_outbound_ip(ip) && !origin_listed)
    });
    if refused {
        return Err(ProviderOutboundError::NotAllowed);
    }
    Ok(VettedProviderTarget { host, addresses })
}

/// HTTP client pinned to the vetted addresses: no proxy, no redirects,
/// bounded connect and total timeouts.
pub(crate) fn pinned_client(
    target: &VettedProviderTarget,
    timeout_seconds: u64,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(PROVIDER_PROBE_CONNECT_TIMEOUT)
        .timeout(Duration::from_secs(timeout_seconds.clamp(3, 60)))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(&target.host, &target.addresses)
        .build()
}

/// Read at most [`PROVIDER_PROBE_MAX_RESPONSE_BYTES`]; `None` when the body
/// is larger or the stream fails.
pub(crate) async fn read_capped_body(response: reqwest::Response) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > PROVIDER_PROBE_MAX_RESPONSE_BYTES as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len() + chunk.len() > PROVIDER_PROBE_MAX_RESPONSE_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_env<T>(name: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let saved = std::env::var_os(name);
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        let out = f();
        match saved {
            Some(saved) => std::env::set_var(name, saved),
            None => std::env::remove_var(name),
        }
        out
    }

    fn with_allowlist<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        with_env("AGENTOS_AUTH_STRICT", None, || {
            with_env(PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV, value, f)
        })
    }

    fn vet(url: &str) -> Result<(), ProviderOutboundError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(vet_provider_url(url)).map(|_| ())
    }

    #[test]
    fn isolation_contract_provider_outbound_rejects_internal_targets_without_allowlist() {
        let _lock = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_allowlist(None, || {
            for url in [
                "http://127.0.0.1:8080/v1/models",
                "http://localhost:11434/v1/models",
                "http://10.0.0.5/v1/models",
                "http://172.16.0.1/v1/models",
                "http://192.168.1.10/v1/models",
                "http://100.64.0.1/v1/models",
                "http://169.254.169.254/latest/meta-data",
                "http://100.100.100.200/latest/meta-data",
                "http://[::1]/v1/models",
                "http://[::ffff:127.0.0.1]/v1/models",
                "http://[fd00:ec2::254]/latest/meta-data",
                "http://[fe80::1]/v1/models",
                "http://0.0.0.0/v1/models",
                "http://user:pw@203.0.113.10/v1/models",
                "ftp://203.0.113.10/v1/models",
                "not a url",
            ] {
                assert_eq!(
                    vet(url),
                    Err(ProviderOutboundError::NotAllowed),
                    "{url} must be refused"
                );
            }
            // Public literal addresses are allowed without an allowlist.
            assert_eq!(vet("https://203.0.113.10/v1/models"), Ok(()));
        });
    }

    #[test]
    fn isolation_contract_provider_outbound_allowlist_is_exact_and_never_permits_metadata() {
        let _lock = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_allowlist(
            Some("http://127.0.0.1:3000, http://169.254.169.254"),
            || {
                // A listed origin may be private/loopback.
                assert_eq!(vet("http://127.0.0.1:3000/v1/models"), Ok(()));
                // Same host, other port: not listed.
                assert_eq!(
                    vet("http://127.0.0.1:3001/v1/models"),
                    Err(ProviderOutboundError::NotAllowed)
                );
                // Public but not listed: refused once an allowlist exists.
                assert_eq!(
                    vet("https://203.0.113.10/v1/models"),
                    Err(ProviderOutboundError::NotAllowed)
                );
                // Metadata stays refused even when listed.
                assert_eq!(
                    vet("http://169.254.169.254/latest/meta-data"),
                    Err(ProviderOutboundError::NotAllowed)
                );
            },
        );
        // A malformed allowlist entry fails closed.
        with_allowlist(Some("http://127.0.0.1:3000,not-an-origin"), || {
            assert_eq!(
                vet("http://127.0.0.1:3000/v1/models"),
                Err(ProviderOutboundError::NotAllowed)
            );
        });
    }

    #[test]
    fn isolation_contract_provider_outbound_strict_mode_requires_allowlist() {
        let _lock = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_env("AGENTOS_AUTH_STRICT", Some("true"), || {
            for allowlist in [None, Some(" ")] {
                with_env(PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV, allowlist, || {
                    assert_eq!(
                        vet("https://203.0.113.10/v1/models"),
                        Err(ProviderOutboundError::NotAllowed)
                    );
                });
            }
            with_env(
                PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV,
                Some("https://203.0.113.10"),
                || assert_eq!(vet("https://203.0.113.10/v1/models"), Ok(())),
            );
        });
    }
}
