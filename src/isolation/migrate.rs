//! Explicit, offline migration for historical named graphs.
//!
//! This module is deliberately absent from request handling. It copies a
//! declared source graph into a claims-minted graph; it never adds query-path
//! read-through or a UNION with historical data.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::executor::block_on;
use hyperspace_engine::engine::{HyperspaceEngine, HyperspaceEngineImpl};
use hyperspace_engine::metric::CosineMetric;
use hyperspace_engine::snapshot::load_snapshot;
use hyperspace_engine::wal::WalSyncMode;
use oxigraph::model::{NamedNode, Quad};
use oxigraph::sparql::UpdateEvaluationError;
use oxigraph::store::{Store, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::IsolationClaims;

pub const PLAN_SCHEMA_VERSION: u8 = 2;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationPlan {
    pub schema_version: u8,
    #[serde(default)]
    pub named_graphs: Vec<NamedGraphMigration>,
    #[serde(default)]
    pub vectors: Vec<VectorMigration>,
    #[serde(default)]
    pub l0: Vec<L0Migration>,
    #[serde(default)]
    pub blobs: Vec<BlobMigration>,
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

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VectorMigration {
    pub source_tag: String,
    pub target: MigrationTarget,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct L0Migration {
    pub source_path: String,
    pub target: MigrationTarget,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlobMigration {
    pub source_prefix: String,
    pub target: MigrationTarget,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MigrationReport {
    pub schema_version: u8,
    pub dry_run: bool,
    pub phase: &'static str,
    pub named_graphs: Vec<GraphMigrationReport>,
    pub vectors: Vec<VectorMigrationReport>,
    pub l0: Vec<FileMigrationReport>,
    pub blobs: Vec<FileMigrationReport>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct GraphMigrationReport {
    pub source_graph: String,
    pub target_graph: String,
    pub source_quad_count: usize,
    pub target_quad_count: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct VectorMigrationReport {
    pub source_tag: String,
    pub target_namespace: String,
    pub source_vector_count: usize,
    pub target_vector_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FileMigrationReport {
    pub source: String,
    pub target: String,
    pub source_file_count: usize,
    pub target_file_count: usize,
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
    #[error("unsupported migration plan schema_version {0}; expected 1 or {PLAN_SCHEMA_VERSION}")]
    UnsupportedSchema(u8),
    #[error("migration plan must contain at least one migration")]
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
    #[error("unsafe {kind} path {path:?}")]
    UnsafePath { kind: &'static str, path: String },
    #[error("target path {path:?} is not empty; refusing to merge")]
    TargetPathNotEmpty { path: String },
    #[error("vector migration requires a checkpointed index.snapshot at {0}")]
    VectorSnapshotRequired(String),
    #[error("vector migration source tag {0:?} must be a safe historical tenant:<id> tag")]
    InvalidVectorTag(String),
    #[error("vector migration target namespace {0:?} appears more than once in the plan")]
    DuplicateVectorTarget(String),
    #[error("vector store access failed: {0}")]
    VectorStore(String),
}

/// Parses and validates a JSON plan before any graph store is opened.
pub fn read_plan(path: impl AsRef<Path>) -> Result<MigrationPlan, MigrationError> {
    let plan = serde_json::from_slice(&fs::read(path)?)?;
    validate_plan(&plan)?;
    Ok(plan)
}

/// Plans against an already opened store without changing it.
pub fn plan(
    data_root: impl AsRef<Path>,
    store: &Store,
    plan: &MigrationPlan,
) -> Result<MigrationReport, MigrationError> {
    let entries = preflight(store, plan)?;
    let data_root = data_root.as_ref();
    Ok(MigrationReport {
        schema_version: plan.schema_version,
        dry_run: true,
        phase: "plan",
        named_graphs: entries,
        vectors: preflight_vectors(data_root, plan)?,
        l0: preflight_l0(data_root, plan)?,
        blobs: preflight_blobs(data_root, plan)?,
    })
}

/// Runs `plan → copy → verify`. Source graphs are retained unless both delete
/// confirmations are supplied. All entries are preflighted before copying, so
/// an invalid later entry cannot leave an earlier target populated.
pub fn migrate(
    data_root: impl AsRef<Path>,
    store: &Store,
    plan: &MigrationPlan,
    delete_source: bool,
    confirm_delete_source: bool,
) -> Result<MigrationReport, MigrationError> {
    if delete_source != confirm_delete_source {
        return Err(MigrationError::DeleteNotConfirmed);
    }

    let data_root = data_root.as_ref();
    let entries = preflight(store, plan)?;
    let vectors = preflight_vectors(data_root, plan)?;
    let l0 = preflight_l0(data_root, plan)?;
    let blobs = preflight_blobs(data_root, plan)?;
    let mut transaction = store.start_transaction()?;
    for entry in &entries {
        copy_graph(&mut transaction, &entry.source_graph, &entry.target_graph)?;
    }

    let verified = verify_in_transaction(&transaction, &entries)?;

    transaction.commit()?;
    execute_vectors(data_root, plan, false)?;
    execute_file_migrations(&l0, false)?;
    execute_file_migrations(&blobs, false)?;

    let report = MigrationReport {
        schema_version: plan.schema_version,
        dry_run: false,
        phase: "verify",
        named_graphs: verified,
        vectors: verify_vectors(data_root, plan, vectors)?,
        l0: verify_file_migrations(&l0)?,
        blobs: verify_file_migrations(&blobs)?,
    };
    if delete_source {
        delete_verified_sources(data_root, store, plan, &report, &l0, &blobs)?;
    }
    Ok(report)
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
        vectors: Vec::new(),
        l0: Vec::new(),
        blobs: Vec::new(),
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
        schema_version: report.schema_version,
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
    if plan.schema_version != 1 && plan.schema_version != PLAN_SCHEMA_VERSION {
        return Err(MigrationError::UnsupportedSchema(plan.schema_version));
    }
    if plan.schema_version == 1
        && (!plan.vectors.is_empty() || !plan.l0.is_empty() || !plan.blobs.is_empty())
    {
        return Err(MigrationError::UnsupportedSchema(plan.schema_version));
    }
    if plan.named_graphs.is_empty()
        && plan.vectors.is_empty()
        && plan.l0.is_empty()
        && plan.blobs.is_empty()
    {
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
    let mut vector_targets = BTreeSet::new();
    for mapping in &plan.vectors {
        let source_tenant = mapping
            .source_tag
            .strip_prefix("tenant:")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| MigrationError::InvalidVectorTag(mapping.source_tag.clone()))?;
        IsolationClaims::from_verified(source_tenant, "migration", "migration")
            .map_err(|_| MigrationError::InvalidVectorTag(mapping.source_tag.clone()))?;
        let target = claims_namespace(&mapping.target)?;
        if !vector_targets.insert(target.clone()) {
            return Err(MigrationError::DuplicateVectorTarget(target));
        }
    }
    let mut l0_sources = BTreeSet::new();
    let mut l0_targets = BTreeSet::new();
    for mapping in &plan.l0 {
        require_relative_path(&mapping.source_path, "L0 source")?;
        if mapping.source_path != "l0_store/l0.redb" {
            return Err(MigrationError::UnsafePath {
                kind: "L0 source",
                path: mapping.source_path.clone(),
            });
        }
        if !l0_sources.insert(mapping.source_path.clone()) {
            return Err(MigrationError::UnsafePath {
                kind: "duplicate L0 source",
                path: mapping.source_path.clone(),
            });
        }
        let target = format!("l0/{}/l0.redb", mapping.target.tenant);
        if !l0_targets.insert(target.clone()) {
            return Err(MigrationError::DuplicateTarget(target));
        }
        claims_namespace(&mapping.target)?;
    }
    let mut blob_sources = BTreeSet::new();
    let mut blob_targets = BTreeSet::new();
    for mapping in &plan.blobs {
        require_relative_path(&mapping.source_prefix, "blob source")?;
        let source_tenant = mapping
            .source_prefix
            .strip_prefix("tenant:")
            .and_then(|value| value.strip_suffix("/kb"))
            .ok_or_else(|| MigrationError::UnsafePath {
                kind: "blob source",
                path: mapping.source_prefix.clone(),
            })?;
        IsolationClaims::from_verified(source_tenant, "migration", "migration").map_err(|_| {
            MigrationError::UnsafePath {
                kind: "blob source",
                path: mapping.source_prefix.clone(),
            }
        })?;
        if !blob_sources.insert(mapping.source_prefix.clone()) {
            return Err(MigrationError::UnsafePath {
                kind: "duplicate blob source",
                path: mapping.source_prefix.clone(),
            });
        }
        let target = format!("blobs/{}/kb", mapping.target.tenant);
        if !blob_targets.insert(target.clone()) {
            return Err(MigrationError::DuplicateTarget(target));
        }
        claims_namespace(&mapping.target)?;
    }
    Ok(())
}

fn claims_namespace(target: &MigrationTarget) -> Result<String, MigrationError> {
    IsolationClaims::from_verified(&target.tenant, &target.project, "migration")
        .map_err(|_| MigrationError::InvalidSourceGraph(target.tenant.clone()))?
        .vector_namespace()
        .map_err(|_| MigrationError::InvalidSourceGraph(target.tenant.clone()))
}

fn require_relative_path(path: &str, kind: &'static str) -> Result<(), MigrationError> {
    let parsed = Path::new(path);
    if path.is_empty()
        || parsed.is_absolute()
        || parsed.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(MigrationError::UnsafePath {
            kind,
            path: path.to_owned(),
        });
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

fn preflight_vectors(
    data_root: &Path,
    plan: &MigrationPlan,
) -> Result<Vec<VectorMigrationReport>, MigrationError> {
    if plan.vectors.is_empty() {
        return Ok(Vec::new());
    }
    let snapshot_path = data_root.join("vector_store/index.snapshot");
    let snapshot = load_snapshot(&snapshot_path)
        .map_err(|_| MigrationError::VectorSnapshotRequired(snapshot_path.display().to_string()))?;
    let records: BTreeMap<_, _> = snapshot.forward_meta.into_iter().collect();
    plan.vectors
        .iter()
        .map(|mapping| {
            let namespace = claims_namespace(&mapping.target)?;
            let source_vector_count = records
                .values()
                .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
                .filter(|payload| has_tag(payload, &mapping.source_tag))
                .count();
            let target_vector_count = records
                .values()
                .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
                .filter(|payload| {
                    payload.get("named_graph").and_then(Value::as_str) == Some(&namespace)
                })
                .count();
            if target_vector_count != 0 {
                return Err(MigrationError::TargetPathNotEmpty { path: namespace });
            }
            Ok(VectorMigrationReport {
                source_tag: mapping.source_tag.clone(),
                target_namespace: namespace,
                source_vector_count,
                target_vector_count,
            })
        })
        .collect()
}

fn preflight_l0(
    data_root: &Path,
    plan: &MigrationPlan,
) -> Result<Vec<FileMigrationReport>, MigrationError> {
    plan.l0
        .iter()
        .map(|mapping| {
            let source = data_root.join(&mapping.source_path);
            let target = data_root
                .join("l0")
                .join(&mapping.target.tenant)
                .join("l0.redb");
            preflight_file(&source, &target)
        })
        .collect()
}

fn preflight_blobs(
    data_root: &Path,
    plan: &MigrationPlan,
) -> Result<Vec<FileMigrationReport>, MigrationError> {
    plan.blobs
        .iter()
        .map(|mapping| {
            let source = data_root.join("blobs").join(&mapping.source_prefix);
            let target = data_root
                .join("blobs")
                .join(&mapping.target.tenant)
                .join("kb");
            preflight_directory(&source, &target)
        })
        .collect()
}

fn preflight_file(source: &Path, target: &Path) -> Result<FileMigrationReport, MigrationError> {
    if target.exists() || target.parent().is_some_and(|parent| parent.exists()) {
        return Err(MigrationError::TargetPathNotEmpty {
            path: target.display().to_string(),
        });
    }
    Ok(FileMigrationReport {
        source: source.display().to_string(),
        target: target.display().to_string(),
        source_file_count: usize::from(source.is_file()),
        target_file_count: 0,
    })
}

fn preflight_directory(
    source: &Path,
    target: &Path,
) -> Result<FileMigrationReport, MigrationError> {
    if target.exists() {
        return Err(MigrationError::TargetPathNotEmpty {
            path: target.display().to_string(),
        });
    }
    Ok(FileMigrationReport {
        source: source.display().to_string(),
        target: target.display().to_string(),
        source_file_count: count_files(source),
        target_file_count: 0,
    })
}

fn count_files(path: &Path) -> usize {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
}

fn execute_file_migrations(
    entries: &[FileMigrationReport],
    delete_source: bool,
) -> Result<(), MigrationError> {
    for entry in entries {
        let source = Path::new(&entry.source);
        let target = Path::new(&entry.target);
        if source.is_file() {
            let parent = target.parent().expect("file target has a parent");
            fs::create_dir_all(parent)?;
            fs::copy(source, target)?;
        } else if source.is_dir() {
            copy_directory(source, target)?;
        }
        if count_files(target) != entry.source_file_count {
            return Err(MigrationError::Verification {
                graph: target.display().to_string(),
                expected: entry.source_file_count,
                actual: count_files(target),
            });
        }
        if delete_source && source.exists() {
            if source.is_file() {
                fs::remove_file(source)?;
            } else {
                fs::remove_dir_all(source)?;
            }
        }
    }
    Ok(())
}

fn copy_directory(source: &Path, target: &Path) -> Result<(), MigrationError> {
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry =
            entry.map_err(|error| MigrationError::ReadPlan(std::io::Error::other(error)))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walk entry is under source");
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(destination)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), destination)?;
        } else {
            return Err(MigrationError::UnsafePath {
                kind: "blob source",
                path: entry.path().display().to_string(),
            });
        }
    }
    Ok(())
}

fn verify_file_migrations(
    entries: &[FileMigrationReport],
) -> Result<Vec<FileMigrationReport>, MigrationError> {
    entries
        .iter()
        .map(|entry| {
            let actual = count_files(Path::new(&entry.target));
            if actual != entry.source_file_count {
                return Err(MigrationError::Verification {
                    graph: entry.target.clone(),
                    expected: entry.source_file_count,
                    actual,
                });
            }
            Ok(FileMigrationReport {
                target_file_count: actual,
                ..entry.clone()
            })
        })
        .collect()
}

fn has_tag(payload: &Value, tag: &str) -> bool {
    payload
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().any(|value| value.as_str() == Some(tag)))
}

fn execute_vectors(
    data_root: &Path,
    plan: &MigrationPlan,
    delete_source: bool,
) -> Result<(), MigrationError> {
    if plan.vectors.is_empty() {
        return Ok(());
    }
    let vector_dir = data_root.join("vector_store");
    let snapshot_path = vector_dir.join("index.snapshot");
    let snapshot = load_snapshot(&snapshot_path)
        .map_err(|_| MigrationError::VectorSnapshotRequired(snapshot_path.display().to_string()))?;
    let engine = HyperspaceEngineImpl::open(
        &vector_dir,
        WalSyncMode::Strict,
        snapshot.dimension,
        Box::new(CosineMetric),
        snapshot.config,
    )
    .map_err(|error| MigrationError::VectorStore(error.to_string()))?;

    block_on(async {
        let source_hits = engine
            .list(0, usize::MAX)
            .await
            .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
        for mapping in &plan.vectors {
            let namespace = claims_namespace(&mapping.target)?;
            for hit in source_hits.iter().filter(|hit| {
                hit.payload
                    .as_ref()
                    .is_some_and(|payload| has_tag(payload, &mapping.source_tag))
                    && !hit.iri.starts_with("vector://")
            }) {
                let vector = engine
                    .get_vector(&hit.iri)
                    .await
                    .map_err(|error| MigrationError::VectorStore(error.to_string()))?
                    .ok_or_else(|| {
                        MigrationError::VectorStore(format!("missing vector {}", hit.iri))
                    })?;
                let target_iri = format!("{namespace}#{}", hit.iri);
                let mut payload = hit.payload.clone().expect("filtered hit has payload");
                let object = payload.as_object_mut().ok_or_else(|| {
                    MigrationError::VectorStore(format!(
                        "vector payload {} is not an object",
                        hit.iri
                    ))
                })?;
                object.insert("iri".to_owned(), Value::String(target_iri.clone()));
                object.insert("named_graph".to_owned(), Value::String(namespace.clone()));
                engine
                    .upsert(&target_iri, vector, payload)
                    .await
                    .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
            }
        }
        engine
            .checkpoint()
            .await
            .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
        if delete_source {
            for mapping in &plan.vectors {
                for hit in source_hits.iter().filter(|hit| {
                    hit.payload
                        .as_ref()
                        .is_some_and(|payload| has_tag(payload, &mapping.source_tag))
                }) {
                    engine
                        .delete(&hit.iri)
                        .await
                        .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
                }
            }
            engine
                .checkpoint()
                .await
                .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
        }
        Ok(())
    })
}

fn verify_vectors(
    data_root: &Path,
    plan: &MigrationPlan,
    planned: Vec<VectorMigrationReport>,
) -> Result<Vec<VectorMigrationReport>, MigrationError> {
    let snapshot_path = data_root.join("vector_store/index.snapshot");
    let snapshot = load_snapshot(&snapshot_path)
        .map_err(|_| MigrationError::VectorSnapshotRequired(snapshot_path.display().to_string()))?;
    let records: BTreeMap<_, _> = snapshot.forward_meta.into_iter().collect();
    planned
        .into_iter()
        .zip(&plan.vectors)
        .map(|(entry, mapping)| {
            let actual = records
                .values()
                .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
                .filter(|payload| {
                    payload.get("named_graph").and_then(Value::as_str)
                        == Some(&entry.target_namespace)
                })
                .count();
            if actual != entry.source_vector_count {
                return Err(MigrationError::Verification {
                    graph: entry.target_namespace,
                    expected: entry.source_vector_count,
                    actual,
                });
            }
            let _ = mapping;
            Ok(VectorMigrationReport {
                target_vector_count: actual,
                ..entry
            })
        })
        .collect()
}

fn delete_verified_sources(
    data_root: &Path,
    store: &Store,
    plan: &MigrationPlan,
    report: &MigrationReport,
    l0: &[FileMigrationReport],
    blobs: &[FileMigrationReport],
) -> Result<(), MigrationError> {
    let mut transaction = store.start_transaction()?;
    for entry in &report.named_graphs {
        clear_graph(&mut transaction, &entry.source_graph)?;
    }
    transaction.commit()?;
    delete_vector_sources(&report.vectors, plan, data_root)?;
    for entry in l0.iter().chain(blobs) {
        let source = Path::new(&entry.source);
        if source.is_file() {
            fs::remove_file(source)?;
        } else if source.is_dir() {
            fs::remove_dir_all(source)?;
        }
    }
    Ok(())
}

fn delete_vector_sources(
    reports: &[VectorMigrationReport],
    plan: &MigrationPlan,
    data_root: &Path,
) -> Result<(), MigrationError> {
    if reports.is_empty() {
        return Ok(());
    }
    let vector_dir = data_root.join("vector_store");
    let snapshot = load_snapshot(&vector_dir.join("index.snapshot"))
        .map_err(|_| MigrationError::VectorSnapshotRequired(vector_dir.display().to_string()))?;
    let engine = HyperspaceEngineImpl::open(
        &vector_dir,
        WalSyncMode::Strict,
        snapshot.dimension,
        Box::new(CosineMetric),
        snapshot.config,
    )
    .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
    block_on(async {
        let hits = engine
            .list(0, usize::MAX)
            .await
            .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
        for mapping in &plan.vectors {
            for hit in hits.iter().filter(|hit| {
                hit.payload
                    .as_ref()
                    .is_some_and(|payload| has_tag(payload, &mapping.source_tag))
                    && !hit.iri.starts_with("vector://")
            }) {
                engine
                    .delete(&hit.iri)
                    .await
                    .map_err(|error| MigrationError::VectorStore(error.to_string()))?;
            }
        }
        engine
            .checkpoint()
            .await
            .map_err(|error| MigrationError::VectorStore(error.to_string()))
    })
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
            vectors: vec![],
            l0: vec![],
            blobs: vec![],
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
        let root = tempfile::tempdir().unwrap();
        assert!(migrate(root.path(), &store, &plan, false, false).is_err());
        let target = NamedNode::new("graph://acme/research").unwrap();
        assert!(graph_quads(&store, &target).unwrap().is_empty());
    }

    #[test]
    fn dry_run_does_not_copy_or_delete() {
        let store = fixture_store();
        let root = tempfile::tempdir().unwrap();
        let report = plan(root.path(), &store, &valid_plan()).unwrap();
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
        let root = tempfile::tempdir().unwrap();
        let planned = crate::isolation::migrate::plan(root.path(), &store, &plan).unwrap();
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
        let root = tempfile::tempdir().unwrap();
        let report = migrate(root.path(), &store, &valid_plan(), false, false).unwrap();
        assert_eq!(report.phase, "verify");
        assert_eq!(report.named_graphs[0].target_quad_count, 1);
        assert_eq!(
            graph_quads(&store, &NamedNode::new("graph:world").unwrap())
                .unwrap()
                .len(),
            1
        );
        let audit = write_audit_log(root.path(), &report, false).unwrap();
        assert!(audit.is_file());
        assert!(std::fs::read_to_string(audit)
            .unwrap()
            .contains("\"source_deleted\": false"));
    }

    #[test]
    fn local_blob_and_l0_migration_copies_and_verifies_without_deleting_sources() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("blobs/tenant:legacy/kb/old")).unwrap();
        std::fs::write(
            root.path().join("blobs/tenant:legacy/kb/old/document"),
            b"historical blob",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("l0_store")).unwrap();
        std::fs::write(root.path().join("l0_store/l0.redb"), b"historical l0").unwrap();
        let plan = MigrationPlan {
            schema_version: 2,
            named_graphs: vec![],
            vectors: vec![],
            l0: vec![L0Migration {
                source_path: "l0_store/l0.redb".to_owned(),
                target: MigrationTarget {
                    tenant: "acme".to_owned(),
                    project: "research".to_owned(),
                },
            }],
            blobs: vec![BlobMigration {
                source_prefix: "tenant:legacy/kb".to_owned(),
                target: MigrationTarget {
                    tenant: "acme".to_owned(),
                    project: "research".to_owned(),
                },
            }],
        };
        let store = Store::new().unwrap();
        let report = migrate(root.path(), &store, &plan, false, false).unwrap();
        assert_eq!(report.l0[0].target_file_count, 1);
        assert_eq!(report.blobs[0].target_file_count, 1);
        assert_eq!(
            std::fs::read(root.path().join("blobs/acme/kb/old/document")).unwrap(),
            b"historical blob"
        );
        assert!(root.path().join("l0_store/l0.redb").exists());
        assert!(root
            .path()
            .join("blobs/tenant:legacy/kb/old/document")
            .exists());
    }
}
