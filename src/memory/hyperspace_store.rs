use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use hyperspace_engine::engine::{HyperspaceEngine, HyperspaceEngineImpl, SearchHit};
use hyperspace_engine::filter::JsonLdFilter;
use hyperspace_engine::hnsw::HnswConfig;
use hyperspace_engine::hyper_vector::{EmbeddingVector, MetricKind};
use hyperspace_engine::metric::CosineMetric;
use hyperspace_engine::wal::WalSyncMode;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::isolation::IsolationClaims;
use crate::memory::embedding_service::EmbeddingService;
use crate::CoreError;

pub const EXACT_SCAN_RECORD_LIMIT: usize = 20_000;

#[derive(Debug, Clone)]
pub struct ExactMatchResult {
    pub entries: Vec<ScoredEntry>,
    pub truncated: bool,
}
/// Search filter combining tag matching, type filtering, and importance range.
///
/// Mirrors the original qdrant-based filter interface for backward compatibility
/// while mapping cleanly to `JsonLdFilter` for HyperspaceEngine.
#[derive(Debug, Clone, Default)]
pub struct HybridSearchFilter {
    pub must_tags: Vec<String>,
    pub should_tags: Vec<String>,
    pub must_not_tags: Vec<String>,
    pub min_importance: Option<f32>,
    pub jsonld_types: Vec<String>,
    pub named_graph: Option<String>,
    /// 多租户向量隔离：仅返回归属于此租户的向量条目（以 `tenant:<id>` 标签写入索引）。
    pub tenant_id: Option<String>,
    /// Only return entries stored after this Unix timestamp (seconds)
    pub created_after: Option<f64>,
    /// Only return entries stored before this Unix timestamp (seconds)
    pub created_before: Option<f64>,
}

impl HybridSearchFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_must_tags(mut self, tags: Vec<String>) -> Self {
        self.must_tags = tags;
        self
    }

    pub fn with_should_tags(mut self, tags: Vec<String>) -> Self {
        self.should_tags = tags;
        self
    }

    pub fn with_must_not_tags(mut self, tags: Vec<String>) -> Self {
        self.must_not_tags = tags;
        self
    }

    pub fn with_min_importance(mut self, min: f32) -> Self {
        self.min_importance = Some(min);
        self
    }

    pub fn with_jsonld_types(mut self, types: Vec<String>) -> Self {
        self.jsonld_types = types;
        self
    }

    pub fn with_named_graph(mut self, graph: String) -> Self {
        self.named_graph = Some(graph);
        self
    }

    /// 限定只返回属于指定租户的向量条目（多租户向量隔离）。
    /// 写入时需同时调用 `upsert_with_tenant`，或在 tags 中添加 `"tenant:<tenant_id>"`。
    pub fn with_tenant(mut self, tenant_id: &str) -> Self {
        self.tenant_id = Some(tenant_id.to_string());
        self
    }

    /// Filter to only entries stored after this Unix timestamp (seconds)
    pub fn with_created_after(mut self, timestamp: f64) -> Self {
        self.created_after = Some(timestamp);
        self
    }

    /// Filter to only entries stored before this Unix timestamp (seconds)
    pub fn with_created_before(mut self, timestamp: f64) -> Self {
        self.created_before = Some(timestamp);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.must_tags.is_empty()
            && self.should_tags.is_empty()
            && self.must_not_tags.is_empty()
            && self.min_importance.is_none()
            && self.jsonld_types.is_empty()
            && self.named_graph.is_none()
            && self.tenant_id.is_none()
            && self.created_after.is_none()
            && self.created_before.is_none()
    }
}

/// Single search result from the vector store.
#[derive(Debug, Clone)]
pub struct ScoredEntry {
    pub iri: String,
    pub text: String,
    pub score: f32,
    pub match_kind: MatchKind,
    pub tags: Vec<String>,
    pub importance: Option<f32>,
    pub jsonld_types: Vec<String>,
    /// Unix timestamp (seconds) when this entry was stored
    pub stored_at: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Vector,
    Exact,
}

/// In-memory vector store backed by HyperspaceEngine.
///
/// Replaces the old Qdrant-based VectorStore. Wraps `HyperspaceEngineImpl`
/// for HNSW ANN search + `Arc<dyn EmbeddingService>` for text→vector conversion.
/// All public methods mirror the old API so callers (ProjectionEngine,
/// SkillDiscoveryEngine) work with minimal changes.
pub struct HyperspaceStore {
    engine: Arc<HyperspaceEngineImpl>,
    embed: Arc<dyn EmbeddingService>,
    ownership_cache: RwLock<OwnershipCacheState>,
    ownership_generation: AtomicU64,
}

#[derive(Default)]
struct OwnershipCacheState {
    entries: HashMap<String, (u64, bool)>,
}

#[cfg(not(test))]
fn unverified_claims_error(operation: &str) -> CoreError {
    CoreError::Internal {
        message: format!("{operation} requires verified IsolationClaims"),
    }
}

impl HyperspaceStore {
    /// Open or create a HyperspaceEngine-backed vector store.
    ///
    /// `data_dir` — persistent storage directory (WAL + snapshots + HNSW index).
    /// `embed` — embedding service that determines the vector dimension.
    pub fn open(data_dir: &Path, embed: Arc<dyn EmbeddingService>) -> Result<Self, CoreError> {
        let dim = embed.dimension();
        let engine = HyperspaceEngineImpl::open(
            data_dir,
            WalSyncMode::Batch { interval_ms: 100 },
            dim,
            Box::new(CosineMetric),
            HnswConfig::default(),
        )
        .map_err(|e| CoreError::Internal {
            message: format!("HyperspaceEngine init: {e}"),
        })?;
        info!(dim = dim, "HyperspaceEngine opened");
        Ok(Self {
            engine: Arc::new(engine),
            embed,
            ownership_cache: RwLock::new(OwnershipCacheState::default()),
            ownership_generation: AtomicU64::new(0),
        })
    }

    /// 当前向量维度（由注入的 embedding 服务决定）。用于配置热切换时判断维度是否变化。
    pub fn dimension(&self) -> usize {
        self.embed.dimension()
    }

    /// Whether every persisted vector has both a text payload and tags.
    pub fn is_retrieval_ready(&self) -> bool {
        self.engine.is_retrieval_ready()
    }

    /// Drops cached KB ownership-validation results after a vector mutation.
    pub fn invalidate_ownership_cache(&self) {
        let mut cache = self
            .ownership_cache
            .write()
            .unwrap_or_else(|error| error.into_inner());
        self.ownership_generation.fetch_add(1, Ordering::AcqRel);
        cache.entries.clear();
    }

    pub fn ownership_generation(&self, _key: &str) -> u64 {
        self.ownership_generation.load(Ordering::Acquire)
    }

    pub fn cached_ownership_health(&self, key: &str) -> Option<bool> {
        let cache = self
            .ownership_cache
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let generation = self.ownership_generation.load(Ordering::Acquire);
        cache
            .entries
            .get(key)
            .and_then(|(cached_generation, healthy)| {
                (*cached_generation == generation).then_some(*healthy)
            })
    }

    pub fn cache_ownership_health_if_current(&self, key: String, generation: u64, healthy: bool) {
        let mut cache = self
            .ownership_cache
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if self.ownership_generation.load(Ordering::Acquire) == generation {
            cache.entries.insert(key, (generation, healthy));
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Obtains the embedding used by every vector query and write.
    ///
    /// An unavailable provider must fail the operation rather than silently
    /// substituting a placeholder vector.
    async fn get_query_embedding(&self, text: &str) -> Result<Vec<f32>, CoreError> {
        let embedding = self.embed.embed(text).await.map_err(|error| {
            warn!(error = %error, "Embedding service failed; vector operation rejected");
            CoreError::StorageError {
                message: "embedding provider unavailable".into(),
            }
        })?;
        if embedding.len() != self.embed.dimension()
            || embedding.iter().any(|value| !value.is_finite())
            || embedding.iter().all(|value| *value == 0.0)
        {
            return Err(CoreError::StorageError {
                message: "embedding provider returned an invalid vector".into(),
            });
        }
        Ok(embedding)
    }

    /// Convert a `HybridSearchFilter` to a `Vec<JsonLdFilter>` for the engine.
    ///
    /// Semantics matches the original Qdrant filter:
    /// - `must_tags` / `jsonld_types` / `named_graph` / `min_importance` / `tenant_id` → ANDed together
    /// - `should_tags` → OR (at least one must match)
    /// - `must_not_tags` → NOT (none must match)
    ///
    /// `tenant_id` 转化为 must_tag `tenant:<id>`，与写入时的 tag 约定一致。
    fn to_jsonld_filters(&self, filter: &HybridSearchFilter) -> Vec<JsonLdFilter> {
        if filter.is_empty() {
            return vec![];
        }

        let mut engine_filters: Vec<JsonLdFilter> = Vec::new();

        // Must group (AND of all must conditions)
        let mut must_children: Vec<JsonLdFilter> = Vec::new();
        for tag in &filter.must_tags {
            must_children.push(JsonLdFilter::tag("tags", tag));
        }
        for type_iri in &filter.jsonld_types {
            must_children.push(JsonLdFilter::Type(type_iri.clone()));
        }
        if let Some(ref graph) = filter.named_graph {
            must_children.push(JsonLdFilter::NamedGraph(graph.clone()));
        }
        if let Some(min) = filter.min_importance {
            must_children.push(JsonLdFilter::Range {
                key: "importance".into(),
                gte: Some(min as f64),
                lte: None,
            });
        }
        // 多租户向量隔离：tenant_id → must_tag "tenant:<id>"
        if let Some(ref tid) = filter.tenant_id {
            must_children.push(JsonLdFilter::tag("tags", format!("tenant:{}", tid)));
        }
        if let Some(after) = filter.created_after {
            must_children.push(JsonLdFilter::Range {
                key: "stored_at".into(),
                gte: Some(after),
                lte: None,
            });
        }
        if let Some(before) = filter.created_before {
            must_children.push(JsonLdFilter::Range {
                key: "stored_at".into(),
                gte: None,
                lte: Some(before),
            });
        }
        if !must_children.is_empty() {
            engine_filters.push(JsonLdFilter::Must(must_children));
        }

        // Should group (OR — at least one should match)
        if !filter.should_tags.is_empty() {
            let should_children: Vec<JsonLdFilter> = filter
                .should_tags
                .iter()
                .map(|t| JsonLdFilter::tag("tags", t))
                .collect();
            engine_filters.push(JsonLdFilter::Should(should_children));
        }

        // MustNot group (NONE must match)
        if !filter.must_not_tags.is_empty() {
            let must_not_children: Vec<JsonLdFilter> = filter
                .must_not_tags
                .iter()
                .map(|t| JsonLdFilter::tag("tags", t))
                .collect();
            engine_filters.push(JsonLdFilter::MustNot(must_not_children));
        }

        engine_filters
    }

    /// Convert engine `SearchHit`s into `ScoredEntry`s (extracting payload fields).
    fn scored_hits_to_entries(hits: Vec<SearchHit>) -> Vec<ScoredEntry> {
        hits.into_iter()
            .map(|hit| {
                let (text, tags, importance, jsonld_types, stored_at) = hit
                    .payload
                    .as_ref()
                    .map(|p| {
                        let text = p
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let tags = p
                            .get("tags")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let importance = p
                            .get("importance")
                            .and_then(|v| v.as_f64().map(|f| f as f32));
                        let jsonld_types = p
                            .get("@type")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let stored_at = p.get("stored_at").and_then(|v| v.as_f64());
                        (text, tags, importance, jsonld_types, stored_at)
                    })
                    .unwrap_or_default();

                ScoredEntry {
                    iri: hit.iri,
                    text,
                    score: hit.score,
                    match_kind: MatchKind::Vector,
                    tags,
                    importance,
                    jsonld_types,
                    stored_at,
                }
            })
            .collect()
    }

    // ── Public API (mirrors old VectorStore) ─────────────────────────────────

    /// Legacy unscoped write API.
    ///
    /// Production callers must use [`Self::upsert_with_claims`] so the vector
    /// is stored in a namespace minted from verified claims. Unit tests retain
    /// this API as a fixture compatibility shim.
    #[cfg(not(test))]
    pub async fn upsert(
        &self,
        _iri: &str,
        _text: &str,
        _tags: &[String],
    ) -> Result<u32, CoreError> {
        Err(unverified_claims_error("vector upsert"))
    }

    #[cfg(test)]
    pub async fn upsert_with_tenant(
        &self,
        iri: &str,
        text: &str,
        tags: &[String],
        tenant_id: &str,
    ) -> Result<u32, CoreError> {
        let mut all_tags = tags.to_vec();
        all_tags.push(format!("tenant:{}", tenant_id));
        self.upsert_with_metadata(iri, text, &all_tags, None, None, None)
            .await
    }

    /// Legacy unscoped write API retained only for unit-test fixtures.
    #[cfg(test)]
    pub async fn upsert(&self, iri: &str, text: &str, tags: &[String]) -> Result<u32, CoreError> {
        self.upsert_with_metadata(iri, text, tags, None, None, None)
            .await
    }

    /// Legacy tenant-tag write API.
    ///
    /// A caller-provided tenant string is not a verified vector namespace and
    /// therefore cannot be used for production writes.
    #[cfg(not(test))]
    pub async fn upsert_with_tenant(
        &self,
        _iri: &str,
        _text: &str,
        _tags: &[String],
        _tenant_id: &str,
    ) -> Result<u32, CoreError> {
        Err(unverified_claims_error("vector upsert"))
    }

    /// Stores an entry in the namespace minted from verified claims.
    ///
    /// The engine has no namespace primitive, so the minted namespace scopes
    /// both the engine IRI and the JSON-LD named-graph metadata. This prevents
    /// equal caller-supplied IRIs in different tenants/projects from
    /// overwriting one another and makes every claims-aware search filter on
    /// the same scope.
    pub async fn upsert_with_claims(
        &self,
        claims: &IsolationClaims,
        iri: &str,
        text: &str,
        tags: &[String],
    ) -> Result<u32, CoreError> {
        self.upsert_with_claims_and_metadata(claims, iri, text, tags, None, None)
            .await
    }

    /// Stores an entry with metadata in the namespace minted from verified
    /// claims.
    pub async fn upsert_with_claims_and_metadata(
        &self,
        claims: &IsolationClaims,
        iri: &str,
        text: &str,
        tags: &[String],
        importance: Option<f32>,
        jsonld_types: Option<&[String]>,
    ) -> Result<u32, CoreError> {
        let namespace = claims.vector_namespace().map_err(|e| CoreError::Internal {
            message: format!("invalid verified vector namespace: {e}"),
        })?;
        let scoped_iri = format!("{namespace}#{iri}");
        let mut scoped_tags = tags.to_vec();
        let tenant_tag = format!("tenant:{}", claims.tenant_id());
        if !scoped_tags.contains(&tenant_tag) {
            scoped_tags.push(tenant_tag);
        }
        self.upsert_with_metadata_inner(
            &scoped_iri,
            text,
            &scoped_tags,
            importance,
            jsonld_types,
            Some(&namespace),
        )
        .await
    }

    /// Store a vector entry with full metadata.
    ///
    /// Production callers must use
    /// [`Self::upsert_with_claims_and_metadata`]. This unscoped API is kept
    /// for existing unit-test fixtures only.
    #[cfg(not(test))]
    pub async fn upsert_with_metadata(
        &self,
        _iri: &str,
        _text: &str,
        _tags: &[String],
        _importance: Option<f32>,
        _jsonld_types: Option<&[String]>,
        _named_graph: Option<&str>,
    ) -> Result<u32, CoreError> {
        Err(unverified_claims_error("vector upsert"))
    }

    #[cfg(test)]
    pub async fn upsert_with_metadata(
        &self,
        iri: &str,
        text: &str,
        tags: &[String],
        importance: Option<f32>,
        jsonld_types: Option<&[String]>,
        named_graph: Option<&str>,
    ) -> Result<u32, CoreError> {
        self.upsert_with_metadata_inner(iri, text, tags, importance, jsonld_types, named_graph)
            .await
    }

    async fn upsert_with_metadata_inner(
        &self,
        iri: &str,
        text: &str,
        tags: &[String],
        importance: Option<f32>,
        jsonld_types: Option<&[String]>,
        named_graph: Option<&str>,
    ) -> Result<u32, CoreError> {
        if text.trim().is_empty() {
            return Err(CoreError::ValidationFailed {
                message: "vector text must not be empty".into(),
            });
        }
        let vector = self.get_query_embedding(text).await?;
        let vec = EmbeddingVector::from_f32_slice(&vector, MetricKind::Cosine).map_err(|e| {
            CoreError::Internal {
                message: format!("EmbeddingVector: {e}"),
            }
        })?;

        let mut payload = serde_json::Map::new();
        payload.insert("iri".into(), Value::String(iri.into()));
        // Store current Unix timestamp for time-based filtering
        let now_ts = Utc::now().timestamp() as f64;
        payload.insert(
            "stored_at".into(),
            Value::Number(
                serde_json::Number::from_f64(now_ts).unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        );
        payload.insert(
            "text".into(),
            Value::String(text.chars().take(500).collect()),
        );
        payload.insert(
            "tags".into(),
            Value::Array(tags.iter().map(|t| Value::String(t.clone())).collect()),
        );

        if let Some(imp) = importance {
            payload.insert(
                "importance".into(),
                Value::Number(
                    serde_json::Number::from_f64(imp as f64)
                        .unwrap_or_else(|| serde_json::Number::from(0)),
                ),
            );
        }
        if let Some(types) = jsonld_types {
            payload.insert(
                "@type".into(),
                Value::Array(types.iter().map(|t| Value::String(t.clone())).collect()),
            );
        }
        if let Some(graph) = named_graph {
            payload.insert("named_graph".into(), Value::String(graph.to_string()));
        }

        let point_id = self
            .engine
            .upsert(iri, vec, Value::Object(payload))
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace upsert: {e}"),
            })?;
        self.invalidate_ownership_cache();

        debug!(iri = %iri, point_id = point_id, "Vector stored via HyperspaceEngine");
        Ok(point_id)
    }

    /// Semantic search by query string.
    #[cfg(not(test))]
    pub async fn search(&self, _query: &str, _limit: u64) -> Result<Vec<ScoredEntry>, CoreError> {
        Err(unverified_claims_error("vector search"))
    }

    #[cfg(test)]
    pub async fn search(&self, query: &str, limit: u64) -> Result<Vec<ScoredEntry>, CoreError> {
        self.search_with_filter(query, &HybridSearchFilter::new(), limit)
            .await
    }

    /// Semantic search with metadata filters.
    ///
    /// Production callers must use [`Self::search_with_claims`], which
    /// overrides any caller-provided named graph with the namespace minted
    /// from verified claims.
    #[cfg(not(test))]
    pub async fn search_with_filter(
        &self,
        _query: &str,
        _filter: &HybridSearchFilter,
        _limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        Err(unverified_claims_error("vector search"))
    }

    #[cfg(test)]
    pub async fn search_with_filter(
        &self,
        query: &str,
        filter: &HybridSearchFilter,
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        let vector = self.get_query_embedding(query).await?;
        let vec = EmbeddingVector::from_f32_slice(&vector, MetricKind::Cosine).map_err(|e| {
            CoreError::Internal {
                message: format!("EmbeddingVector: {e}"),
            }
        })?;

        let filters = self.to_jsonld_filters(filter);
        let results = self
            .engine
            .search(&vec, limit as usize, &filters)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace search: {e}"),
            })?;

        Ok(Self::scored_hits_to_entries(results))
    }

    /// Searches only entries in the vector namespace minted from verified
    /// claims. Historical `tenant:<id>` rows have a different named graph and
    /// are intentionally not included or migrated.
    pub async fn search_with_claims(
        &self,
        claims: &IsolationClaims,
        query: &str,
        filter: &HybridSearchFilter,
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        let namespace = claims.vector_namespace().map_err(|e| CoreError::Internal {
            message: format!("invalid verified vector namespace: {e}"),
        })?;
        let mut scoped_filter = filter.clone();
        let namespace_prefix = format!("{namespace}#");
        scoped_filter.named_graph = Some(namespace);
        // Hyperspace treats an empty filter bitmap as an unfiltered search.
        // Retain only the scoped engine IRIs as a fail-closed guard when the
        // requested namespace has no indexed vectors.
        Ok(self
            .search_with_filter_inner(query, &scoped_filter, limit)
            .await?
            .into_iter()
            .filter(|entry| entry.iri.starts_with(&namespace_prefix))
            .collect())
    }

    /// Searches the claims namespace and requires server-owned entry tags.
    ///
    /// The engine filter narrows candidates, then the returned payload tags are
    /// checked again. The latter is a fail-closed guard for engine backends
    /// that treat a metadata-filter miss as an unfiltered search.
    pub async fn search_with_claims_and_required_tags(
        &self,
        claims: &IsolationClaims,
        query: &str,
        required_tags: &[String],
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        let filter = HybridSearchFilter::new().with_must_tags(required_tags.to_vec());
        let candidate_limit = self.count().await?.min(usize::MAX as u64);
        Ok(self
            .search_with_claims(claims, query, &filter, candidate_limit)
            .await?
            .into_iter()
            .filter(|entry| entry_matches_claims_and_tags(entry, claims, required_tags))
            .take(limit as usize)
            .collect())
    }

    /// Finds literal ASCII token matches only after applying the same verified
    /// namespace and required-tag filters used for semantic retrieval.
    pub async fn exact_matches_with_claims_and_required_tags(
        &self,
        claims: &IsolationClaims,
        required_tags: &[String],
        tokens: &[String],
        limit: u64,
    ) -> Result<ExactMatchResult, CoreError> {
        self.exact_matches_with_claims_and_required_tags_capped(
            claims,
            required_tags,
            tokens,
            limit,
            EXACT_SCAN_RECORD_LIMIT,
        )
        .await
    }

    async fn exact_matches_with_claims_and_required_tags_capped(
        &self,
        claims: &IsolationClaims,
        required_tags: &[String],
        tokens: &[String],
        limit: u64,
        scan_cap: usize,
    ) -> Result<ExactMatchResult, CoreError> {
        if tokens.is_empty() {
            return Ok(ExactMatchResult {
                entries: Vec::new(),
                truncated: false,
            });
        }
        let namespace = claims.vector_namespace().map_err(|e| CoreError::Internal {
            message: format!("invalid verified vector namespace: {e}"),
        })?;
        let filter = HybridSearchFilter::new()
            .with_must_tags(required_tags.to_vec())
            .with_named_graph(namespace);
        let filters = self.to_jsonld_filters(&filter);
        let candidates = self
            .engine
            .list_filtered(&filters, scan_cap + 1)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace filtered list: {e}"),
            })?;
        let truncated = candidates.len() > scan_cap;
        if truncated {
            warn!(
                scanned = scan_cap,
                cap = scan_cap,
                "exact retrieval scan cap reached"
            );
        }
        let mut matches: Vec<ScoredEntry> = Self::scored_hits_to_entries(candidates)
            .into_iter()
            .take(scan_cap)
            .filter(|entry| entry_matches_claims_and_tags(entry, claims, required_tags))
            .filter(|entry| {
                tokens
                    .iter()
                    .any(|token| contains_ascii_token(&entry.text, token))
            })
            .collect();
        // Stable tiers: an entire chunk equal to a requested token precedes a
        // token merely contained within longer chunk text.
        matches.sort_by_key(|entry| {
            if tokens
                .iter()
                .any(|token| entry.text.eq_ignore_ascii_case(token))
            {
                0
            } else {
                1
            }
        });
        matches.truncate(limit as usize);
        Ok(ExactMatchResult {
            entries: matches,
            truncated,
        })
    }

    async fn search_with_filter_inner(
        &self,
        query: &str,
        filter: &HybridSearchFilter,
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        if !self.is_retrieval_ready() {
            return Err(CoreError::StorageError {
                message: "vector store integrity check failed; retrieval unavailable".into(),
            });
        }
        let vector = self.get_query_embedding(query).await?;
        let vec = EmbeddingVector::from_f32_slice(&vector, MetricKind::Cosine).map_err(|e| {
            CoreError::Internal {
                message: format!("EmbeddingVector: {e}"),
            }
        })?;

        let filters = self.to_jsonld_filters(filter);
        let results = self
            .engine
            .search(&vec, limit as usize, &filters)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace search: {e}"),
            })?;

        Ok(Self::scored_hits_to_entries(results))
    }

    /// Search with exponential time-decay applied to scores.
    ///
    /// After fetching results, each entry's score is multiplied by
    /// `exp(-λ * hours_since_stored)` — older entries are penalised.
    /// The results are then re-sorted by the new score.
    ///
    /// `decay_lambda = 0.0` → no decay (identical to `search_with_filter`).
    pub async fn search_with_time_decay(
        &self,
        query: &str,
        filter: &HybridSearchFilter,
        decay_lambda: f64,
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        let mut results = self.search_with_filter(query, filter, limit).await?;
        let now = Utc::now();
        for entry in results.iter_mut() {
            if let Some(stored_at) = entry.stored_at {
                let age_secs = now.timestamp() as f64 - stored_at;
                let age_hours = age_secs / 3600.0;
                if age_hours > 0.0 {
                    entry.score *= (-decay_lambda * age_hours).exp() as f32;
                }
            }
        }
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    /// Search by tag match (uses combined tag string as query).
    pub async fn search_by_tags(
        &self,
        tags: &[String],
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let query = tags.join(" ");
        let filter = HybridSearchFilter::new().with_must_tags(tags.to_vec());
        self.search_with_filter(&query, &filter, limit).await
    }

    /// Hybrid search combining free-text and tag filtering.
    #[cfg(not(test))]
    pub async fn hybrid_search(
        &self,
        _query: &str,
        _must_tags: &[String],
        _should_tags: &[String],
        _min_importance: Option<f32>,
        _limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        Err(unverified_claims_error("vector hybrid search"))
    }

    #[cfg(test)]
    pub async fn hybrid_search(
        &self,
        query: &str,
        must_tags: &[String],
        should_tags: &[String],
        min_importance: Option<f32>,
        limit: u64,
    ) -> Result<Vec<ScoredEntry>, CoreError> {
        let mut filter = HybridSearchFilter::new()
            .with_must_tags(must_tags.to_vec())
            .with_should_tags(should_tags.to_vec());
        if let Some(min) = min_importance {
            filter = filter.with_min_importance(min);
        }
        self.search_with_filter(query, &filter, limit).await
    }

    /// Delete a vector entry by IRI.
    #[cfg(not(test))]
    pub async fn delete(&self, _iri: &str) -> Result<(), CoreError> {
        Err(unverified_claims_error("vector delete"))
    }

    #[cfg(test)]
    pub async fn delete(&self, iri: &str) -> Result<(), CoreError> {
        self.delete_inner(iri).await
    }

    /// Deletes only the entry addressed inside the verified claims namespace.
    pub async fn delete_with_claims(
        &self,
        claims: &IsolationClaims,
        iri: &str,
    ) -> Result<(), CoreError> {
        let namespace = claims.vector_namespace().map_err(|e| CoreError::Internal {
            message: format!("invalid verified vector namespace: {e}"),
        })?;
        self.delete_inner(&format!("{namespace}#{iri}")).await
    }

    async fn delete_inner(&self, iri: &str) -> Result<(), CoreError> {
        self.engine
            .delete(iri)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace delete: {e}"),
            })?;
        self.invalidate_ownership_cache();
        Ok(())
    }

    /// Total number of indexed entries.
    pub async fn count(&self) -> Result<u64, CoreError> {
        self.engine.count().await.map_err(|e| CoreError::Internal {
            message: format!("Hyperspace count: {e}"),
        })
    }

    /// Lists persisted records for startup and ownership-integrity validation.
    pub async fn list(&self, offset: usize, limit: usize) -> Result<Vec<SearchHit>, CoreError> {
        self.engine
            .list(offset, limit)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace list: {e}"),
            })
    }

    /// Resolve an IRI to its numeric point ID (if indexed).
    pub async fn resolve_iri(&self, iri: &str) -> Result<Option<u32>, CoreError> {
        self.engine
            .resolve_iri(iri)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace resolve_iri: {e}"),
            })
    }

    /// Look up the IRI for a numeric point ID (reverse of resolve_iri).
    pub async fn lookup_id(&self, id: u32) -> Result<Option<String>, CoreError> {
        self.engine
            .lookup_id(id)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("Hyperspace lookup_id: {e}"),
            })
    }
}

fn contains_ascii_token(text: &str, token: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let token = token.to_ascii_lowercase();
    if text.match_indices(&token).any(|(index, _)| {
        let before_is_word = text[..index]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_');
        let after_index = index + token.len();
        let after_is_word = text[after_index..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_');
        !before_is_word && !after_is_word
    }) {
        return true;
    }

    let normalized_token = normalize_separator_token(&token);
    token
        .chars()
        .any(|character| matches!(character, '-' | '_' | '/'))
        && text
            .split(|character: char| {
                !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '/')
            })
            .any(|candidate| normalize_separator_token(candidate) == normalized_token)
}

fn normalize_separator_token(value: &str) -> String {
    value
        .chars()
        .filter(|character| !matches!(character, '-' | '_' | '/'))
        .collect()
}

fn entry_matches_claims_and_tags(
    entry: &ScoredEntry,
    claims: &IsolationClaims,
    required_tags: &[String],
) -> bool {
    claims
        .vector_namespace()
        .is_ok_and(|namespace| entry.iri.starts_with(&format!("{namespace}#")))
        && required_tags.iter().all(|tag| entry.tags.contains(tag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use crate::memory::embedding_service::{EmbeddingService, FallbackEmbeddingService};
    use async_trait::async_trait;

    struct UnavailableEmbeddingService;

    #[async_trait]
    impl EmbeddingService for UnavailableEmbeddingService {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
            Err("unavailable".into())
        }

        fn dimension(&self) -> usize {
            8
        }
    }

    struct InvalidEmbeddingService {
        vector: Vec<f32>,
        dimension: usize,
    }

    #[async_trait]
    impl EmbeddingService for InvalidEmbeddingService {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
            Ok(self.vector.clone())
        }

        fn dimension(&self) -> usize {
            self.dimension
        }
    }

    fn setup_store() -> HyperspaceStore {
        let dir = tempfile::tempdir().unwrap();
        let embed = Arc::new(FallbackEmbeddingService::new());
        HyperspaceStore::open(dir.path(), embed).unwrap()
    }

    struct UnavailableEmbedding;

    #[async_trait]
    impl EmbeddingService for UnavailableEmbedding {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
            Err("unavailable".into())
        }

        fn dimension(&self) -> usize {
            128
        }
    }

    #[tokio::test]
    async fn exact_matches_do_not_require_embedding_availability() {
        let directory = tempfile::tempdir().unwrap();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let seeded =
            HyperspaceStore::open(directory.path(), Arc::new(FallbackEmbeddingService::new()))
                .unwrap();
        seeded
            .upsert_with_claims(&claims, "entry", "B2033 placeholder", &["kb:local".into()])
            .await
            .unwrap();
        drop(seeded);
        let store =
            HyperspaceStore::open(directory.path(), Arc::new(UnavailableEmbedding)).unwrap();
        let result = store
            .exact_matches_with_claims_and_required_tags(
                &claims,
                &["kb:local".into()],
                &["b2033".into()],
                3,
            )
            .await
            .unwrap();
        assert_eq!(result.entries.len(), 1);
    }

    #[tokio::test]
    async fn exact_matches_scan_large_scoped_library_without_embeddings() {
        let store = setup_store();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let foreign = IsolationClaims::from_verified("tenant-b", "project-a", "actor-b").unwrap();
        for index in 0..5_001 {
            store
                .upsert_with_claims(
                    &claims,
                    &format!("entry-{index}"),
                    if index == 5_000 {
                        "B2033 marker"
                    } else {
                        "placeholder"
                    },
                    &["kb:local".into()],
                )
                .await
                .unwrap();
        }
        store
            .upsert_with_claims(&foreign, "foreign", "B2033 foreign", &["kb:local".into()])
            .await
            .unwrap();
        let result = store
            .exact_matches_with_claims_and_required_tags(
                &claims,
                &["kb:local".into()],
                &["b2033".into()],
                3,
            )
            .await
            .unwrap();
        assert!(!result.truncated);
        assert_eq!(result.entries.len(), 1);
        assert!(result.entries[0].text.contains("marker"));
    }

    #[tokio::test]
    async fn exact_matches_report_when_scan_cap_is_exceeded() {
        let store = setup_store();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        for index in 0..=EXACT_SCAN_RECORD_LIMIT {
            store
                .upsert_with_claims(
                    &claims,
                    &format!("capped-{index}"),
                    "placeholder",
                    &["kb:local".into()],
                )
                .await
                .unwrap();
        }
        let result = store
            .exact_matches_with_claims_and_required_tags(
                &claims,
                &["kb:local".into()],
                &["missing".into()],
                1,
            )
            .await
            .unwrap();
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn exact_matches_report_truncation_without_exact_hits() {
        let store = setup_store();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        for index in 0..3 {
            store
                .upsert_with_claims(
                    &claims,
                    &format!("entry-{index}"),
                    "placeholder",
                    &["kb:local".into()],
                )
                .await
                .unwrap();
        }
        let result = store
            .exact_matches_with_claims_and_required_tags_capped(
                &claims,
                &["kb:local".into()],
                &["absent".into()],
                1,
                2,
            )
            .await
            .unwrap();
        assert!(result.truncated);
        assert!(result.entries.is_empty());
    }

    #[tokio::test]
    async fn exact_scan_metadata_scope_keeps_own_match_visible_after_foreign_cap() {
        let store = setup_store();
        let own = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let foreign = IsolationClaims::from_verified("tenant-b", "project-a", "actor-b").unwrap();
        for index in 0..EXACT_SCAN_RECORD_LIMIT {
            store
                .upsert_with_claims(
                    &foreign,
                    &format!("foreign-{index}"),
                    "foreign placeholder",
                    &["kb:local".into()],
                )
                .await
                .unwrap();
        }
        store
            .upsert_with_claims(&own, "own", "B2033", &["kb:local".into()])
            .await
            .unwrap();
        let result = store
            .exact_matches_with_claims_and_required_tags(
                &own,
                &["kb:local".into()],
                &["b2033".into()],
                1,
            )
            .await
            .unwrap();
        assert!(!result.truncated);
        assert_eq!(
            result.entries.first().map(|entry| entry.text.as_str()),
            Some("B2033")
        );
    }

    #[tokio::test]
    async fn exact_matches_rank_equality_before_contains_stably() {
        let store = setup_store();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        for (id, text) in [
            ("contains-first", "prefix fault code 2033 suffix"),
            ("exact-target", "fault code 2033"),
            ("contains-second", "more fault code 2033 text"),
        ] {
            store
                .upsert_with_claims(&claims, id, text, &["kb:local".into()])
                .await
                .unwrap();
        }
        let result = store
            .exact_matches_with_claims_and_required_tags(
                &claims,
                &["kb:local".into()],
                &["fault code 2033".into()],
                3,
            )
            .await
            .unwrap();
        assert_eq!(result.entries[0].text, "fault code 2033");
        assert_eq!(result.entries[1].text, "prefix fault code 2033 suffix");
        assert_eq!(result.entries[2].text, "more fault code 2033 text");
    }

    #[tokio::test]
    async fn exact_token_text_ranks_before_containing_text() {
        let store = setup_store();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        store
            .upsert_with_claims(
                &claims,
                "contains",
                "prefix B2033 suffix",
                &["kb:local".into()],
            )
            .await
            .unwrap();
        store
            .upsert_with_claims(&claims, "exact", "B2033", &["kb:local".into()])
            .await
            .unwrap();
        let result = store
            .exact_matches_with_claims_and_required_tags(
                &claims,
                &["kb:local".into()],
                &["b2033".into()],
                2,
            )
            .await
            .unwrap();
        assert_eq!(result.entries[0].text, "B2033");
    }

    #[tokio::test]
    async fn exact_matches_normalize_separator_tokens_in_document_text() {
        let store = setup_store();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        // fictional
        store
            .upsert_with_claims(&claims, "separator", "TKT_0002", &["kb:local".into()])
            .await
            .unwrap();
        // fictional
        store
            .upsert_with_claims(&claims, "unseparated", "TKT0002", &["kb:local".into()])
            .await
            .unwrap();

        let result = store
            .exact_matches_with_claims_and_required_tags(
                &claims,
                &["kb:local".into()],
                &["tkt-0002".into()],
                2,
            )
            .await
            .unwrap();
        assert_eq!(result.entries.len(), 2);
    }

    #[tokio::test]
    async fn test_upsert_and_count() {
        let store = setup_store();
        store.upsert("v:1", "hello world", &[]).await.unwrap();
        store.upsert("v:2", "foo bar baz", &[]).await.unwrap();
        assert_eq!(store.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_search_returns_results() {
        let store = setup_store();
        store
            .upsert("s:1", "rust async programming", &[])
            .await
            .unwrap();
        store
            .upsert("s:2", "python web framework", &[])
            .await
            .unwrap();

        let results = store.search("programming", 10).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_search_empty_store() {
        let store = setup_store();
        let results = store.search("nothing", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn embedding_failure_does_not_search_with_a_placeholder_vector() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            HyperspaceStore::open(dir.path(), Arc::new(UnavailableEmbeddingService)).unwrap();
        let result = store.search("unavailable provider", 10).await;
        assert!(result.is_err());
        assert_eq!(store.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn invalid_query_embeddings_are_rejected() {
        for (vector, dimension) in [(vec![0.0; 3], 3), (vec![f32::NAN; 3], 3), (vec![1.0; 2], 3)] {
            let dir = tempfile::tempdir().unwrap();
            let store = HyperspaceStore::open(
                dir.path(),
                Arc::new(InvalidEmbeddingService { vector, dimension }),
            )
            .unwrap();
            assert!(store.search("query", 1).await.is_err());
        }
    }

    #[tokio::test]
    async fn vector_writes_invalidate_cached_ownership_health() {
        let store = setup_store();
        let key = "vector://tenant/project#kb-a";
        store.cache_ownership_health_if_current(key.into(), store.ownership_generation(key), true);
        store
            .upsert("cache:1", "invalidate ownership cache", &[])
            .await
            .unwrap();
        assert!(store
            .cached_ownership_health("vector://tenant/project#kb-a")
            .is_none());
    }

    #[tokio::test]
    async fn test_delete() {
        let store = setup_store();
        store.upsert("d:1", "delete me", &[]).await.unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
        store.delete("d:1").await.unwrap();
        assert_eq!(store.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_returns_error() {
        let store = setup_store();
        let result = store.delete("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_by_tags() {
        let store = setup_store();
        store
            .upsert("t:1", "rust code", &["lang:rust".into()])
            .await
            .unwrap();
        store
            .upsert("t:2", "python code", &["lang:python".into()])
            .await
            .unwrap();

        let results = store
            .search_by_tags(&["lang:rust".into()], 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "t:1");
    }

    #[tokio::test]
    async fn test_search_by_tags_empty() {
        let store = setup_store();
        let results = store.search_by_tags(&[], 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_with_filter_importance() {
        let store = setup_store();
        store
            .upsert_with_metadata("a:1", "important doc", &[], Some(0.9), None, None)
            .await
            .unwrap();
        store
            .upsert_with_metadata("a:2", "low importance doc", &[], Some(0.1), None, None)
            .await
            .unwrap();

        let filter = HybridSearchFilter::new().with_min_importance(0.5);
        let results = store.search_with_filter("doc", &filter, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "a:1");
    }

    #[tokio::test]
    async fn test_search_with_filter_types() {
        let store = setup_store();
        store
            .upsert_with_metadata("c:1", "code", &[], None, Some(&["Code".into()]), None)
            .await
            .unwrap();
        store
            .upsert_with_metadata("d:1", "document", &[], None, Some(&["Doc".into()]), None)
            .await
            .unwrap();

        let filter = HybridSearchFilter::new().with_jsonld_types(vec!["Code".into()]);
        let results = store.search_with_filter("item", &filter, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "c:1");
    }

    #[tokio::test]
    async fn test_hybrid_search() {
        let store = setup_store();
        store
            .upsert("h:1", "urgent bug fix", &["urgent".into()])
            .await
            .unwrap();
        store
            .upsert("h:2", "routine maintenance", &["normal".into()])
            .await
            .unwrap();

        let results = store
            .hybrid_search("task", &["urgent".into()], &[], None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "h:1");
    }

    #[tokio::test]
    async fn test_upsert_replaces_existing() {
        let store = setup_store();
        store.upsert("u:1", "first version", &[]).await.unwrap();
        store.upsert("u:1", "updated version", &[]).await.unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_scored_entry_fields() {
        let store = setup_store();
        store
            .upsert_with_metadata(
                "e:1",
                "test content",
                &["tag1".into(), "tag2".into()],
                Some(0.7),
                Some(&["TypeA".into()]),
                Some("graph1"),
            )
            .await
            .unwrap();

        // Use an importance filter to find it
        let filter = HybridSearchFilter::new().with_min_importance(0.5);
        let results = store.search_with_filter("test", &filter, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "e:1");
        assert_eq!(results[0].text, "test content");
        assert!(results[0].tags.contains(&"tag1".to_string()));
        assert!(results[0].tags.contains(&"tag2".to_string()));
        assert_eq!(results[0].importance, Some(0.7));
        assert!(results[0].jsonld_types.contains(&"TypeA".to_string()));
    }

    #[tokio::test]
    async fn test_search_filter_is_empty() {
        let store = setup_store();
        store.upsert("f:1", "item", &[]).await.unwrap();
        let filter = HybridSearchFilter::new();
        let results = store.search_with_filter("item", &filter, 10).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_hybrid_search_filter_builder() {
        let filter = HybridSearchFilter::new()
            .with_must_tags(vec!["rust".to_string(), "async".to_string()])
            .with_should_tags(vec!["tokio".to_string()])
            .with_min_importance(0.5)
            .with_jsonld_types(vec!["Code".to_string()]);

        assert_eq!(filter.must_tags.len(), 2);
        assert_eq!(filter.should_tags.len(), 1);
        assert_eq!(filter.min_importance, Some(0.5));
        assert_eq!(filter.jsonld_types.len(), 1);
        assert!(!filter.is_empty());
    }

    #[test]
    fn test_empty_filter() {
        let filter = HybridSearchFilter::new();
        assert!(filter.is_empty());
    }

    #[test]
    fn test_to_jsonld_filters_empty() {
        let dir = tempfile::tempdir().unwrap();
        let embed = Arc::new(FallbackEmbeddingService::new());
        let store = HyperspaceStore::open(dir.path(), embed).unwrap();

        let filters = store.to_jsonld_filters(&HybridSearchFilter::new());
        assert!(filters.is_empty());
    }

    #[test]
    fn test_to_jsonld_filters_must_tags() {
        let dir = tempfile::tempdir().unwrap();
        let embed = Arc::new(FallbackEmbeddingService::new());
        let store = HyperspaceStore::open(dir.path(), embed).unwrap();

        let filter = HybridSearchFilter::new().with_must_tags(vec!["a".into(), "b".into()]);
        let filters = store.to_jsonld_filters(&filter);
        assert_eq!(filters.len(), 1);
        match &filters[0] {
            JsonLdFilter::Must(children) => {
                assert_eq!(children.len(), 2);
            }
            _ => panic!("Expected Must filter"),
        }
    }

    #[test]
    fn test_to_jsonld_filters_all_groups() {
        let dir = tempfile::tempdir().unwrap();
        let embed = Arc::new(FallbackEmbeddingService::new());
        let store = HyperspaceStore::open(dir.path(), embed).unwrap();

        let filter = HybridSearchFilter::new()
            .with_must_tags(vec!["must".into()])
            .with_should_tags(vec!["should".into()])
            .with_must_not_tags(vec!["bad".into()]);
        let filters = store.to_jsonld_filters(&filter);
        // Expect 3 top-level filters: Must, Should, MustNot
        assert_eq!(filters.len(), 3);
    }

    // ──────────────────────────────────────────────────────────────
    // F2b: 向量搜索租户过滤
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_with_tenant_sets_field() {
        let filter = HybridSearchFilter::new().with_tenant("acme");
        assert_eq!(filter.tenant_id.as_deref(), Some("acme"));
        // tenant_id 让 filter 非空
        assert!(!filter.is_empty());
    }

    #[test]
    fn test_to_jsonld_filters_tenant_adds_must_tag() {
        let dir = tempfile::tempdir().unwrap();
        let embed = Arc::new(FallbackEmbeddingService::new());
        let store = HyperspaceStore::open(dir.path(), embed).unwrap();

        let filter = HybridSearchFilter::new().with_tenant("acme");
        let filters = store.to_jsonld_filters(&filter);
        // 应当产生至少一个 Must 条件（tenant tag）
        assert!(!filters.is_empty());
        let has_tenant_must = filters.iter().any(|f| matches!(f, JsonLdFilter::Must(_)));
        assert!(has_tenant_must, "Expected a Must filter for tenant tag");
    }

    #[tokio::test]
    async fn test_upsert_with_tenant_tags_vector() {
        let store = setup_store();
        store
            .upsert_with_tenant("tv:1", "battery repair doc", &[], "acme")
            .await
            .unwrap();
        // 通过 tenant 过滤器能找回
        let filter = HybridSearchFilter::new().with_tenant("acme");
        let results = store
            .search_with_filter("battery", &filter, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "tv:1");
        assert!(
            results[0].tags.contains(&"tenant:acme".to_string()),
            "Expected tenant tag in stored entry"
        );
    }

    #[test]
    fn stale_ownership_scan_cannot_restore_a_cache_entry_after_invalidation() {
        let store = setup_store();
        let key = "vector://tenant/project#kb-a";
        let scan_generation = store.ownership_generation(key);
        store.invalidate_ownership_cache();
        store.cache_ownership_health_if_current(key.into(), scan_generation, true);

        assert!(
            store.cached_ownership_health(key).is_none(),
            "a scan that started before a write must not restore a stale healthy cache entry"
        );
    }

    #[tokio::test]
    async fn test_tenant_filter_isolates_entries() {
        let store = setup_store();
        store
            .upsert_with_tenant("ti:1", "acme battery data", &[], "acme")
            .await
            .unwrap();
        store
            .upsert_with_tenant("ti:2", "other battery data", &[], "other_corp")
            .await
            .unwrap();

        // acme 只能看到自己的条目
        let filter_acme = HybridSearchFilter::new().with_tenant("acme");
        let res_acme = store
            .search_with_filter("battery", &filter_acme, 10)
            .await
            .unwrap();
        assert_eq!(res_acme.len(), 1);
        assert_eq!(res_acme[0].iri, "ti:1");

        // other_corp 只能看到自己的条目
        let filter_other = HybridSearchFilter::new().with_tenant("other_corp");
        let res_other = store
            .search_with_filter("battery", &filter_other, 10)
            .await
            .unwrap();
        assert_eq!(res_other.len(), 1);
        assert_eq!(res_other[0].iri, "ti:2");
    }

    #[tokio::test]
    async fn claims_namespace_prevents_cross_tenant_recall() {
        let store = setup_store();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-1", "agent-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-1", "agent-b").unwrap();

        store
            .upsert_with_claims(
                &tenant_a,
                "battery-guide",
                "replace the battery in the tenant A vehicle",
                &[],
            )
            .await
            .unwrap();

        let tenant_a_hits = store
            .search_with_claims(
                &tenant_a,
                "replace the battery",
                &HybridSearchFilter::new(),
                10,
            )
            .await
            .unwrap();
        assert_eq!(tenant_a_hits.len(), 1);
        assert_eq!(
            tenant_a_hits[0].iri,
            "vector://tenant-a/project-1#battery-guide"
        );

        let tenant_b_hits = store
            .search_with_claims(
                &tenant_b,
                "replace the battery",
                &HybridSearchFilter::new(),
                10,
            )
            .await
            .unwrap();
        assert!(
            tenant_b_hits.is_empty(),
            "tenant B must not recall tenant A's namespaced vector"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_claims_writes_remain_isolated_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let embed = Arc::new(FallbackEmbeddingService::new());
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-b", "actor-b").unwrap();
        let store = Arc::new(HyperspaceStore::open(dir.path(), embed.clone()).unwrap());
        let mut tasks = tokio::task::JoinSet::new();

        for index in 0..4 {
            for (claims, kb_tag, prefix) in [
                (tenant_a.clone(), "kb:kb-a", "a"),
                (tenant_b.clone(), "kb:kb-b", "b"),
            ] {
                let store = Arc::clone(&store);
                let text = format!("isolated {prefix} content {index}");
                let iri = format!("{prefix}:{index}");
                let tags = vec![kb_tag.to_string(), format!("chunk:{prefix}:{index}")];
                tasks.spawn(
                    async move { store.upsert_with_claims(&claims, &iri, &text, &tags).await },
                );
            }
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        store.engine.checkpoint().await.unwrap();

        for index in 4..8 {
            for (claims, kb_tag, prefix) in [
                (tenant_a.clone(), "kb:kb-a", "a"),
                (tenant_b.clone(), "kb:kb-b", "b"),
            ] {
                let store = Arc::clone(&store);
                let text = format!("isolated {prefix} content {index}");
                let iri = format!("{prefix}:{index}");
                let tags = vec![kb_tag.to_string(), format!("chunk:{prefix}:{index}")];
                tasks.spawn(
                    async move { store.upsert_with_claims(&claims, &iri, &text, &tags).await },
                );
            }
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        drop(store);

        let store = HyperspaceStore::open(dir.path(), embed).unwrap();
        for (claims, kb_tag, prefix) in [(&tenant_a, "kb:kb-a", "a"), (&tenant_b, "kb:kb-b", "b")] {
            for index in 0..8 {
                let text = format!("isolated {prefix} content {index}");
                let namespace = claims.vector_namespace().unwrap();
                let payload = store
                    .engine
                    .get_payload(&format!("{namespace}#{prefix}:{index}"))
                    .await
                    .unwrap();
                let payload = payload.expect("exact record must survive restart");
                assert_eq!(payload["text"], text);
                assert!(payload["tags"]
                    .as_array()
                    .unwrap()
                    .contains(&Value::String(format!("tenant:{}", claims.tenant_id()))));
                assert!(payload["tags"]
                    .as_array()
                    .unwrap()
                    .contains(&Value::String(kb_tag.to_string())));
            }
        }

        let cross_tenant = store
            .search_with_claims_and_required_tags(
                &tenant_a,
                "isolated b content 0",
                &["kb:kb-b".to_string(), "chunk:b:0".to_string()],
                1,
            )
            .await
            .unwrap();
        assert!(cross_tenant.is_empty());
    }
}
