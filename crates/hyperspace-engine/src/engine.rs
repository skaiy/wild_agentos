//! HyperspaceEngine — unified orchestration layer.
//!
//! Provides:
//! - `HyperspaceEngine` async trait (design Section 2.1)
//! - `HyperspaceEngineImpl` struct implementing the trait
//! - `IriRegistry` for u32 ↔ String ID mapping
//! - `SearchHit` typed search result
//! - Full lifecycle: open → insert/upsert/delete → search → checkpoint → vacuum
//!
//! # Architecture
//!
//! ```text
//! HyperspaceEngine trait (async)
//!     └── HyperspaceEngineImpl
//!           ├── EngineWal (write-ahead log)
//!           ├── VectorStore (slot-based persistent storage)
//!           ├── IncrementalHNSW (ANN index with multi-layer search)
//!           ├── JsonLdMetadataIndex (JSON-LD metadata + RoaringBitmap filters)
//!           └── IriRegistry (u32 ↔ String IRI mapping)
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{info, warn};

use crate::error::EngineError;
use crate::filter::{evaluate_filters, JsonLdFilter};
use crate::hnsw::{HnswConfig, IncrementalHNSW};
use crate::hyper_vector::EmbeddingVector;
use crate::jsonld_meta::JsonLdMetadataIndex;
use crate::metric::{metric_from_kind, Metric};
use crate::snapshot::{self, EngineSnapshot};
use crate::storage::VectorStore;
use crate::wal::{EngineWal, WalOp, WalSyncMode};

// ── SearchHit ────────────────────────────────────────────────────────────────

/// Typed search result (design Section 2.1).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: u32,
    pub iri: String,
    pub score: f32,
    pub payload: Option<Value>,
}

// ── IriRegistry ──────────────────────────────────────────────────────────────

/// Bi-directional IRI ↔ u32 ID mapping.
///
/// Provides:
/// - `resolve(iri) → Option<u32>`: find existing ID for an IRI
/// - `register(iri) → u32`: get or create ID
/// - `lookup(id) → Option<String>`: get IRI by numeric ID
#[derive(Debug, Clone)]
pub struct IriRegistry {
    iri_to_id: std::collections::HashMap<String, u32>,
    id_to_iri: std::collections::HashMap<u32, String>,
    next_id: u32,
}

impl Default for IriRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IriRegistry {
    pub fn new() -> Self {
        Self {
            iri_to_id: std::collections::HashMap::new(),
            id_to_iri: std::collections::HashMap::new(),
            next_id: 1, // Start at 1; 0 is reserved for non-IRI entries
        }
    }

    /// Register an IRI, returning its numeric ID.
    /// If already registered, returns the existing ID.
    pub fn register(&mut self, iri: &str) -> u32 {
        if let Some(&id) = self.iri_to_id.get(iri) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.iri_to_id.insert(iri.to_string(), id);
        self.id_to_iri.insert(id, iri.to_string());
        id
    }

    /// Resolve an IRI to its numeric ID (if registered).
    pub fn resolve(&self, iri: &str) -> Option<u32> {
        self.iri_to_id.get(iri).copied()
    }

    /// Look up the IRI for a numeric ID.
    pub fn lookup(&self, id: u32) -> Option<String> {
        self.id_to_iri.get(&id).cloned()
    }

    /// Number of registered IRIs.
    pub fn len(&self) -> usize {
        self.iri_to_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.iri_to_id.is_empty()
    }

    /// Export all entries for snapshot serialization.
    pub fn export(&self) -> Vec<(u32, String)> {
        self.id_to_iri
            .iter()
            .map(|(&id, iri)| (id, iri.clone()))
            .collect()
    }

    /// Import entries from a snapshot.
    pub fn import(&mut self, entries: Vec<(u32, String)>) {
        for (id, iri) in entries {
            self.iri_to_id.insert(iri.clone(), id);
            self.id_to_iri.insert(id, iri);
            if id >= self.next_id {
                self.next_id = id + 1;
            }
        }
    }
}

// ── Searcher (read-only snapshot) ────────────────────────────────────────────

/// Searcher carries a snapshot of the engine state for concurrent search.
///
/// Created via `HyperspaceEngineImpl::searcher()`. Clone is O(n) — for production
/// use with large datasets, implement a snapshot-based mechanism.
pub struct Searcher {
    index: IncrementalHNSW,
    metadata: JsonLdMetadataIndex,
    iri_registry: IriRegistry,
}

impl Searcher {
    fn new(
        index: IncrementalHNSW,
        metadata: JsonLdMetadataIndex,
        iri_registry: IriRegistry,
    ) -> Self {
        Self {
            index,
            metadata,
            iri_registry,
        }
    }

    /// Search without filters.
    pub fn search(&mut self, query: &EmbeddingVector, top_k: usize) -> Vec<SearchHit> {
        self.search_with_filter(query, top_k, &[])
    }

    /// Search with JSON-LD filters.
    pub fn search_with_filter(
        &mut self,
        query: &EmbeddingVector,
        top_k: usize,
        filters: &[JsonLdFilter],
    ) -> Vec<SearchHit> {
        // Evaluate filters to get allowed bitmap
        let allowed = if filters.is_empty() {
            None
        } else {
            evaluate_filters(&self.metadata, filters)
        };

        let results = if let Some(ref ab) = allowed {
            self.index.search_with_filter(query, top_k, Some(ab))
        } else {
            self.index.search(query, top_k)
        };

        results
            .into_iter()
            .map(|(id, dist)| {
                let iri = self.iri_registry.lookup(id).unwrap_or_default();
                let payload = self.metadata.get_payload(id);
                SearchHit {
                    id,
                    iri,
                    score: -(dist as f32),
                    payload,
                }
            })
            .collect()
    }

    pub fn metadata(&self) -> &JsonLdMetadataIndex {
        &self.metadata
    }
}

// ── EngineInner (Mutex-protected mutable state) ──────────────────────────────

struct EngineInner {
    index: IncrementalHNSW,
    store: VectorStore,
    clock: u64,
}

// ── HyperspaceEngine trait ───────────────────────────────────────────────────

/// Core engine trait (design Section 2.1).
#[async_trait]
pub trait HyperspaceEngine: Send + Sync {
    // ── Writing ──
    async fn insert(
        &self,
        iri: &str,
        vector: EmbeddingVector,
        jsonld: Value,
    ) -> Result<u32, EngineError>;
    async fn upsert(
        &self,
        iri: &str,
        vector: EmbeddingVector,
        jsonld: Value,
    ) -> Result<u32, EngineError>;
    async fn delete(&self, iri: &str) -> Result<(), EngineError>;

    /// Resolve an IRI to its numeric ID (if registered).
    async fn resolve_iri(&self, iri: &str) -> Result<Option<u32>, EngineError>;

    /// Look up the IRI for a numeric ID (reverse of resolve_iri).
    async fn lookup_id(&self, id: u32) -> Result<Option<String>, EngineError>;

    // ── Retrieval ──
    async fn search(
        &self,
        query: &EmbeddingVector,
        top_k: usize,
        filters: &[JsonLdFilter],
    ) -> Result<Vec<SearchHit>, EngineError>;

    /// Dual-space hybrid search: text (Cosine) × struct (Poincaré) weighted fusion.
    async fn hybrid_search(
        &self,
        text_query: Option<&EmbeddingVector>,
        struct_query: Option<&EmbeddingVector>,
        top_k: usize,
        alpha: f32,
        filters: &[JsonLdFilter],
    ) -> Result<Vec<SearchHit>, EngineError>;

    // ── Metadata ──
    async fn count(&self) -> Result<u64, EngineError>;
    async fn get_payload(&self, iri: &str) -> Result<Option<Value>, EngineError>;
    async fn get_vector(&self, iri: &str) -> Result<Option<EmbeddingVector>, EngineError>;
    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<SearchHit>, EngineError>;

    // ── Maintenance ──
    async fn checkpoint(&self) -> Result<(), EngineError>;
    async fn vacuum(&self) -> Result<(), EngineError>;
}

// ── HyperspaceEngineImpl ─────────────────────────────────────────────────────

/// Concrete engine implementation.
pub struct HyperspaceEngineImpl {
    inner: Mutex<EngineInner>,
    metadata: JsonLdMetadataIndex,
    iri_registry: Mutex<IriRegistry>,
    wal: EngineWal,
    data_dir: PathBuf,
    config: HnswConfig,
    dim: usize,
}

impl HyperspaceEngineImpl {
    /// Open or create an engine at the given directory.
    pub fn open(
        dir: &Path,
        sync_mode: WalSyncMode,
        dim: usize,
        metric: Box<dyn Metric>,
        config: HnswConfig,
    ) -> Result<Self, EngineError> {
        let wal = EngineWal::open(dir, sync_mode)?;
        let element_size = EmbeddingVector::element_size(dim);
        let store = VectorStore::new(dir, element_size);
        let index = IncrementalHNSW::new(metric, config.clone());
        let metadata = JsonLdMetadataIndex::new();
        let iri_registry = IriRegistry::new();

        let engine = Self {
            inner: Mutex::new(EngineInner {
                index,
                store,
                clock: 0,
            }),
            metadata,
            iri_registry: Mutex::new(iri_registry),
            wal,
            data_dir: dir.to_owned(),
            config,
            dim,
        };

        // Try loading snapshot first (faster than full WAL replay)
        let snapshot_path = dir.join("index.snapshot");
        if snapshot_path.exists() {
            match snapshot::load_snapshot(&snapshot_path) {
                Ok(snap) => {
                    info!(
                        "Loading snapshot: {} nodes, clock={}",
                        snap.nodes.len(),
                        snap.clock
                    );
                    eprintln!("DEBUG: snapshot forward_meta len={}, deleted_ids len={}, iri_registry len={}",
                        snap.forward_meta.len(), snap.deleted_ids.len(), snap.iri_registry.len());
                    let mut inner = engine.inner.lock().unwrap();
                    let metric_kind = inner.index.metric().kind();
                    inner.index.import_nodes(snap.nodes.clone());
                    inner.clock = snap.clock;
                    // Populate VectorStore from HNSW node data
                    for (node_id, node_opt) in snap.nodes.iter().enumerate() {
                        if let Some(node) = node_opt {
                            let vec =
                                EmbeddingVector::new_unchecked(node.coords.clone(), metric_kind);
                            let bytes = vec.as_bytes();
                            let _ = inner.store.set(node_id as u32, &bytes);
                        }
                    }
                    if let Ok(mut reg) = engine.iri_registry.lock() {
                        reg.import(snap.iri_registry);
                    }
                    // Restore forward metadata (JSON strings → Values)
                    for (id, payload_str) in snap.forward_meta {
                        if let Ok(payload) = serde_json::from_str(&payload_str) {
                            engine.metadata.index(id, &payload);
                        }
                    }
                    // Restore deleted IDs
                    for id in snap.deleted_ids {
                        engine.metadata.remove(id);
                    }
                    drop(inner);
                }
                Err(e) => {
                    warn!("Failed to load snapshot, falling back to WAL replay: {e}");
                    engine.recover()?;
                }
            }
        } else {
            // No snapshot — full WAL replay
            engine.recover()?;
        }

        Ok(engine)
    }

    /// Recover state by replaying the WAL into store + index.
    fn recover(&self) -> Result<(), EngineError> {
        let wal_path = self.wal.active_path().to_owned();
        let mut inner = self.inner.lock().unwrap();

        let _count = EngineWal::replay(&wal_path, |op, ts, data| {
            inner.clock = inner.clock.max(ts);
            match op {
                WalOp::Insert { id, iri } | WalOp::Upsert { id, iri } => {
                    let _ = inner.store.set(id, data);
                    if !data.is_empty() {
                        let dim = (data.len().saturating_sub(12)) / 8;
                        if let Ok(vec) = EmbeddingVector::from_bytes(data, dim) {
                            inner.index.insert(id, vec);
                        }
                    }
                    if !iri.is_empty() {
                        if let Ok(mut reg) = self.iri_registry.lock() {
                            reg.register(&iri);
                        }
                    }
                }
                WalOp::Delete { id, .. } => {
                    inner.store.remove(id);
                    inner.index.remove(id);
                    self.metadata.remove(id);
                }
                WalOp::MetadataUpdate { .. } => {}
            }
        })?;

        info!("WAL replay complete: {} entries", _count);
        Ok(())
    }

    /// Create a Searcher (read-only snapshot) for concurrent querying.
    /// Note: O(n) — clones the HNSW index.
    pub fn searcher(&self) -> Searcher {
        let inner = self.inner.lock().unwrap();
        let metric_kind = inner.index.metric().kind();
        let mut new_index =
            IncrementalHNSW::new(metric_from_kind(metric_kind), self.config.clone());
        for (id, _) in inner.store.iter_active() {
            if let Some(bytes) = inner.store.get(id) {
                let dim = (inner.store.element_size().saturating_sub(12)) / 8;
                if let Ok(vec) = EmbeddingVector::from_bytes(bytes, dim) {
                    new_index.insert(id, vec);
                }
            }
        }
        let iri_registry = self.iri_registry.lock().unwrap().clone();
        Searcher::new(new_index, self.metadata.clone(), iri_registry)
    }
}

#[async_trait]
impl HyperspaceEngine for HyperspaceEngineImpl {
    // ── Insert ──────────────────────────────────────────────────────────────

    async fn insert(
        &self,
        iri: &str,
        vector: EmbeddingVector,
        jsonld: Value,
    ) -> Result<u32, EngineError> {
        let id = {
            let mut reg = self.iri_registry.lock().unwrap();
            reg.register(iri)
        };
        let bytes = vector.as_bytes();

        // WAL first
        self.wal.append(
            &WalOp::Insert {
                id,
                iri: iri.to_string(),
            },
            {
                let mut inner = self.inner.lock().unwrap();
                inner.clock += 1;
                inner.clock
            },
            &bytes,
        )?;

        // Apply to store + index + metadata
        let mut inner = self.inner.lock().unwrap();
        inner.store.set(id, &bytes)?;
        inner.index.insert(id, vector);
        self.metadata.index(id, &jsonld);
        // Clear deleted flag in case this is a re-insert of the same IRI
        self.metadata.undelete(id);

        Ok(id)
    }

    // ── Upsert ──────────────────────────────────────────────────────────────

    async fn upsert(
        &self,
        iri: &str,
        vector: EmbeddingVector,
        jsonld: Value,
    ) -> Result<u32, EngineError> {
        let id = {
            let mut reg = self.iri_registry.lock().unwrap();
            reg.register(iri)
        };
        let bytes = vector.as_bytes();

        self.wal.append(
            &WalOp::Upsert {
                id,
                iri: iri.to_string(),
            },
            {
                let mut inner = self.inner.lock().unwrap();
                inner.clock += 1;
                inner.clock
            },
            &bytes,
        )?;

        let mut inner = self.inner.lock().unwrap();
        inner.store.set(id, &bytes)?;
        inner.index.insert(id, vector);
        self.metadata.index(id, &jsonld);

        Ok(id)
    }

    // ── Delete ──────────────────────────────────────────────────────────────

    async fn delete(&self, iri: &str) -> Result<(), EngineError> {
        let id = {
            let reg = self.iri_registry.lock().unwrap();
            reg.resolve(iri)
                .ok_or_else(|| EngineError::NotFound(iri.to_string()))?
        };

        self.wal.append(
            &WalOp::Delete {
                id,
                iri: iri.to_string(),
            },
            {
                let mut inner = self.inner.lock().unwrap();
                inner.clock += 1;
                inner.clock
            },
            &[],
        )?;

        let mut inner = self.inner.lock().unwrap();
        inner.store.remove(id);
        inner.index.remove(id);
        self.metadata.remove(id);

        Ok(())
    }

    // ── Search ──────────────────────────────────────────────────────────────

    async fn search(
        &self,
        query: &EmbeddingVector,
        top_k: usize,
        filters: &[JsonLdFilter],
    ) -> Result<Vec<SearchHit>, EngineError> {
        let allowed = if filters.is_empty() {
            None
        } else {
            evaluate_filters(&self.metadata, filters)
        };

        let mut inner = self.inner.lock().unwrap();
        let results = if let Some(ref ab) = allowed {
            inner.index.search_with_filter(query, top_k, Some(ab))
        } else {
            inner.index.search(query, top_k)
        };

        let reg = self.iri_registry.lock().unwrap();
        Ok(results
            .into_iter()
            .map(|(id, dist)| {
                let iri = reg.lookup(id).unwrap_or_default();
                let payload = self.metadata.get_payload(id);
                SearchHit {
                    id,
                    iri,
                    score: -(dist as f32),
                    payload,
                }
            })
            .collect())
    }

    // ── Hybrid Search ───────────────────────────────────────────────────────

    async fn hybrid_search(
        &self,
        text_query: Option<&EmbeddingVector>,
        struct_query: Option<&EmbeddingVector>,
        top_k: usize,
        alpha: f32,
        filters: &[JsonLdFilter],
    ) -> Result<Vec<SearchHit>, EngineError> {
        // Evaluate filters
        let allowed = if filters.is_empty() {
            None
        } else {
            evaluate_filters(&self.metadata, filters)
        };

        let mut inner = self.inner.lock().unwrap();

        // For hybrid search, text and struct use the same index.
        // In production, use separate indexes: one Cosine, one Poincaré.
        let text_results = text_query.map_or(Vec::new(), |q| {
            if let Some(ref ab) = allowed {
                inner.index.search_with_filter(q, top_k * 3, Some(ab))
            } else {
                inner.index.search(q, top_k * 3)
            }
        });
        let struct_results = struct_query.map_or(Vec::new(), |q| {
            if let Some(ref ab) = allowed {
                inner.index.search_with_filter(q, top_k * 3, Some(ab))
            } else {
                inner.index.search(q, top_k * 3)
            }
        });

        drop(inner);
        let reg = self.iri_registry.lock().unwrap();

        if text_results.is_empty() && struct_results.is_empty() {
            return Ok(Vec::new());
        }

        // RRF-style fusion
        let max_text_dist = text_results.first().map(|r| r.1).unwrap_or(1.0).max(0.001);
        let max_struct_dist = struct_results
            .first()
            .map(|r| r.1)
            .unwrap_or(1.0)
            .max(0.001);

        let mut fused: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
        for (id, d) in &text_results {
            let score = alpha * (1.0 - (*d as f32) / max_text_dist as f32);
            *fused.entry(*id).or_insert(0.0) += score;
        }
        for (id, d) in &struct_results {
            let score = (1.0 - alpha) * (1.0 - (*d as f32) / max_struct_dist as f32);
            *fused.entry(*id).or_insert(0.0) += score;
        }

        let mut sorted: Vec<(u32, f32)> = fused.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(top_k);

        Ok(sorted
            .into_iter()
            .map(|(id, score)| {
                let iri = reg.lookup(id).unwrap_or_default();
                let payload = self.metadata.get_payload(id);
                SearchHit {
                    id,
                    iri,
                    score,
                    payload,
                }
            })
            .collect())
    }

    // ── Count ───────────────────────────────────────────────────────────────

    async fn count(&self) -> Result<u64, EngineError> {
        Ok(self.metadata.count())
    }

    // ── Get Payload ─────────────────────────────────────────────────────────

    async fn get_payload(&self, iri: &str) -> Result<Option<Value>, EngineError> {
        let reg = self.iri_registry.lock().unwrap();
        let payload = reg
            .resolve(iri)
            .and_then(|id| self.metadata.get_payload(id));
        Ok(payload)
    }

    async fn get_vector(&self, iri: &str) -> Result<Option<EmbeddingVector>, EngineError> {
        let id = {
            let reg = self.iri_registry.lock().unwrap();
            reg.resolve(iri)
        };
        match id {
            None => Ok(None),
            Some(id) => {
                let inner = self.inner.lock().unwrap();
                match inner.store.get(id) {
                    None => Ok(None),
                    Some(bytes) => {
                        let dim = (inner.store.element_size().saturating_sub(12)) / 8;
                        let vec = EmbeddingVector::from_bytes(bytes, dim).map_err(|e| {
                            EngineError::StorageError {
                                message: format!("Vector deserialization: {e}"),
                            }
                        })?;
                        Ok(Some(vec))
                    }
                }
            }
        }
    }

    async fn resolve_iri(&self, iri: &str) -> Result<Option<u32>, EngineError> {
        let reg = self.iri_registry.lock().unwrap();
        Ok(reg.resolve(iri))
    }

    async fn lookup_id(&self, id: u32) -> Result<Option<String>, EngineError> {
        let reg = self.iri_registry.lock().unwrap();
        Ok(reg.lookup(id))
    }

    // ── List ────────────────────────────────────────────────────────────────

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<SearchHit>, EngineError> {
        let reg = self.iri_registry.lock().unwrap();
        let all_ids = self.metadata.all_ids();
        let page: Vec<u32> = all_ids.into_iter().skip(offset).take(limit).collect();
        Ok(page
            .into_iter()
            .map(|id| {
                let iri = reg.lookup(id).unwrap_or_default();
                let payload = self.metadata.get_payload(id);
                SearchHit {
                    id,
                    iri,
                    score: 0.0,
                    payload,
                }
            })
            .collect())
    }

    // ── Checkpoint ──────────────────────────────────────────────────────────

    async fn checkpoint(&self) -> Result<(), EngineError> {
        let snapshot_path = self.data_dir.join("index.snapshot");

        // Phase 1: Rotate WAL (current → frozen, new active is empty)
        self.wal.rotate()?;

        // Phase 2: Build snapshot from current state
        let (nodes, clock, iri_entries, forward_entries, deleted_ids) = {
            let inner = self.inner.lock().unwrap();
            let reg = self.iri_registry.lock().unwrap();

            let nodes = inner.index.export_nodes();
            let clock = inner.clock;
            let iri_entries = reg.export();
            let forward_entries: Vec<(u32, String)> = self
                .metadata
                .forward
                .iter()
                .map(|e| {
                    (
                        *e.key(),
                        serde_json::to_string(e.value()).unwrap_or_default(),
                    )
                })
                .collect();
            let deleted_ids: Vec<u32> = self
                .metadata
                .deleted
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .collect();

            (nodes, clock, iri_entries, forward_entries, deleted_ids)
        };

        let snap = EngineSnapshot {
            nodes,
            entry_point: 0, // will be reconstructed on import
            clock,
            iri_registry: iri_entries,
            forward_meta: forward_entries,
            deleted_ids,
            dimension: self.dim,
            config: self.config.clone(),
        };
        snapshot::save_snapshot(&snapshot_path, &snap)?;

        // Phase 3: Delete frozen WAL files (already safe in snapshot)
        self.wal.cleanup_frozen()?;

        info!("Checkpoint complete: snapshot saved, frozen WALs cleaned");
        Ok(())
    }

    // ── Vacuum ──────────────────────────────────────────────────────────────

    async fn vacuum(&self) -> Result<(), EngineError> {
        let cleaned = self.metadata.vacuum();
        info!(
            "Vacuum complete: cleaned {} entries from metadata indexes",
            cleaned
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyper_vector::MetricKind;
    use crate::metric::CosineMetric;
    use serde_json::json;

    fn setup_engine(dir: &Path) -> HyperspaceEngineImpl {
        setup_engine_with_config(dir, HnswConfig::default())
    }

    fn setup_engine_with_config(dir: &Path, config: HnswConfig) -> HyperspaceEngineImpl {
        HyperspaceEngineImpl::open(dir, WalSyncMode::Strict, 4, Box::new(CosineMetric), config)
            .unwrap()
    }

    fn v(coords: Vec<f64>) -> EmbeddingVector {
        EmbeddingVector::new_unchecked(coords, MetricKind::Cosine)
    }

    fn setup_async_engine(dir: &Path) -> HyperspaceEngineImpl {
        setup_engine(dir)
    }

    fn sequential_unit_vector(index: usize, total: usize) -> EmbeddingVector {
        let angle = (index + 1) as f64 * std::f64::consts::FRAC_PI_2 / (total + 1) as f64;
        v(vec![angle.cos(), angle.sin(), 0.0, 0.0])
    }

    #[tokio::test]
    async fn test_engine_insert_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert(
            "vec:1",
            v(vec![1.0, 0.0, 0.0, 0.0]),
            json!({"@type": ["Test"], "label": "first"}),
        )
        .await
        .unwrap();
        eng.insert(
            "vec:2",
            v(vec![0.0, 1.0, 0.0, 0.0]),
            json!({"@type": ["Test"], "label": "second"}),
        )
        .await
        .unwrap();

        assert_eq!(eng.count().await.unwrap(), 2);

        let results = eng
            .search(&v(vec![1.0, 0.0, 0.0, 0.0]), 5, &[])
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 1);
        assert_eq!(results[0].iri, "vec:1");
    }

    #[tokio::test]
    async fn test_engine_delete() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert("a", v(vec![1.0, 0.0, 0.0, 0.0]), json!({"id": "a"}))
            .await
            .unwrap();
        eng.insert("b", v(vec![0.0, 1.0, 0.0, 0.0]), json!({"id": "b"}))
            .await
            .unwrap();
        assert_eq!(eng.count().await.unwrap(), 2);

        eng.delete("a").await.unwrap();
        assert_eq!(eng.count().await.unwrap(), 1);

        let results = eng
            .search(&v(vec![1.0, 0.0, 0.0, 0.0]), 5, &[])
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);
    }

    #[tokio::test]
    async fn test_engine_recovery() {
        let dir = tempfile::tempdir().unwrap();
        {
            let eng = setup_async_engine(dir.path());
            eng.insert(
                "p",
                v(vec![1.0, 0.0, 0.0, 0.0]),
                json!({"label": "persist"}),
            )
            .await
            .unwrap();
            eng.checkpoint().await.unwrap();
        }
        // Re-open
        let eng = setup_async_engine(dir.path());
        assert_eq!(eng.count().await.unwrap(), 1);
        let results = eng
            .search(&v(vec![1.0, 0.0, 0.0, 0.0]), 5, &[])
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].iri, "p");
    }

    #[tokio::test]
    async fn test_search_with_filters() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert(
            "doc:1",
            v(vec![1.0, 0.0, 0.0, 0.0]),
            json!({"@type": ["Document"], "tags": ["important"], "importance": 0.9}),
        )
        .await
        .unwrap();
        eng.insert(
            "doc:2",
            v(vec![0.0, 1.0, 0.0, 0.0]),
            json!({"@type": ["Document", "Report"], "tags": ["normal"], "importance": 0.5}),
        )
        .await
        .unwrap();
        eng.insert(
            "note:1",
            v(vec![0.0, 0.0, 1.0, 0.0]),
            json!({"@type": ["Note"], "tags": ["important"], "importance": 0.3}),
        )
        .await
        .unwrap();

        // Filter by type
        let results = eng
            .search(
                &v(vec![0.5, 0.5, 0.5, 0.0]),
                10,
                &[JsonLdFilter::Type("Document".into())],
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|h| h.iri == "doc:1"));
        assert!(results.iter().any(|h| h.iri == "doc:2"));

        // Filter by tag
        let results = eng
            .search(
                &v(vec![0.5, 0.5, 0.5, 0.0]),
                10,
                &[JsonLdFilter::tag("tags", "important")],
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|h| h.iri == "doc:1"));
        assert!(results.iter().any(|h| h.iri == "note:1"));
    }

    #[tokio::test]
    async fn test_sequential_inserts_preserve_self_recall_through_tag_filtering() {
        let config = HnswConfig {
            m: 16,
            ef_search: 50,
            level_mult: 0.0,
            ..HnswConfig::default()
        };
        let tag_filter = JsonLdFilter::tag("tags", "self-recall");

        for total in [64, 256, 1000] {
            let dir = tempfile::tempdir().unwrap();
            let eng = setup_engine_with_config(dir.path(), config.clone());

            for index in 0..total {
                eng.insert(
                    &format!("recall:{index}"),
                    sequential_unit_vector(index, total),
                    json!({"tags": ["self-recall"]}),
                )
                .await
                .unwrap();
            }

            for index in 0..total {
                let query = sequential_unit_vector(index, total);
                let expected_iri = format!("recall:{index}");
                assert_eq!(
                    eng.search(&query, 1, &[])
                        .await
                        .unwrap()
                        .first()
                        .map(|hit| hit.iri.as_str()),
                    Some(expected_iri.as_str()),
                    "unfiltered search should return vector {index} as its own nearest neighbor"
                );
                assert_eq!(
                    eng.search(&query, 1, std::slice::from_ref(&tag_filter))
                        .await
                        .unwrap()
                        .first()
                        .map(|hit| hit.iri.as_str()),
                    Some(expected_iri.as_str()),
                    "tag-filtered search should return vector {index} as its own nearest neighbor"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_get_payload() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert("x", v(vec![1.0, 0.0, 0.0, 0.0]), json!({"text": "hello"}))
            .await
            .unwrap();
        let payload = eng.get_payload("x").await.unwrap();
        assert!(payload.is_some());
        assert_eq!(
            payload.unwrap().get("text").unwrap().as_str().unwrap(),
            "hello"
        );

        let missing = eng.get_payload("nonexistent").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        for i in 0..5u32 {
            let iri = format!("item:{i}");
            eng.insert(&iri, v(vec![1.0, 0.0, 0.0, 0.0]), json!({"idx": i}))
                .await
                .unwrap();
        }

        let all = eng.list(0, 10).await.unwrap();
        assert_eq!(all.len(), 5);

        let page = eng.list(1, 2).await.unwrap();
        assert_eq!(page.len(), 2);
    }

    #[tokio::test]
    async fn test_hybrid_search() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert("h:1", v(vec![1.0, 0.0, 0.0, 0.0]), json!({"text": "first"}))
            .await
            .unwrap();
        eng.insert(
            "h:2",
            v(vec![0.0, 1.0, 0.0, 0.0]),
            json!({"text": "second"}),
        )
        .await
        .unwrap();

        let q = v(vec![1.0, 0.0, 0.0, 0.0]);
        let results = eng
            .hybrid_search(Some(&q), Some(&q), 5, 0.5, &[])
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].iri, "h:1");
    }

    #[tokio::test]
    async fn test_vacuum_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert("v:1", v(vec![1.0, 0.0, 0.0, 0.0]), json!({"x": "y"}))
            .await
            .unwrap();
        eng.delete("v:1").await.unwrap();
        // Vacuum should not panic on empty cleaned state
        eng.vacuum().await.unwrap();
    }

    #[tokio::test]
    async fn test_searcher() {
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_async_engine(dir.path());

        eng.insert("s:1", v(vec![1.0, 0.0, 0.0, 0.0]), json!({"x": "y"}))
            .await
            .unwrap();
        eng.insert("s:2", v(vec![0.0, 1.0, 0.0, 0.0]), json!({"x": "z"}))
            .await
            .unwrap();

        let mut srch = eng.searcher();
        let results = srch.search(&v(vec![1.0, 0.0, 0.0, 0.0]), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "s:1");
    }
}
