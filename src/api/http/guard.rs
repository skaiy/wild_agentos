//! ToolGuard 审计与统计只读端点。

use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::tools::tool_guard::{GuardAuditEntry, GUARD_AUDIT_LOG};

pub(crate) async fn guard_audit_handler() -> impl IntoResponse {
    let log = GUARD_AUDIT_LOG.read();
    let entries: Vec<GuardAuditEntry> = log.clone();
    Json(json!({
        "total": entries.len(),
        "entries": entries,
    }))
}

pub(crate) async fn guard_stats_handler() -> impl IntoResponse {
    let log = GUARD_AUDIT_LOG.read();
    let total = log.len();
    if total == 0 {
        return Json(json!({
            "total_checks": 0,
            "passed_checks": 0,
            "failed_checks": 0,
            "pass_rate": 1.0,
        }));
    }
    let passed = log.iter().filter(|e| e.validation_passed).count();
    Json(json!({
        "total_checks": total,
        "passed_checks": passed,
        "failed_checks": total - passed,
        "pass_rate": passed as f64 / total as f64,
    }))
}
