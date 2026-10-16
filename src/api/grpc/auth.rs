//! gRPC authentication and scope gate.
//!
//! Every RPC uses the same verified JWT as HTTP (`authorization: Bearer`).
//! A task outside the caller's tenant and project is indistinguishable from
//! a missing task. Payloads forwarded on an execution stream never carry a
//! `request_id`.

use serde_json::Value;
use tonic::{Request, Status};

use crate::api::http::iam::verify_bearer_blocking;
use crate::core::event_bus::Event;
use crate::isolation::IsolationClaims;

pub(crate) const NOT_FOUND_MESSAGE: &str = "not found";

pub(crate) fn not_found() -> Status {
    Status::not_found(NOT_FOUND_MESSAGE)
}

fn unauthenticated() -> Status {
    Status::unauthenticated("verified JWT required")
}

/// Synchronous interceptor. HS256 verification matches HTTP. OIDC verification
/// uses the same JWKS path, driven from the server runtime.
#[allow(clippy::result_large_err)]
pub fn jwt_interceptor(request: Request<()>) -> Result<Request<()>, Status> {
    claims_from_request(&request)?;
    Ok(request)
}

#[allow(clippy::result_large_err)]
pub(crate) fn claims_from_request<T>(request: &Request<T>) -> Result<IsolationClaims, Status> {
    let header = request
        .metadata()
        .get("authorization")
        .ok_or_else(unauthenticated)?
        .to_str()
        .map_err(|_| unauthenticated())?;
    let mut parts = header.split_ascii_whitespace();
    let token = match (parts.next(), parts.next(), parts.next()) {
        (Some(scheme), Some(token), None)
            if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() =>
        {
            token.to_string()
        }
        _ => return Err(unauthenticated()),
    };
    verify_bearer_blocking(&token)
        .and_then(|identity| identity.isolation_claims().cloned())
        .ok_or_else(unauthenticated)
}

/// A stored node is in scope only when its persisted tenant and project match
/// the verified claims. Malformed records are out of scope.
pub(crate) fn resource_in_scope(json_ld: &str, claims: &IsolationClaims) -> bool {
    if crate::api::http::core_ops::task_is_in_scope(json_ld, claims) {
        return true;
    }
    let Ok(value) = serde_json::from_str::<Value>(json_ld) else {
        return false;
    };
    value.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && value.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
}

/// Approval events exist to carry `request_id`. They are not forwarded on the
/// execution stream, so a caller cannot learn another scope's request id from
/// a subscription that was opened for a different reason.
pub(crate) fn forward_execution_event(event: &Event) -> bool {
    !matches!(
        event.event_type.as_str(),
        "HUMAN_APPROVAL_REQUIRED" | "HUMAN_APPROVAL_RESULT"
    )
}

/// Remove `request_id` from a JSON payload. A payload that still contains the
/// field name after that (non-JSON text, for example) is replaced with `{}`.
pub(crate) fn redact_request_ids(payload: &str) -> String {
    let redacted = match serde_json::from_str::<Value>(payload) {
        Ok(mut value) => {
            strip_request_id(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        }
        Err(_) => payload.to_string(),
    };
    if redacted.contains("request_id") {
        "{}".to_string()
    } else {
        redacted
    }
}

#[cfg(test)]
mod tests {
    use super::redact_request_ids;

    #[test]
    fn isolation_contract_redact_request_ids_removes_nested_and_non_json() {
        let nested = r#"{"ok":true,"meta":{"request_id":"secret-id"},"items":[{"request_id":"also-secret","n":1}]}"#;
        let redacted = redact_request_ids(nested);
        assert!(!redacted.contains("request_id"));
        assert!(!redacted.contains("secret"));
        let value: serde_json::Value = serde_json::from_str(&redacted).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["meta"], serde_json::json!({}));
        assert_eq!(value["items"][0]["n"], 1);
        assert!(value["items"][0].get("request_id").is_none());

        assert_eq!(
            redact_request_ids(r#"{"status":"running","turn":2}"#),
            r#"{"status":"running","turn":2}"#
        );
        assert_eq!(redact_request_ids("request_id=plain-text"), "{}");
        assert_eq!(
            redact_request_ids(r#"{"request_id_note":"still names the field"}"#),
            "{}"
        );
    }
}

fn strip_request_id(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("request_id");
            for child in map.values_mut() {
                strip_request_id(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_request_id(item);
            }
        }
        _ => {}
    }
}
