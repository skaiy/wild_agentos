//! One-time mirror of built-in methodologies into the platform-scoped KG graph.
//!
//! Execution still uses the in-memory [`MethodologyRegistry`]; this write is
//! optional and only runs when deployment platform scope is configured.

use std::sync::Once;

use tracing::{debug, info, warn};

use crate::isolation::IsolationClaims;
use crate::knowledge_graph::store::KnowledgeGraphStore;

use super::MethodologyDefinition;

/// Deployment tenant for platform-scoped graph writes (same as platform admin).
pub const PLATFORM_ADMIN_TENANT_ENV: &str = "AGENTOS_PLATFORM_ADMIN_TENANT";
/// Project within the platform tenant that owns mirrored methodology triples.
pub const PLATFORM_METHODOLOGY_PROJECT_ENV: &str = "AGENTOS_PLATFORM_METHODOLOGY_PROJECT";

const SEED_ACTOR_ID: &str = "platform-methodology-seed";

static SEED_ONCE: Once = Once::new();

/// Verified isolation scope for platform-level methodology KG seeding.
///
/// Both `AGENTOS_PLATFORM_ADMIN_TENANT` and `AGENTOS_PLATFORM_METHODOLOGY_PROJECT`
/// must be set to non-empty values other than `default`, matching the platform
/// admin tenant gate in the HTTP IAM layer.
pub fn platform_methodology_seed_claims() -> Option<IsolationClaims> {
    let tenant = std::env::var(PLATFORM_ADMIN_TENANT_ENV).ok()?;
    let project = std::env::var(PLATFORM_METHODOLOGY_PROJECT_ENV).ok()?;
    platform_methodology_seed_claims_from_scope(&tenant, &project)
}

/// Builds seed claims from deployment platform tenant and project strings.
pub fn platform_methodology_seed_claims_from_scope(
    tenant: &str,
    project: &str,
) -> Option<IsolationClaims> {
    let tenant = tenant.trim();
    if tenant.is_empty() || tenant == "default" {
        return None;
    }
    let project = project.trim();
    if project.is_empty() || project == "default" {
        return None;
    }
    IsolationClaims::from_verified(tenant, project, SEED_ACTOR_ID).ok()
}

/// Writes built-in methodology quads into the KG once per process.
pub fn seed_builtin_methodologies_once(
    kg: &KnowledgeGraphStore,
    methodologies: &[MethodologyDefinition],
) {
    SEED_ONCE.call_once(|| {
        let seeded = seed_builtin_methodologies(kg, methodologies);
        if seeded == 0 && platform_methodology_seed_claims().is_none() {
            debug!(
                "Built-in methodologies are served from the in-memory registry; \
                 KG seed skipped (set {} and {} to mirror into the platform graph)",
                PLATFORM_ADMIN_TENANT_ENV, PLATFORM_METHODOLOGY_PROJECT_ENV,
            );
        }
    });
}

/// Seeds all given methodologies; returns the number of successful writes.
pub fn seed_builtin_methodologies(
    kg: &KnowledgeGraphStore,
    methodologies: &[MethodologyDefinition],
) -> usize {
    seed_builtin_methodologies_for_claims(kg, methodologies, platform_methodology_seed_claims())
}

/// Seeds methodologies when `claims` is set; returns the number of successful writes.
pub(crate) fn seed_builtin_methodologies_for_claims(
    kg: &KnowledgeGraphStore,
    methodologies: &[MethodologyDefinition],
    claims: Option<IsolationClaims>,
) -> usize {
    let claims = match claims {
        Some(claims) => claims,
        None => return 0,
    };
    let graph = match claims.graph_iri() {
        Ok(graph) => graph,
        Err(e) => {
            warn!(
                "Methodology KG seed skipped: invalid platform graph scope: {}",
                e
            );
            return 0;
        }
    };

    let mut seeded = 0usize;
    for m in methodologies {
        let quads = m.to_kg_quads();
        match kg.write_quads_for_claims(&claims, &quads) {
            Ok(()) => seeded += 1,
            Err(e) => {
                warn!(
                    methodology_id = m.id,
                    error = %e,
                    "Failed to seed methodology into KG"
                );
            }
        }
    }

    if seeded > 0 {
        info!(
            seeded,
            graph = %graph,
            "Seeded methodology definitions into knowledge graph"
        );
    }

    seeded
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::methodology::MethodologyRegistry;

    #[test]
    fn platform_methodology_seed_claims_from_scope_rejects_invalid_values() {
        assert!(platform_methodology_seed_claims_from_scope("", "ops").is_none());
        assert!(platform_methodology_seed_claims_from_scope("platform", "").is_none());
        assert!(platform_methodology_seed_claims_from_scope("default", "ops").is_none());
        assert!(platform_methodology_seed_claims_from_scope("platform", "ops").is_some());
    }

    #[test]
    fn seed_builtin_methodologies_writes_under_platform_claims() {
        let claims =
            platform_methodology_seed_claims_from_scope("platform", "ops").expect("claims");
        let store = Arc::new(oxigraph::store::Store::new().expect("store"));
        let kg = KnowledgeGraphStore::with_shared_store(store).expect("kg");
        let registry = MethodologyRegistry::new();
        let entries: Vec<_> = registry.all().to_vec();

        let seeded = seed_builtin_methodologies_for_claims(&kg, &entries, Some(claims.clone()));
        assert_eq!(seeded, entries.len());

        let sample_id = entries[0].id.replace(':', "/");
        let ask = format!("ASK {{ <https://wildagentos.org/methodology/{sample_id}> ?p ?o }}");
        let rows = kg.query_sparql_for_claims(&claims, &ask).expect("query");
        assert_eq!(rows[0]["result"], serde_json::json!(true));
    }

    #[test]
    fn seed_builtin_methodologies_without_platform_scope_writes_nothing() {
        let store = Arc::new(oxigraph::store::Store::new().expect("store"));
        let kg = KnowledgeGraphStore::with_shared_store(store).expect("kg");
        let registry = MethodologyRegistry::new();
        let entries: Vec<_> = registry.all().to_vec();

        assert_eq!(
            seed_builtin_methodologies_for_claims(&kg, &entries, None),
            0
        );
    }
}
