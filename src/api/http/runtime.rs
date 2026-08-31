//! 运行时只读观测：health / metrics / unified-stats。
//!
//! `live_runtime_hardening_fields` 亦供 `mod.rs` 的 config 处理器复用。

use std::sync::Arc;

use axum::{
    extract::State,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use serde_json::{json, Value};

use super::AppState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

pub(crate) async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

pub(crate) fn live_runtime_hardening_fields() -> Value {
    json!({
        "sandbox": crate::tools::builtin::sandbox::sandbox_runtime_snapshot(),
        "verify_first": crate::core::sa::verify_first_runtime_snapshot(),
        "memory_scheduler": crate::memory::scheduler::MemoryScheduler::runtime_snapshot(),
        "embedding_health": crate::memory::embedding_health_snapshot(),
    })
}

pub(crate) async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let hardening = live_runtime_hardening_fields();
    Json(json!({
        "l2_nodes": state.core.blackboard.node_count(),
        "l2_bytes": state.core.blackboard.total_bytes(),
        "events": state.core.events.event_count(),
        "subscribers": state.core.events.subscriber_count(),
        "skills": state.core.skills.skill_count(),
        "checkpoints": state.core.checkpoints.checkpoint_count(),
        "sandbox_enabled": hardening["sandbox"]["enabled"],
        "unshare_supported": hardening["sandbox"]["unshare_supported"],
        "unshare_enabled": hardening["sandbox"]["unshare_enabled"],
        "memory_scheduler": hardening["memory_scheduler"],
        "verify_first": hardening["verify_first"],
        "embedding_health": hardening["embedding_health"],
    }))
}

/// GET /api/v1/memory/unified-stats — 记忆与知识运维中心的薄聚合只读端点。
/// 一次性返回系统记忆四层(L0-L3) + 业务知识(知识库/知识包/本体) + 运行时的真实规模，
/// 供记忆中心/知识中心顶部 Dashboard 消费。L1/L3 当前无枚举接口，返回 null 并附说明。
pub(crate) async fn unified_stats_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // ── L0 长期记忆 ──
    let l0_entries = match state.core.l0_store.count() {
        Ok(c) => json!(c),
        Err(e) => {
            tracing::warn!("unified-stats: L0 count failed: {}", e);
            json!(null)
        }
    };

    // ── L2 黑板 ──
    let l2_nodes = state.core.blackboard.node_count();
    let l2_bytes = state.core.blackboard.total_bytes();
    let l2_tasks = state.core.blackboard.list_task_summaries().len() as u64;

    // ── 业务知识：知识库（按类型分桶）+ 知识包 ──
    let (kb_total, kb_vector, kb_graph) = {
        let bases = state.knowledge_bases.read().await;
        let vector = bases
            .iter()
            .filter(|b| b.get("kb_type").and_then(|v| v.as_str()) == Some("vector"))
            .count() as u64;
        let graph = bases
            .iter()
            .filter(|b| b.get("kb_type").and_then(|v| v.as_str()) == Some("graph"))
            .count() as u64;
        (bases.len() as u64, vector, graph)
    };
    let kb_packs = state.knowledge_packs.read().await.len() as u64;

    // ── 本体层 ──
    let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();

    Json(json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "memory_tiers": {
            "l0_longterm": {
                "entries": l0_entries,
                "description": "Persistent long-term store (redb)"
            },
            "l1_session": {
                "sessions": null,
                "description": "In-memory session storage (not enumerable)"
            },
            "l2_blackboard": {
                "nodes": l2_nodes,
                "bytes": l2_bytes,
                "tasks": l2_tasks,
                "description": "Shared cross-agent blackboard (Oxigraph)"
            },
            "l3_projection": {
                "projections": null,
                "description": "Derived projection cache (stats not exposed)"
            }
        },
        "knowledge_bases": {
            "total": kb_total,
            "by_type": { "vector": kb_vector, "graph": kb_graph }
        },
        "knowledge_packs": kb_packs,
        "ontology": {
            "domain": ont.domain,
            "object_types": ont.object_types.len() as u64,
            "link_types": ont.link_types.len() as u64,
            "action_types": ont.action_types.len() as u64,
            "functions": ont.functions.len() as u64
        },
        "runtime": {
            "events": {
                "total_emitted": state.core.events.event_count(),
                "active_subscribers": state.core.events.subscriber_count()
            },
            "checkpoints": state.core.checkpoints.checkpoint_count(),
            "skills_registered": state.core.skills.skill_count(),
            "sandbox": crate::tools::builtin::sandbox::sandbox_runtime_snapshot(),
            "verify_first": crate::core::sa::verify_first_runtime_snapshot(),
            "memory_scheduler": crate::memory::scheduler::MemoryScheduler::runtime_snapshot(),
            "embedding_health": crate::memory::embedding_health_snapshot()
        }
    }))
}
