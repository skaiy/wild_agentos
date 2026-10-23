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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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

/// Read-only persistence health information for a vector store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistenceHealth {
    pub vector_count: u32,
    pub text_count: u32,
    pub tag_count: u32,
    pub corrupt_count: u32,
}

impl PersistenceHealth {
    pub fn is_consistent(self) -> bool {
        self.corrupt_count == 0
            && self.vector_count == self.text_count
            && self.vector_count == self.tag_count
    }

    pub fn missing_text_count(self) -> u32 {
        self.vector_count.saturating_sub(self.text_count)
    }

    pub fn missing_tag_count(self) -> u32 {
        self.vector_count.saturating_sub(self.tag_count)
    }
}

#[derive(Default)]
struct HealthCounters {
    vector_count: AtomicU32,
    text_count: AtomicU32,
    tag_count: AtomicU32,
    corrupt_count: AtomicU32,
}

impl HealthCounters {
    fn report(&self) -> PersistenceHealth {
        PersistenceHealth {
            vector_count: self.vector_count.load(Ordering::Acquire),
            text_count: self.text_count.load(Ordering::Acquire),
            tag_count: self.tag_count.load(Ordering::Acquire),
            corrupt_count: self.corrupt_count.load(Ordering::Acquire),
        }
    }

    fn reset(&self, health: PersistenceHealth) {
        self.vector_count
            .store(health.vector_count, Ordering::Release);
        self.text_count.store(health.text_count, Ordering::Release);
        self.tag_count.store(health.tag_count, Ordering::Release);
    }

    fn record_corruption(&self) {
        self.corrupt_count.fetch_add(1, Ordering::AcqRel);
    }

    fn apply_delta(counter: &AtomicU32, before: bool, after: bool) {
        match (before, after) {
            (false, true) => {
                counter.fetch_add(1, Ordering::AcqRel);
            }
            (true, false) => {
                counter.fetch_sub(1, Ordering::AcqRel);
            }
            _ => {}
        }
    }

    fn apply_entry_change(&self, before: EntryPresence, after: EntryPresence) {
        Self::apply_delta(&self.vector_count, before.vector, after.vector);
        Self::apply_delta(&self.text_count, before.text, after.text);
        Self::apply_delta(&self.tag_count, before.tags, after.tags);
    }
}

#[derive(Clone, Copy)]
struct EntryPresence {
    vector: bool,
    text: bool,
    tags: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct WalRecord {
    iri: String,
    vector: Vec<u8>,
    payload: Option<String>,
}

impl WalRecord {
    const MAGIC: &'static [u8; 4] = b"HSR1";

    fn encode(iri: &str, vector: &[u8], payload: Option<&Value>) -> Result<Vec<u8>, EngineError> {
        let mut bytes = Self::MAGIC.to_vec();
        bytes.extend(
            bincode::serialize(&Self {
                iri: iri.to_owned(),
                vector: vector.to_vec(),
                payload: payload
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| EngineError::StorageError {
                        message: format!("WAL payload serialization: {e}"),
                    })?,
            })
            .map_err(|e| EngineError::StorageError {
                message: format!("WAL record serialization: {e}"),
            })?,
        );
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Option<Self>, EngineError> {
        if !bytes.starts_with(Self::MAGIC) {
            return Ok(None);
        }
        bincode::deserialize(&bytes[Self::MAGIC.len()..])
            .map(Some)
            .map_err(|e| EngineError::StorageError {
                message: format!("WAL record deserialization: {e}"),
            })
    }
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

    /// Restore an IRI with its persisted ID during WAL replay.
    pub fn register_id(&mut self, id: u32, iri: &str) {
        self.iri_to_id.insert(iri.to_string(), id);
        self.id_to_iri.insert(id, iri.to_string());
        if id >= self.next_id {
            self.next_id = id + 1;
        }
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
        if !filters.is_empty() && allowed.is_none() {
            return Vec::new();
        }

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
    health: HealthCounters,
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
            health: HealthCounters::default(),
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
                        match serde_json::from_str(&payload_str) {
                            Ok(payload) => engine.metadata.index(id, &payload),
                            Err(error) => {
                                engine.health.record_corruption();
                                tracing::error!(
                                    id,
                                    error = %error,
                                    "Unreadable snapshot metadata; retrieval will remain unavailable"
                                );
                            }
                        }
                    }
                    // Restore deleted IDs
                    for id in snap.deleted_ids {
                        engine.metadata.remove(id);
                    }
                    drop(inner);
                }
                Err(e) => {
                    engine.health.record_corruption();
                    warn!("Failed to load snapshot, falling back to WAL replay: {e}");
                }
            }
        }
        // Replay every uncheckpointed operation, including frozen WAL
        // segments left behind by a crash during checkpoint rotation.
        engine.recover()?;
        engine.initialize_health();
        engine.log_retrieval_health();

        Ok(engine)
    }

    /// Recover state by replaying WAL records into store, index, and metadata.
    fn recover(&self) -> Result<(), EngineError> {
        let mut inner = self.inner.lock().unwrap();

        let mut total = 0;
        for wal_path in self.wal.pending_paths()? {
            total += EngineWal::replay(&wal_path, |op, ts, data| {
                inner.clock = inner.clock.max(ts);
                match op {
                    WalOp::Insert { id, .. } | WalOp::Upsert { id, .. } => {
                        let record = match WalRecord::decode(data) {
                            Ok(record) => record,
                            Err(error) => {
                                self.health.record_corruption();
                                tracing::error!(
                                    id,
                                    error = %error,
                                    "Unreadable durable vector record; retrieval will remain unavailable"
                                );
                                return;
                            }
                        };
                        let (vector_bytes, iri, payload) = match record {
                            Some(record) => (record.vector, Some(record.iri), record.payload),
                            None => {
                                warn!(
                                    id,
                                    "Recovered legacy vector record without text or metadata"
                                );
                                (data.to_vec(), None, None)
                            }
                        };
                        if let Err(error) = inner.store.set(id, &vector_bytes) {
                            self.health.record_corruption();
                            tracing::error!(
                                id,
                                error = %error,
                                "Invalid recovered vector record; retrieval will remain unavailable"
                            );
                            return;
                        }
                        if !vector_bytes.is_empty() {
                            let dim = (vector_bytes.len().saturating_sub(12)) / 8;
                            match EmbeddingVector::from_bytes(&vector_bytes, dim) {
                                Ok(vec) => inner.index.insert(id, vec),
                                Err(error) => {
                                    self.health.record_corruption();
                                    tracing::error!(
                                        id,
                                        error = %error,
                                        "Unreadable vector payload; retrieval will remain unavailable"
                                    );
                                }
                            }
                        }
                        if let Some(iri) = iri {
                            if let Ok(mut reg) = self.iri_registry.lock() {
                                reg.register_id(id, &iri);
                            }
                        }
                        if let Some(payload) = payload {
                            match serde_json::from_str(&payload) {
                                Ok(payload) => {
                                    self.metadata.index(id, &payload);
                                    self.metadata.undelete(id);
                                }
                                Err(error) => {
                                    self.health.record_corruption();
                                    tracing::error!(
                                        id,
                                        error = %error,
                                        "Unreadable durable metadata; retrieval will remain unavailable"
                                    );
                                }
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
        }

        info!("WAL replay complete: {} entries", total);
        Ok(())
    }

    fn scan_persistence_health(&self) -> PersistenceHealth {
        let inner = self.inner.lock().unwrap();
        let vector_count = inner.store.active_count();
        let (text_count, tag_count) =
            inner
                .store
                .iter_active()
                .fold((0, 0), |(text_count, tag_count), (id, _)| {
                    let payload = self.metadata.get_payload(id);
                    let has_text = payload
                        .as_ref()
                        .and_then(|payload| payload.get("text"))
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty());
                    let has_tags = payload
                        .as_ref()
                        .and_then(|payload| payload.get("tags"))
                        .is_some_and(Value::is_array);
                    (
                        text_count + u32::from(has_text),
                        tag_count + u32::from(has_tags),
                    )
                });
        PersistenceHealth {
            vector_count,
            text_count,
            tag_count,
            corrupt_count: self.health.corrupt_count.load(Ordering::Acquire),
        }
    }

    fn initialize_health(&self) {
        let health = self.scan_persistence_health();
        self.health.reset(health);
    }

    fn log_retrieval_health(&self) {
        let health = self.health.report();
        if !health.is_consistent() {
            tracing::error!(
                vector_count = health.vector_count,
                text_count = health.text_count,
                tag_count = health.tag_count,
                corrupt_count = health.corrupt_count,
                missing_text_count = health.missing_text_count(),
                missing_tag_count = health.missing_tag_count(),
                "Vector store integrity check failed; retrieval is unavailable until records are repaired"
            );
        }
    }

    /// Report vectors lacking persisted text without modifying the store.
    pub fn persistence_health_report(&self) -> PersistenceHealth {
        self.health.report()
    }

    /// Return whether every active vector has both a text payload and tags.
    pub fn is_retrieval_ready(&self) -> bool {
        self.health.report().is_consistent()
    }

    fn payload_presence(payload: Option<Value>) -> (bool, bool) {
        let text = payload
            .as_ref()
            .and_then(|payload| payload.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty());
        let tags = payload
            .as_ref()
            .and_then(|payload| payload.get("tags"))
            .is_some_and(Value::is_array);
        (text, tags)
    }

    fn entry_presence(&self, inner: &EngineInner, id: u32) -> EntryPresence {
        let (text, tags) = Self::payload_presence(self.metadata.get_payload(id));
        EntryPresence {
            vector: inner.store.get(id).is_some(),
            text,
            tags,
        }
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
        let bytes = vector.as_bytes();
        let wal_record = WalRecord::encode(iri, &bytes, Some(&jsonld))?;
        let mut inner = self.inner.lock().unwrap();
        let mut reg = self.iri_registry.lock().unwrap();
        let id = reg.register(iri);
        let before = self.entry_presence(&inner, id);
        inner.clock += 1;
        // Keep WAL append and in-memory application under one lock so two
        // writers cannot interleave the record identity and payload.
        self.wal.append(
            &WalOp::Insert {
                id,
                iri: iri.to_string(),
            },
            inner.clock,
            &wal_record,
        )?;
        inner.store.set(id, &bytes)?;
        inner.index.insert(id, vector);
        self.metadata.index(id, &jsonld);
        self.metadata.undelete(id);
        let after = self.entry_presence(&inner, id);
        self.health.apply_entry_change(before, after);

        Ok(id)
    }

    // ── Upsert ──────────────────────────────────────────────────────────────

    async fn upsert(
        &self,
        iri: &str,
        vector: EmbeddingVector,
        jsonld: Value,
    ) -> Result<u32, EngineError> {
        let bytes = vector.as_bytes();
        let wal_record = WalRecord::encode(iri, &bytes, Some(&jsonld))?;
        let mut inner = self.inner.lock().unwrap();
        let mut reg = self.iri_registry.lock().unwrap();
        let id = reg.register(iri);
        let before = self.entry_presence(&inner, id);
        inner.clock += 1;
        self.wal.append(
            &WalOp::Upsert {
                id,
                iri: iri.to_string(),
            },
            inner.clock,
            &wal_record,
        )?;
        inner.store.set(id, &bytes)?;
        inner.index.insert(id, vector);
        self.metadata.index(id, &jsonld);
        self.metadata.undelete(id);
        let after = self.entry_presence(&inner, id);
        self.health.apply_entry_change(before, after);

        Ok(id)
    }

    // ── Delete ──────────────────────────────────────────────────────────────

    async fn delete(&self, iri: &str) -> Result<(), EngineError> {
        let mut inner = self.inner.lock().unwrap();
        let reg = self.iri_registry.lock().unwrap();
        let id = reg
            .resolve(iri)
            .ok_or_else(|| EngineError::NotFound(iri.to_string()))?;
        let before = self.entry_presence(&inner, id);
        inner.clock += 1;
        self.wal.append(
            &WalOp::Delete {
                id,
                iri: iri.to_string(),
            },
            inner.clock,
            &[],
        )?;
        inner.store.remove(id);
        inner.index.remove(id);
        self.metadata.remove(id);
        self.health.apply_entry_change(
            before,
            EntryPresence {
                vector: false,
                text: false,
                tags: false,
            },
        );

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
        if !filters.is_empty() && allowed.is_none() {
            return Ok(Vec::new());
        }

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
        if !filters.is_empty() && allowed.is_none() {
            return Ok(Vec::new());
        }

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

    fn setup_engine_with_dimension(
        dir: &Path,
        dim: usize,
        config: HnswConfig,
    ) -> HyperspaceEngineImpl {
        HyperspaceEngineImpl::open(
            dir,
            WalSyncMode::Strict,
            dim,
            Box::new(CosineMetric),
            config,
        )
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

    fn synthetic_1024d_vector(index: usize, total: usize) -> EmbeddingVector {
        let angle = (index + 1) as f64 * std::f64::consts::FRAC_PI_2 / (total + 1) as f64;
        let mut coords = vec![0.0; 1024];
        coords[0] = angle.cos();
        coords[1] = angle.sin();
        EmbeddingVector::new_unchecked(coords, MetricKind::Cosine)
    }

    async fn assert_filtered_self_recall(
        engine: &HyperspaceEngineImpl,
        entries: &[(String, String, usize)],
    ) {
        for (iri, tag, vector_index) in entries {
            let query = synthetic_1024d_vector(*vector_index, entries.len());
            let filter = JsonLdFilter::tag("tags", tag);
            assert_eq!(
                engine
                    .search(&query, 1, std::slice::from_ref(&filter))
                    .await
                    .unwrap()
                    .first()
                    .map(|hit| hit.iri.as_str()),
                Some(iri.as_str()),
                "tag-filtered search should return {iri} as its own nearest neighbor"
            );
        }
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
    async fn test_wal_recovery_preserves_text_and_tags_without_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        {
            let eng = setup_async_engine(dir.path());
            eng.insert(
                "record:1",
                v(vec![1.0, 0.0, 0.0, 0.0]),
                json!({"text": "persisted content", "tags": ["kb:alpha"]}),
            )
            .await
            .unwrap();
            assert!(dir.path().join("active.wal").metadata().unwrap().len() > 0);
        }

        assert!(dir.path().join("active.wal").metadata().unwrap().len() > 0);
        let replayed = EngineWal::replay(&dir.path().join("active.wal"), |_, _, data| {
            assert!(WalRecord::decode(data).unwrap().unwrap().payload.is_some());
        })
        .unwrap();
        assert_eq!(replayed, 1);
        let eng = setup_async_engine(dir.path());
        assert_eq!(eng.count().await.unwrap(), 1);
        assert_eq!(
            eng.get_payload("record:1").await.unwrap().unwrap()["text"],
            "persisted content"
        );
        let results = eng
            .search(
                &v(vec![1.0, 0.0, 0.0, 0.0]),
                5,
                &[JsonLdFilter::tag("tags", "kb:alpha")],
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "record:1");
        assert_eq!(
            results[0].payload.as_ref().unwrap()["text"],
            "persisted content"
        );
        assert_eq!(eng.persistence_health_report().missing_text_count(), 0);
    }

    #[tokio::test]
    async fn test_recovery_replays_frozen_wal_after_interrupted_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        {
            let eng = setup_async_engine(dir.path());
            eng.insert(
                "record:2",
                v(vec![1.0, 0.0, 0.0, 0.0]),
                json!({"text": "atomic record", "tags": ["kb:beta"]}),
            )
            .await
            .unwrap();
            eng.wal.rotate().unwrap();
        }

        let eng = setup_async_engine(dir.path());
        assert_eq!(eng.count().await.unwrap(), 1);
        let results = eng
            .search(
                &v(vec![1.0, 0.0, 0.0, 0.0]),
                5,
                &[JsonLdFilter::tag("tags", "kb:beta")],
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].payload.as_ref().unwrap()["text"],
            "atomic record"
        );
    }

    #[tokio::test]
    async fn test_legacy_vector_records_are_reported_as_missing_text() {
        let dir = tempfile::tempdir().unwrap();
        let raw_vector = v(vec![1.0, 0.0, 0.0, 0.0]).as_bytes();
        {
            let wal = EngineWal::open(dir.path(), WalSyncMode::Strict).unwrap();
            wal.append(
                &WalOp::Insert {
                    id: 1,
                    iri: "legacy:1".into(),
                },
                1,
                &raw_vector,
            )
            .unwrap();
        }

        let eng = setup_engine(dir.path());
        assert_eq!(
            eng.persistence_health_report(),
            PersistenceHealth {
                vector_count: 1,
                text_count: 0,
                tag_count: 0,
                corrupt_count: 0,
            }
        );
        assert!(!eng.is_retrieval_ready());
    }

    #[test]
    fn test_unreadable_wal_record_blocks_retrieval() {
        let dir = tempfile::tempdir().unwrap();
        {
            let wal = EngineWal::open(dir.path(), WalSyncMode::Strict).unwrap();
            wal.append(
                &WalOp::Insert {
                    id: 1,
                    iri: "corrupt:1".into(),
                },
                1,
                b"HSR1not-a-valid-record",
            )
            .unwrap();
        }

        let eng = setup_engine(dir.path());
        assert_eq!(eng.persistence_health_report().corrupt_count, 1);
        assert!(!eng.is_retrieval_ready());
    }

    #[tokio::test]
    async fn test_concurrent_writes_keep_vectors_text_and_tags_paired() {
        let dir = tempfile::tempdir().unwrap();
        let eng = std::sync::Arc::new(
            HyperspaceEngineImpl::open(
                dir.path(),
                WalSyncMode::Strict,
                16,
                Box::new(CosineMetric),
                HnswConfig::default(),
            )
            .unwrap(),
        );
        let mut tasks = tokio::task::JoinSet::new();

        for index in 0..16 {
            let eng = std::sync::Arc::clone(&eng);
            tasks.spawn(async move {
                let mut coords = vec![0.0; 16];
                coords[index] = 1.0;
                eng.insert(
                    &format!("record:{index}"),
                    v(coords),
                    json!({
                        "text": format!("parallel content {index}"),
                        "tags": [format!("kb:{index}")],
                    }),
                )
                .await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }

        for index in 0..16 {
            let mut coords = vec![0.0; 16];
            coords[index] = 1.0;
            let results = eng.search(&v(coords), 1, &[]).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].iri, format!("record:{index}"));
            let payload = results[0].payload.as_ref().unwrap();
            assert_eq!(payload["text"], format!("parallel content {index}"));
            assert_eq!(payload["tags"][0], format!("kb:{index}"));
        }
        assert!(eng.is_retrieval_ready());
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
    async fn test_1024d_multi_collection_filtered_self_recall_survives_restart() {
        const LARGE_COLLECTION_SIZE: usize = 3_000;
        const SMALL_COLLECTION_SIZES: [usize; 3] = [3, 11, 50];
        let total = LARGE_COLLECTION_SIZE + SMALL_COLLECTION_SIZES.iter().sum::<usize>();
        let config = HnswConfig {
            m: 16,
            ef_search: 50,
            ..HnswConfig::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mut entries = Vec::with_capacity(total);

        {
            let engine = setup_engine_with_dimension(dir.path(), 1024, config.clone());
            let mut vector_index = 0;
            for (tag, collection_size) in
                std::iter::once(("synthetic-large", LARGE_COLLECTION_SIZE)).chain(
                    SMALL_COLLECTION_SIZES
                        .iter()
                        .enumerate()
                        .map(|(index, &size)| {
                            (
                                match index {
                                    0 => "synthetic-small-a",
                                    1 => "synthetic-small-b",
                                    _ => "synthetic-small-c",
                                },
                                size,
                            )
                        }),
                )
            {
                for local_index in 0..collection_size {
                    let iri = format!("synthetic:{tag}:{local_index}");
                    engine
                        .insert(
                            &iri,
                            synthetic_1024d_vector(vector_index, total),
                            json!({"tags": [tag]}),
                        )
                        .await
                        .unwrap();
                    entries.push((iri, tag.to_owned(), vector_index));
                    vector_index += 1;
                }
            }
            assert_filtered_self_recall(&engine, &entries).await;
        }

        // Reopening without a checkpoint exercises WAL replay.
        {
            let engine = setup_engine_with_dimension(dir.path(), 1024, config.clone());
            assert_filtered_self_recall(&engine, &entries).await;
            engine.checkpoint().await.unwrap();
        }

        // Reopening after a checkpoint exercises snapshot graph restoration.
        let engine = setup_engine_with_dimension(dir.path(), 1024, config);
        assert_filtered_self_recall(&engine, &entries).await;
    }

    #[tokio::test]
    async fn test_tag_filtering_does_not_return_other_tenant_vectors() {
        let config = HnswConfig {
            m: 16,
            ef_search: 50,
            level_mult: 0.0,
            ..HnswConfig::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_engine_with_config(dir.path(), config);
        let tenant_a_filter = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("tags", "tenant:exact-a"),
            JsonLdFilter::tag("tags", "kb:exact-a"),
        ]);
        let tenant_b_filter = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("tags", "tenant:exact-b"),
            JsonLdFilter::tag("tags", "kb:exact-b"),
        ]);

        for index in 0..64 {
            eng.insert(
                &format!("tenant-a:{index}"),
                sequential_unit_vector(index * 2, 128),
                json!({"tags": ["tenant:exact-a", "kb:exact-a"]}),
            )
            .await
            .unwrap();
            eng.insert(
                &format!("tenant-b:{index}"),
                sequential_unit_vector(index * 2 + 1, 128),
                json!({"tags": ["tenant:exact-b", "kb:exact-b"]}),
            )
            .await
            .unwrap();
        }

        let query_index = 23;
        let query = sequential_unit_vector(query_index * 2 + 1, 128);
        let tenant_a_results = eng
            .search(&query, 10, std::slice::from_ref(&tenant_a_filter))
            .await
            .unwrap();
        assert!(
            !tenant_a_results.is_empty(),
            "tenant A should receive its own filtered results"
        );
        assert!(tenant_a_results
            .iter()
            .all(|hit| hit.iri.starts_with("tenant-a:")));

        let tenant_b_results = eng
            .search(&query, 1, std::slice::from_ref(&tenant_b_filter))
            .await
            .unwrap();
        assert_eq!(
            tenant_b_results.first().map(|hit| hit.iri.as_str()),
            Some("tenant-b:23")
        );

        for missing_scope in [
            vec![
                JsonLdFilter::tag("tags", "tenant:absent"),
                JsonLdFilter::tag("tags", "kb:exact-b"),
            ],
            vec![
                JsonLdFilter::tag("tags", "tenant:exact-a"),
                JsonLdFilter::tag("tags", "kb:absent"),
            ],
        ] {
            assert!(eng
                .search(&query, 10, &missing_scope)
                .await
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn test_tenant_and_kb_filters_prevent_cross_scope_results_in_hnsw_path() {
        const FILTERED_HNSW_SIZE: usize = 5_001;
        let config = HnswConfig {
            m: 4,
            m0: 8,
            ef_search: 8,
            ..HnswConfig::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let eng = setup_engine_with_config(dir.path(), config);
        let scope_b = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("tags", "tenant:scope-b"),
            JsonLdFilter::tag("tags", "kb:scope-b"),
        ]);

        eng.insert(
            "synthetic:scope-a:0",
            sequential_unit_vector(0, FILTERED_HNSW_SIZE + 1),
            json!({"tags": ["tenant:scope-a", "kb:scope-a"]}),
        )
        .await
        .unwrap();
        for index in 0..FILTERED_HNSW_SIZE {
            eng.insert(
                &format!("synthetic:scope-b:{index}"),
                sequential_unit_vector(index + 1, FILTERED_HNSW_SIZE + 1),
                json!({"tags": ["tenant:scope-b", "kb:scope-b"]}),
            )
            .await
            .unwrap();
        }

        let b_query = sequential_unit_vector(24, FILTERED_HNSW_SIZE + 1);
        let b_results = eng.search(&b_query, 10, &[scope_b]).await.unwrap();
        assert!(
            !b_results.is_empty(),
            "the HNSW path should return entries for the authorized scope"
        );
        assert!(b_results
            .iter()
            .all(|hit| hit.iri.starts_with("synthetic:scope-b:")));

        for missing_scope in [
            vec![
                JsonLdFilter::tag("tags", "tenant:absent"),
                JsonLdFilter::tag("tags", "kb:scope-b"),
            ],
            vec![
                JsonLdFilter::tag("tags", "tenant:scope-a"),
                JsonLdFilter::tag("tags", "kb:absent"),
            ],
        ] {
            assert!(eng
                .search(&b_query, 10, &missing_scope)
                .await
                .unwrap()
                .is_empty());
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
