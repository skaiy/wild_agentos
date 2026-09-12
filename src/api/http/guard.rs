//! Claims-scoped, secret-safe ToolGuard audit and stats endpoints.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::iam::UserIdentity;
use crate::{
    isolation::IsolationClaims,
    tools::tool_guard::{GuardAuditEntry, GUARD_AUDIT_LOG},
};

fn require_claims(identity: &UserIdentity) -> Result<&IsolationClaims, Response> {
    identity.isolation_claims().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "verified_isolation_claims_required",
                "message": "Verified IsolationClaims are required for guard audit access",
            })),
        )
            .into_response()
    })
}

fn audit_entry_is_in_scope(entry: &GuardAuditEntry, claims: &IsolationClaims) -> bool {
    entry.tenant_id.as_deref() == Some(claims.tenant_id())
        && entry.project_id.as_deref() == Some(claims.project_id())
}

fn scoped_audit_entries(claims: &IsolationClaims) -> Vec<GuardAuditEntry> {
    GUARD_AUDIT_LOG
        .read()
        .iter()
        .filter(|entry| audit_entry_is_in_scope(entry, claims))
        .cloned()
        .collect()
}

fn is_sensitive_field(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    [
        "apikey",
        "token",
        "password",
        "authorization",
        "secret",
        "credential",
        "privatekey",
        "accesskey",
        "bearer",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| !is_sensitive_field(key));
            for value in object.values_mut() {
                redact_secrets(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_secrets(value);
            }
        }
        // Validators can include an external tool's JSON response in `error`.
        // Never expose an opaque error that advertises or embeds secret fields.
        Value::String(text) if is_sensitive_field(text) => {
            *text = "[REDACTED]".to_string();
        }
        _ => {}
    }
}

fn redact_audit_entry(entry: GuardAuditEntry) -> Value {
    let mut value = serde_json::to_value(entry).expect("GuardAuditEntry must serialize");
    redact_secrets(&mut value);
    value
}

pub(crate) async fn guard_audit_handler(identity: UserIdentity) -> Response {
    let claims = match require_claims(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let entries: Vec<Value> = scoped_audit_entries(claims)
        .into_iter()
        .map(redact_audit_entry)
        .collect();
    Json(json!({
        "total": entries.len(),
        "entries": entries,
    }))
    .into_response()
}

pub(crate) async fn guard_stats_handler(identity: UserIdentity) -> Response {
    let claims = match require_claims(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let entries = scoped_audit_entries(claims);
    let total = entries.len();
    if total == 0 {
        return Json(json!({
            "total_checks": 0,
            "passed_checks": 0,
            "failed_checks": 0,
            "pass_rate": 1.0,
        }))
        .into_response();
    }
    let passed = entries
        .iter()
        .filter(|entry| entry.validation_passed)
        .count();
    Json(json!({
        "total_checks": total,
        "passed_checks": passed,
        "failed_checks": total - passed,
        "pass_rate": passed as f64 / total as f64,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::get,
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        api::http::{iam::JwtClaims, TEST_ENV_LOCK},
        tools::tool_guard::GUARD_AUDIT_LOG,
    };

    static GUARD_AUDIT_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn entry(
        tenant_id: Option<&str>,
        project_id: Option<&str>,
        error: Option<&str>,
    ) -> GuardAuditEntry {
        GuardAuditEntry {
            timestamp: 0,
            tool_name: "http_request".to_string(),
            agent_id: "agent".to_string(),
            tenant_id: tenant_id.map(str::to_string),
            project_id: project_id.map(str::to_string),
            pre_injected: true,
            validation_passed: true,
            retry_count: 0,
            error: error.map(str::to_string),
        }
    }

    fn token(tenant_id: &str, project_id: &str) -> String {
        encode(
            &Header::default(),
            &JwtClaims {
                sub: "audit-reader".to_string(),
                tenant_id: tenant_id.to_string(),
                project_id: Some(project_id.to_string()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    async fn get_json(router: &Router, uri: &str, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().uri(uri);
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn guard_audit_and_stats_require_verified_claims() {
        let _env = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _audit = GUARD_AUDIT_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let router = Router::new()
            .route("/audit", get(guard_audit_handler))
            .route("/stats", get(guard_stats_handler));

        assert_eq!(
            get_json(&router, "/audit", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_json(&router, "/stats", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn guard_audit_and_stats_are_claims_scoped_and_redacted() {
        let _env = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _audit = GUARD_AUDIT_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::mem::take(&mut *GUARD_AUDIT_LOG.write());
        GUARD_AUDIT_LOG.write().extend([
            entry(
                Some("tenant-a"),
                Some("project-a"),
                Some(r#"tool failed: {"api_key":"do-not-return"}"#),
            ),
            entry(Some("tenant-b"), Some("project-a"), None),
            entry(None, None, None),
        ]);
        let router = Router::new()
            .route("/audit", get(guard_audit_handler))
            .route("/stats", get(guard_stats_handler));
        let tenant_a = token("tenant-a", "project-a");

        let (audit_status, audit) = get_json(&router, "/audit", Some(&tenant_a)).await;
        let (stats_status, stats) = get_json(&router, "/stats", Some(&tenant_a)).await;

        *GUARD_AUDIT_LOG.write() = previous;

        assert_eq!(audit_status, StatusCode::OK);
        assert_eq!(stats_status, StatusCode::OK);
        assert_eq!(audit["total"], 1);
        assert_eq!(stats["total_checks"], 1);
        let encoded = audit.to_string();
        assert!(!encoded.contains("tenant-b"));
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("do-not-return"));
    }
}
