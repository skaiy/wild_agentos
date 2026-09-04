//! Explicit, offline migration for historical named graphs.
//!
//! This module is deliberately absent from request handling. It copies a
//! declared source graph into a claims-minted graph; it never adds query-path
//! read-through or a UNION with historical data.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use oxigraph::model::{NamedNode, Quad};
use oxigraph::sparql::UpdateEvaluationError;
use oxigraph::store::{Store, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::IsolationClaims;

pub const PLAN_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationPlan {
    pub schema_version: u8,
    pub named_graphs: Vec<NamedGraphMigration>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamedGraphMigration {
    pub source_graph: String,
    pub target: MigrationTarget,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationTarget {
    pub tenant: String,
    pub project: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MigrationReport {
    pub schema_version: u8,
    pub dry_run: bool,
    pub phase: &'static str,
    pub named_graphs: Vec<GraphMigrationReport>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct GraphMigrationReport {
    pub source_graph: String,
    pub target_graph: String,
    pub source_quad_count: usize,
    pub target_quad_count: usize,
}

#[derive(Debug, Serialize)]
struct AuditLog<'a> {
    schema_version: u8,
    operation: &'static str,
    completed_at_unix_seconds: u64,
    report: &'a MigrationReport,
    source_deleted: bool,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("could not read migration plan: {0}")]
    ReadPlan(#[from] std::io::Error),
    #[error("migration plan is not valid JSON: {0}")]
    ParsePlan(#[from] serde_json::Error),
    #[error("unsupported migration plan schema_version {0}; expected {PLAN_SCHEMA_VERSION}")]
    UnsupportedSchema(u8),
    #[error("migration plan must contain at least one named graph")]
    EmptyPlan,
    #[error("source graph {0:?} is not a valid IRI")]
    InvalidSourceGraph(String),
    #[error("source graph {0:?} is also its target")]
    SourceEqualsTarget(String),
    #[error("target graph {0:?} appears more than once in the plan")]
    DuplicateTarget(String),
    #[error("target graph {graph:?} is not empty ({quad_count} quads); refusing to merge")]
    TargetNotEmpty { graph: String, quad_count: usize },
    #[error("named graph store access failed: {0}")]
    Store(#[from] oxigraph::store::StorageError),
    #[error("named graph SPARQL update failed: {0}")]
    Update(#[from] UpdateEvaluationError),
    #[error("verification failed for {graph:?}: expected {expected} quads, found {actual}")]
    Verification {
        graph: String,
        expected: usize,
        actual: usize,
    },
    #[error("deleting a source requires both --delete-source and --confirm-delete-source")]
    DeleteNotConfirmed,
}

/// Parses and validates a JSON plan before any graph store is opened.
pub fn read_plan(path: impl AsRef<Path>) -> Result<MigrationPlan, MigrationError> {
    let plan = serde_json::from_slice(&fs::read(path)?)?;
    validate_plan(&plan)?;
    Ok(plan)
}

/// Plans against an already opened store without changing it.
pub fn plan(store: &Store, plan: &MigrationPlan) -> Result<MigrationReport, MigrationError> {
    let entries = preflight(store, plan)?;
    Ok(MigrationReport {
        schema_version: PLAN_SCHEMA_VERSION,
        dry_run: true,
        phase: "plan",
        named_graphs: entries,
    })
}

/// Runs `plan → copy → verify`. Source graphs are retained unless both delete
/// confirmations are supplied. All entries are preflighted before copying, so
/// an invalid later entry cannot leave an earlier target populated.
pub fn migrate(
    store: &Store,
    plan: &MigrationPlan,
    delete_source: bool,
    confirm_delete_source: bool,
) -> Result<MigrationReport, MigrationError> {
    if delete_source != confirm_delete_source {
        return Err(MigrationError::DeleteNotConfirmed);
    }

    let entries = preflight(store, plan)?;
    let mut transaction = store.start_transaction()?;
    for entry in &entries {
        copy_graph(&mut transaction, &entry.source_graph, &entry.target_graph)?;
    }

    let verified = verify_in_transaction(&transaction, &entries)?;

    if delete_source {
        for entry in &verified {
            clear_graph(&mut transaction, &entry.source_graph)?;
        }
    }
    transaction.commit()?;

    Ok(MigrationReport {
        schema_version: PLAN_SCHEMA_VERSION,
        dry_run: false,
        phase: "verify",
        named_graphs: verified,
    })
}

/// Verifies that each target still contains every quad counted during `plan`.
/// It is read-only and can be used as an operator's standalone verify phase.
pub fn verify(
    store: &Store,
    planned: &[GraphMigrationReport],
) -> Result<MigrationReport, MigrationError> {
    let mut named_graphs = Vec::with_capacity(planned.len());
    for entry in planned {
        let actual =
            graph_quads(store, &NamedNode::new(entry.target_graph.clone()).unwrap())?.len();
        if actual != entry.source_quad_count {
            return Err(MigrationError::Verification {
                graph: entry.target_graph.clone(),
                expected: entry.source_quad_count,
                actual,
            });
        }
        named_graphs.push(GraphMigrationReport {
            source_graph: entry.source_graph.clone(),
            target_graph: entry.target_graph.clone(),
            source_quad_count: entry.source_quad_count,
            target_quad_count: actual,
        });
    }
    Ok(MigrationReport {
        schema_version: PLAN_SCHEMA_VERSION,
        dry_run: false,
        phase: "verify",
        named_graphs,
    })
}

fn verify_in_transaction(
    transaction: &Transaction<'_>,
    planned: &[GraphMigrationReport],
) -> Result<Vec<GraphMigrationReport>, MigrationError> {
    let mut verified = Vec::with_capacity(planned.len());
    for entry in planned {
        let actual = graph_quads_transaction(
            transaction,
            &NamedNode::new(entry.target_graph.clone()).unwrap(),
        )?
        .len();
        if actual != entry.source_quad_count {
            return Err(MigrationError::Verification {
                graph: entry.target_graph.clone(),
                expected: entry.source_quad_count,
                actual,
            });
        }
        verified.push(GraphMigrationReport {
            source_graph: entry.source_graph.clone(),
            target_graph: entry.target_graph.clone(),
            source_quad_count: entry.source_quad_count,
            target_quad_count: actual,
        });
    }
    Ok(verified)
}

/// Writes the local audit record only after a successful real migration.
pub fn write_audit_log(
    data_root: impl AsRef<Path>,
    report: &MigrationReport,
    source_deleted: bool,
) -> Result<PathBuf, MigrationError> {
    let data_root = data_root.as_ref();
    fs::create_dir_all(data_root)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = data_root.join(format!("isolation-migrate-audit-{timestamp}.json"));
    let audit = AuditLog {
        schema_version: PLAN_SCHEMA_VERSION,
        operation: "named-graph-migration",
        completed_at_unix_seconds: timestamp,
        report,
        source_deleted,
    };
    fs::write(
        &path,
        serde_json::to_vec_pretty(&audit).expect("audit data serializes"),
    )?;
    Ok(path)
}

fn validate_plan(plan: &MigrationPlan) -> Result<(), MigrationError> {
    if plan.schema_version != PLAN_SCHEMA_VERSION {
        return Err(MigrationError::UnsupportedSchema(plan.schema_version));
    }
    if plan.named_graphs.is_empty() {
        return Err(MigrationError::EmptyPlan);
    }

    let mut targets = BTreeSet::new();
    for mapping in &plan.named_graphs {
        NamedNode::new(mapping.source_graph.clone())
            .map_err(|_| MigrationError::InvalidSourceGraph(mapping.source_graph.clone()))?;
        let claims = IsolationClaims::from_verified(
            &mapping.target.tenant,
            &mapping.target.project,
            "migration",
        )
        .map_err(|_| MigrationError::InvalidSourceGraph(mapping.source_graph.clone()))?;
        let target = claims
            .graph_iri()
            .expect("validated claims mint a graph IRI");
        if mapping.source_graph == target {
            return Err(MigrationError::SourceEqualsTarget(target));
        }
        if !targets.insert(target.clone()) {
            return Err(MigrationError::DuplicateTarget(target));
        }
    }
    Ok(())
}

fn preflight(
    store: &Store,
    plan: &MigrationPlan,
) -> Result<Vec<GraphMigrationReport>, MigrationError> {
    validate_plan(plan)?;
    plan.named_graphs
        .iter()
        .map(|mapping| {
            let source = NamedNode::new(mapping.source_graph.clone())
                .map_err(|_| MigrationError::InvalidSourceGraph(mapping.source_graph.clone()))?;
            let target = IsolationClaims::from_verified(
                &mapping.target.tenant,
                &mapping.target.project,
                "migration",
            )
            .expect("plan validation checked target identifiers")
            .graph_iri()
            .expect("validated claims mint a graph IRI");
            let target_node = NamedNode::new(target.clone()).expect("minted graph IRI is valid");
            let target_quad_count = graph_quads(store, &target_node)?.len();
            if target_quad_count != 0 {
                return Err(MigrationError::TargetNotEmpty {
                    graph: target,
                    quad_count: target_quad_count,
                });
            }
            Ok(GraphMigrationReport {
                source_graph: mapping.source_graph.clone(),
                target_graph: target,
                source_quad_count: graph_quads(store, &source)?.len(),
                target_quad_count,
            })
        })
        .collect()
}

fn copy_graph(
    transaction: &mut Transaction<'_>,
    source: &str,
    target: &str,
) -> Result<(), MigrationError> {
    // NamedNode's Display renders a safely escaped SPARQL IRI token.
    let source = NamedNode::new(source)
        .expect("preflight validated source")
        .to_string();
    let target = NamedNode::new(target)
        .expect("preflight minted target")
        .to_string();
    transaction.update(&format!(
        "INSERT {{ GRAPH {target} {{ ?s ?p ?o }} }} WHERE {{ GRAPH {source} {{ ?s ?p ?o }} }}"
    ))?;
    Ok(())
}

fn clear_graph(transaction: &mut Transaction<'_>, graph: &str) -> Result<(), MigrationError> {
    let graph = NamedNode::new(graph)
        .expect("preflight validated source")
        .to_string();
    transaction.update(&format!("CLEAR GRAPH {graph}"))?;
    Ok(())
}

fn graph_quads(store: &Store, graph: &NamedNode) -> Result<Vec<Quad>, MigrationError> {
    Ok(store
        .quads_for_pattern(None, None, None, Some(graph.as_ref().into()))
        .collect::<Result<Vec<_>, _>>()?)
}

fn graph_quads_transaction(
    transaction: &Transaction<'_>,
    graph: &NamedNode,
) -> Result<Vec<Quad>, MigrationError> {
    Ok(transaction
        .quads_for_pattern(None, None, None, Some(graph.as_ref().into()))
        .collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{Literal, NamedNode, Quad};

    fn fixture_store() -> Store {
        let store = Store::new().unwrap();
        store
            .insert(&Quad::new(
                NamedNode::new("urn:subject").unwrap(),
                NamedNode::new("urn:predicate").unwrap(),
                Literal::new_simple_literal("value"),
                NamedNode::new("graph:world").unwrap(),
            ))
            .unwrap();
        store
    }

    fn valid_plan() -> MigrationPlan {
        MigrationPlan {
            schema_version: 1,
            named_graphs: vec![NamedGraphMigration {
                source_graph: "graph:world".to_owned(),
                target: MigrationTarget {
                    tenant: "acme".to_owned(),
                    project: "research".to_owned(),
                },
            }],
        }
    }

    #[test]
    fn invalid_plan_fails_before_targets_change() {
        let store = fixture_store();
        let mut plan = valid_plan();
        plan.named_graphs.push(NamedGraphMigration {
            source_graph: "not an iri".to_owned(),
            target: MigrationTarget {
                tenant: "other".to_owned(),
                project: "project".to_owned(),
            },
        });
        assert!(migrate(&store, &plan, false, false).is_err());
        let target = NamedNode::new("graph://acme/research").unwrap();
        assert!(graph_quads(&store, &target).unwrap().is_empty());
    }

    #[test]
    fn dry_run_does_not_copy_or_delete() {
        let store = fixture_store();
        let report = plan(&store, &valid_plan()).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.named_graphs[0].source_quad_count, 1);
        assert!(
            graph_quads(&store, &NamedNode::new("graph://acme/research").unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            graph_quads(&store, &NamedNode::new("graph:world").unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn verification_detects_omissions() {
        let store = fixture_store();
        let plan = valid_plan();
        let planned = crate::isolation::migrate::plan(&store, &plan).unwrap();
        let mut transaction = store.start_transaction().unwrap();
        copy_graph(&mut transaction, "graph:world", "graph://acme/research").unwrap();
        clear_graph(&mut transaction, "graph://acme/research").unwrap();
        transaction.commit().unwrap();
        assert!(matches!(
            verify(&store, &planned.named_graphs),
            Err(MigrationError::Verification { .. })
        ));
    }

    #[test]
    fn real_migration_preserves_source_and_writes_audit() {
        let store = fixture_store();
        let report = migrate(&store, &valid_plan(), false, false).unwrap();
        assert_eq!(report.phase, "verify");
        assert_eq!(report.named_graphs[0].target_quad_count, 1);
        assert_eq!(
            graph_quads(&store, &NamedNode::new("graph:world").unwrap())
                .unwrap()
                .len(),
            1
        );
        let root = tempfile::tempdir().unwrap();
        let audit = write_audit_log(root.path(), &report, false).unwrap();
        assert!(audit.is_file());
        assert!(std::fs::read_to_string(audit)
            .unwrap()
            .contains("\"source_deleted\": false"));
    }
}
