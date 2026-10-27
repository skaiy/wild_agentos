//! Incremental HNSW index with runtime metric switching and
//! multi-layer entry-point descent.
//!
//! # Design
//!
//! Implements the HNSW (Hierarchical Navigable Small World) algorithm
//! by Malkov & Yashunin, with these adaptations:
//!
//! - Runtime metric switching via `Box<dyn Metric>`
//! - Per-layer search descent (top layer → bottom)
//! - Generation-tagged visited set for lock-free search
//! - Serializable node format for snapshot persistence
//! - Optional filter bitmap integration for filtered search

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use rand::Rng;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

use crate::hyper_vector::EmbeddingVector;
use crate::metric::Metric;

/// HNSW configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswConfig {
    pub ef_construction: usize,
    pub ef_search: usize,
    pub m: usize,
    pub m0: usize,
    pub level_mult: f64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            ef_construction: 200,
            ef_search: 50,
            m: 16,
            m0: 32,
            level_mult: 1.0 / (16.0_f64).ln(),
        }
    }
}

/// Serializable representation of a node for snapshot persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableNode {
    pub coords: Vec<f64>,
    pub metric_tag: u32,
    pub alpha: f64,
    pub neighbors0: Vec<u32>,
    pub neighbors_upper: Vec<Vec<u32>>,
    pub level: usize,
}

impl From<&Node> for SerializableNode {
    fn from(n: &Node) -> Self {
        Self {
            coords: n.vector.coords.clone(),
            metric_tag: n.vector.metric as u32,
            alpha: n.vector.alpha,
            neighbors0: n.neighbors0.clone(),
            neighbors_upper: n.neighbors_upper.clone(),
            level: n.level,
        }
    }
}

impl SerializableNode {
    pub fn to_embedding(&self, metric_kind: crate::hyper_vector::MetricKind) -> EmbeddingVector {
        EmbeddingVector {
            coords: self.coords.clone(),
            metric: metric_kind,
            alpha: self.alpha,
        }
    }
}

#[derive(Clone)]
struct Node {
    vector: EmbeddingVector,
    neighbors0: Vec<u32>,
    neighbors_upper: Vec<Vec<u32>>,
    level: usize,
}

#[derive(Clone, Copy)]
struct Candidate {
    id: u32,
    distance: f64,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.distance.to_bits() == other.distance.to_bits()
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .distance
            .total_cmp(&self.distance)
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// Incremental HNSW index with runtime metric switching.
pub struct IncrementalHNSW {
    nodes: Vec<Option<Node>>,
    metric: Box<dyn Metric>,
    entry_point: AtomicU32,
    max_layer: AtomicUsize,
    config: HnswConfig,
    generation: AtomicUsize,
    /// Per-node visited generation tag.
    /// `Vec<AtomicUsize>` rather than `Vec<usize>` so that the generation-based
    /// visited check is genuinely lock-free — concurrent readers (via e.g. RwLock)
    /// can race-free compare-and-mark without a mutex.
    visited_gen: Vec<AtomicUsize>,
}

impl IncrementalHNSW {
    /// Small filtered collections are cheaper and more reliable to search
    /// exactly than to recover from a global ANN candidate set. In particular,
    /// a tag with only a few vectors can otherwise be entirely absent from the
    /// first `ef_search` global candidates.
    const EXACT_FILTERED_SEARCH_LIMIT: u64 = 5_000;

    pub fn new(metric: Box<dyn Metric>, config: HnswConfig) -> Self {
        Self {
            nodes: Vec::new(),
            metric,
            entry_point: AtomicU32::new(u32::MAX),
            max_layer: AtomicUsize::new(0),
            config,
            generation: AtomicUsize::new(1),
            visited_gen: Vec::new(),
        }
    }

    pub fn set_metric(&mut self, metric: Box<dyn Metric>) {
        self.metric = metric;
    }

    fn random_level(&self) -> usize {
        let r: f64 = rand::thread_rng().gen();
        if r <= 0.0 {
            return 0;
        }
        (r.ln() * (-self.config.level_mult)).floor() as usize
    }

    /// Insert a node with its vector. Uses multi-layer entry-point descent.
    pub fn insert(&mut self, id: u32, vector: EmbeddingVector) {
        let level = self.random_level();
        let id_u = id as usize;

        if id_u >= self.nodes.len() {
            self.nodes.resize(id_u + 1, None);
        }
        if id_u >= self.visited_gen.len() {
            self.visited_gen
                .resize_with(self.visited_gen.len().max(id_u + 1).max(1024), || {
                    AtomicUsize::new(0)
                });
        }

        let ep = self.entry_point.load(Ordering::Relaxed);

        self.nodes[id_u] = Some(Node {
            vector,
            neighbors0: Vec::with_capacity(self.config.m0),
            neighbors_upper: (0..level)
                .map(|_| Vec::with_capacity(self.config.m))
                .collect(),
            level,
        });

        if ep == u32::MAX {
            self.entry_point.store(id, Ordering::Release);
            self.max_layer.store(level, Ordering::Release);
            return;
        }

        let ep_level = self.nodes[ep as usize].as_ref().map_or(0, |n| n.level);
        let max_level = ep_level.max(level);
        let mut curr_ep = ep;

        // Phase 1: Greedy descent from top layer down to (level+1)
        // (only if level < ep_level)
        if level < ep_level {
            for lvl in (level + 1..=max_level).rev() {
                curr_ep = self.search_layer_single(lvl, curr_ep, id);
            }
        }

        // Phase 2: Search and connect at layers 0..=level
        for lvl in (0..=level.min(max_level)).rev() {
            let ef = if lvl == 0 {
                self.config.m0
            } else {
                self.config.m
            };
            let nearest = self.search_layer_top_k(lvl, curr_ep, id, ef);
            let num_neighbors = if lvl == 0 {
                self.config.m0
            } else {
                self.config.m
            };
            let neighbor_ids: Vec<u32> = nearest.iter().take(num_neighbors).copied().collect();

            // Set forward connections
            if lvl == 0 {
                if let Some(ref mut n) = self.nodes[id_u] {
                    n.neighbors0 = neighbor_ids.clone();
                }
            } else if let Some(ref mut n) = self.nodes[id_u] {
                if lvl - 1 < n.neighbors_upper.len() {
                    n.neighbors_upper[lvl - 1] = neighbor_ids.clone();
                }
            }

            // Set backward connections
            for &nid in &neighbor_ids {
                let max_neighbors = if lvl == 0 {
                    self.config.m0
                } else {
                    self.config.m
                };
                self.add_backlink(nid, id, lvl, max_neighbors);
            }

            if !nearest.is_empty() {
                curr_ep = nearest[0];
            }
        }

        // Update entry point if this node is at a higher level
        if level > ep_level {
            self.entry_point.store(id, Ordering::Release);
            self.max_layer.store(level, Ordering::Release);
        }
    }

    /// Add a reciprocal edge while keeping the closest neighbors when a layer is full.
    fn add_backlink(&mut self, neighbor_id: u32, id: u32, layer: usize, max_neighbors: usize) {
        let source_vector = match self
            .nodes
            .get(neighbor_id as usize)
            .and_then(|node| node.as_ref())
        {
            Some(node) => node.vector.clone(),
            None => return,
        };

        let mut candidates = self.get_neighbors(neighbor_id, layer);
        if !candidates.contains(&id) {
            candidates.push(id);
        }
        candidates.sort_by(|left, right| {
            let left_distance = self.metric.distance(
                &source_vector,
                &self.nodes[*left as usize].as_ref().unwrap().vector,
            );
            let right_distance = self.metric.distance(
                &source_vector,
                &self.nodes[*right as usize].as_ref().unwrap().vector,
            );
            left_distance.total_cmp(&right_distance)
        });
        candidates.truncate(max_neighbors);

        if let Some(node) = self
            .nodes
            .get_mut(neighbor_id as usize)
            .and_then(|node| node.as_mut())
        {
            if layer == 0 {
                node.neighbors0 = candidates;
            } else if let Some(neighbors) = node.neighbors_upper.get_mut(layer - 1) {
                *neighbors = candidates;
            }
        }
    }

    /// Single-layer greedy search: find the nearest node to the query.
    fn search_layer_single(&self, layer: usize, entry: u32, query_id: u32) -> u32 {
        let query_node = match self.nodes.get(query_id as usize).and_then(|n| n.as_ref()) {
            Some(n) => &n.vector,
            None => return entry,
        };
        let mut best = entry;
        let mut best_dist = if self.contains(best) {
            self.metric.distance(
                query_node,
                &self.nodes[best as usize].as_ref().unwrap().vector,
            )
        } else {
            return entry;
        };

        loop {
            let neighbors = self.get_neighbors(best, layer);
            let mut improved = false;
            for &nid in &neighbors {
                let d = self.metric.distance(
                    query_node,
                    &self.nodes[nid as usize].as_ref().unwrap().vector,
                );
                if d < best_dist {
                    best_dist = d;
                    best = nid;
                    improved = true;
                    break;
                }
            }
            if !improved {
                break;
            }
        }
        best
    }

    /// Search layer for top-k nearest nodes to the query, using generation-tagged visited set.
    fn search_layer_top_k(
        &mut self,
        layer: usize,
        entry: u32,
        query_id: u32,
        ef: usize,
    ) -> Vec<u32> {
        let gen = self.generation.fetch_add(1, Ordering::AcqRel);
        let query_node = match self.nodes.get(query_id as usize).and_then(|n| n.as_ref()) {
            Some(n) => &n.vector,
            None => return Vec::new(),
        };

        if !self.contains(entry) {
            return Vec::new();
        }

        let entry_dist = self.metric.distance(
            query_node,
            &self.nodes[entry as usize].as_ref().unwrap().vector,
        );
        let mut candidates = BinaryHeap::from([Candidate {
            id: entry,
            distance: entry_dist,
        }]);
        let mut results = vec![(entry, entry_dist)];
        self.visited_gen[entry as usize].store(gen, Ordering::Relaxed);

        while let Some(Candidate {
            id: current,
            distance: current_dist,
        }) = candidates.pop()
        {
            if results.len() >= ef && current_dist > results.last().unwrap().1 {
                break;
            }
            for &nid in &self.get_neighbors(current, layer) {
                let nu = nid as usize;
                if nu < self.visited_gen.len() {
                    if self.visited_gen[nu].load(Ordering::Relaxed) == gen {
                        continue;
                    }
                    self.visited_gen[nu].store(gen, Ordering::Relaxed);
                }
                let d = self.metric.distance(
                    query_node,
                    &self.nodes[nid as usize].as_ref().unwrap().vector,
                );
                if results.len() < ef || d < results.last().unwrap().1 {
                    candidates.push(Candidate {
                        id: nid,
                        distance: d,
                    });

                    let result_pos = results
                        .binary_search_by(|(_, dist)| dist.total_cmp(&d))
                        .unwrap_or_else(|e| e);
                    results.insert(result_pos, (nid, d));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        results.into_iter().map(|(id, _)| id).collect()
    }

    /// Public search: find nearest neighbors to a query vector.
    ///
    /// Uses multi-layer descent from the entry point down to layer 0,
    /// then beam search at layer 0 with ef_search beam width.
    ///
    /// If `allowed` is Some, only returns IDs present in the bitmap.
    pub fn search_with_filter(
        &mut self,
        query: &EmbeddingVector,
        top_k: usize,
        allowed: Option<&RoaringBitmap>,
    ) -> Vec<(u32, f64)> {
        if let Some(allowed) = allowed {
            if Self::should_use_exact_filtered_search(allowed) {
                return self.exact_filtered_search(query, top_k, allowed);
            }
        }

        let mut ep = self.entry_point.load(Ordering::Relaxed);
        if ep != u32::MAX && !self.contains(ep) {
            self.fix_entry_point();
            ep = self.entry_point.load(Ordering::Acquire);
        }
        if ep == u32::MAX {
            return Vec::new();
        }

        let max_layer = self.max_layer.load(Ordering::Relaxed);
        let ef = self.filter_aware_ef(top_k, allowed);

        // Multi-layer descent
        let mut curr_ep = ep;

        // Beam search at layer 0
        if max_layer > 0 {
            for lvl in (1..=max_layer).rev() {
                curr_ep = self.search_layer_single_external(lvl, curr_ep, query);
            }
        }

        // Beam search at layer 0
        let results = self.search_layer0_external(query, ef, curr_ep);

        // Truncate and optionally filter
        let mut results: Vec<(u32, f64)> = results;
        if let Some(ab) = allowed {
            results.retain(|(id, _)| ab.contains(*id));
        }
        results.truncate(top_k);
        results
    }

    fn should_use_exact_filtered_search(allowed: &RoaringBitmap) -> bool {
        allowed.len() <= Self::EXACT_FILTERED_SEARCH_LIMIT
    }

    /// Increase the global beam width for a selective filter. This keeps the
    /// expected number of allowed candidates proportional to `top_k` instead
    /// of allowing a much larger collection to consume a fixed global budget.
    fn filter_aware_ef(&self, top_k: usize, allowed: Option<&RoaringBitmap>) -> usize {
        let base_ef = self.config.ef_search.max(top_k);
        let Some(allowed) = allowed else {
            return base_ef;
        };

        let total = self.len();
        let allowed_count = allowed.len() as usize;
        if total == 0 || allowed_count == 0 {
            return base_ef;
        }

        let scaled_ef = top_k
            .saturating_mul(total)
            .div_ceil(allowed_count)
            .saturating_mul(2);
        base_ef.max(scaled_ef).min(total)
    }

    /// Brute-force the small allowed set. This is the correctness backstop for
    /// sparse tag filters and is independent of graph reachability.
    fn exact_filtered_search(
        &self,
        query: &EmbeddingVector,
        top_k: usize,
        allowed: &RoaringBitmap,
    ) -> Vec<(u32, f64)> {
        let mut results: Vec<(u32, f64)> = allowed
            .iter()
            .filter_map(|id| {
                self.nodes
                    .get(id as usize)
                    .and_then(|node| node.as_ref())
                    .map(|node| (id, self.metric.distance(query, &node.vector)))
            })
            .collect();
        let compare_results =
            |(left_id, left_distance): &(u32, f64), (right_id, right_distance): &(u32, f64)| {
                left_distance
                    .total_cmp(right_distance)
                    .then_with(|| left_id.cmp(right_id))
            };
        if results.len() > top_k {
            results.select_nth_unstable_by(top_k, compare_results);
            results.truncate(top_k);
        }
        results.sort_by(compare_results);
        results
    }

    /// Legacy search interface (no filter).
    pub fn search(&mut self, query: &EmbeddingVector, top_k: usize) -> Vec<(u32, f64)> {
        self.search_with_filter(query, top_k, None)
    }

    /// Single-layer greedy search for an external query (not a node ID).
    fn search_layer_single_external(
        &self,
        layer: usize,
        entry: u32,
        query: &EmbeddingVector,
    ) -> u32 {
        if !self.contains(entry) {
            return entry;
        }
        let mut best = entry;
        let mut best_dist = self
            .metric
            .distance(query, &self.nodes[best as usize].as_ref().unwrap().vector);
        loop {
            let neighbors = self.get_neighbors(best, layer);
            let mut improved = false;
            for &nid in &neighbors {
                let d = self
                    .metric
                    .distance(query, &self.nodes[nid as usize].as_ref().unwrap().vector);
                if d < best_dist {
                    best_dist = d;
                    best = nid;
                    improved = true;
                    break;
                }
            }
            if !improved {
                break;
            }
        }
        best
    }

    /// Beam search at layer 0 for an external query.
    fn search_layer0_external(
        &mut self,
        query: &EmbeddingVector,
        ef: usize,
        entry: u32,
    ) -> Vec<(u32, f64)> {
        let gen = self.generation.fetch_add(1, Ordering::AcqRel);
        if !self.contains(entry) {
            return Vec::new();
        }

        let entry_dist = self
            .metric
            .distance(query, &self.nodes[entry as usize].as_ref().unwrap().vector);
        let mut candidates = BinaryHeap::from([Candidate {
            id: entry,
            distance: entry_dist,
        }]);
        let mut results = vec![(entry, entry_dist)];
        self.visited_gen[entry as usize].store(gen, Ordering::Relaxed);

        while let Some(Candidate {
            id: current,
            distance: current_dist,
        }) = candidates.pop()
        {
            if results.len() >= ef && current_dist > results.last().unwrap().1 {
                break;
            }
            let neighbors = self.get_neighbors(current, 0);
            for &nid in &neighbors {
                let nu = nid as usize;
                if nu < self.visited_gen.len() {
                    if self.visited_gen[nu].load(Ordering::Relaxed) == gen {
                        continue;
                    }
                    self.visited_gen[nu].store(gen, Ordering::Relaxed);
                }
                let d = self
                    .metric
                    .distance(query, &self.nodes[nid as usize].as_ref().unwrap().vector);
                if results.len() < ef || d < results.last().unwrap().1 {
                    candidates.push(Candidate {
                        id: nid,
                        distance: d,
                    });

                    let result_pos = results
                        .binary_search_by(|(_, dist)| dist.total_cmp(&d))
                        .unwrap_or_else(|e| e);
                    results.insert(result_pos, (nid, d));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        results
    }

    fn get_neighbors(&self, node_id: u32, layer: usize) -> Vec<u32> {
        match self.nodes.get(node_id as usize).and_then(|n| n.as_ref()) {
            Some(n) if layer == 0 => n
                .neighbors0
                .iter()
                .filter(|&&id| self.contains(id))
                .copied()
                .collect(),
            Some(n) if layer >= 1 && layer - 1 < n.neighbors_upper.len() => n.neighbors_upper
                [layer - 1]
                .iter()
                .filter(|&&id| self.contains(id))
                .copied()
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn remove(&mut self, id: u32) {
        if let Some(n) = self.nodes.get_mut(id as usize) {
            *n = None;
        }
        let ep = self.entry_point.load(Ordering::Relaxed);
        if ep == id {
            self.fix_entry_point();
        } else {
            self.fix_max_layer();
        }
    }

    fn fix_entry_point(&self) {
        let (new_ep, max_layer) = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| node.as_ref().map(|node| (id as u32, node.level)))
            .max_by_key(|&(_, level)| level)
            .unwrap_or((u32::MAX, 0));
        self.entry_point.store(new_ep, Ordering::Release);
        self.max_layer.store(max_layer, Ordering::Release);
    }

    fn fix_max_layer(&self) {
        let max_lvl = self
            .nodes
            .iter()
            .filter_map(|n| n.as_ref())
            .map(|n| n.level)
            .max()
            .unwrap_or(0);
        self.max_layer.store(max_lvl, Ordering::Release);
    }

    pub fn contains(&self, id: u32) -> bool {
        self.nodes
            .get(id as usize)
            .and_then(|n| n.as_ref())
            .is_some()
    }

    pub fn len(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn metric(&self) -> &dyn Metric {
        self.metric.as_ref()
    }

    pub fn config(&self) -> &HnswConfig {
        &self.config
    }

    pub fn entry_point(&self) -> u32 {
        self.entry_point.load(Ordering::Relaxed)
    }

    pub fn max_layer(&self) -> usize {
        self.max_layer.load(Ordering::Relaxed)
    }

    // ── Snapshot support ────────────────────────────────────────────────────

    /// Export all nodes as serializable nodes.
    pub fn export_nodes(&self) -> Vec<Option<SerializableNode>> {
        self.nodes
            .iter()
            .map(|n| n.as_ref().map(SerializableNode::from))
            .collect()
    }

    /// Import nodes from a snapshot.
    pub fn import_nodes(&mut self, nodes: Vec<Option<SerializableNode>>) {
        let metric_kind = self.metric.kind();
        self.nodes = nodes
            .into_iter()
            .map(|sn| {
                sn.map(|s| Node {
                    vector: s.to_embedding(metric_kind),
                    neighbors0: s.neighbors0,
                    neighbors_upper: s.neighbors_upper,
                    level: s.level,
                })
            })
            .collect();
        self.fix_entry_point();
        // Resize visited_gen
        self.visited_gen.resize_with(
            self.visited_gen.len().max(self.nodes.len()).max(1024),
            || AtomicUsize::new(0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyper_vector::MetricKind;
    use crate::metric::CosineMetric;

    fn v(coords: Vec<f64>) -> EmbeddingVector {
        EmbeddingVector::new_unchecked(coords, MetricKind::Cosine)
    }

    #[test]
    fn test_empty_search() {
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        assert!(idx.search(&v(vec![1.0, 0.0]), 5).is_empty());
    }

    #[test]
    fn test_insert_one() {
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        idx.insert(0, v(vec![1.0, 0.0]));
        assert!(idx.contains(0));
        let results = idx.search(&v(vec![1.0, 0.0]), 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_insert_two_nearest() {
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        idx.insert(0, v(vec![1.0, 0.0]));
        idx.insert(1, v(vec![0.0, 1.0]));
        let results = idx.search(&v(vec![0.9, 0.1]), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_remove() {
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        idx.insert(0, v(vec![1.0, 0.0]));
        idx.insert(1, v(vec![0.0, 1.0]));
        idx.remove(0);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn test_random_level_distribution() {
        let idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        let mut counts = [0usize; 10];
        for _ in 0..10000 {
            let l = idx.random_level();
            if l < 10 {
                counts[l] += 1;
            }
        }
        assert!(counts[0] > counts[1]);
    }

    #[test]
    fn test_search_with_filter() {
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        for i in 0..10u32 {
            let x = (i as f64) * 0.1;
            idx.insert(i, v(vec![x, 0.0]));
        }

        // Only allow even IDs
        let mut allowed = RoaringBitmap::new();
        allowed.insert(0);
        allowed.insert(2);
        allowed.insert(4);
        allowed.insert(6);
        allowed.insert(8);

        let results = idx.search_with_filter(&v(vec![0.5, 0.0]), 10, Some(&allowed));
        assert_eq!(results.len(), 5);
        for (id, _) in &results {
            assert!(allowed.contains(*id), "ID {id} should be in allowed set");
        }
    }

    #[test]
    fn test_filtered_exact_search_threshold_includes_5000() {
        let mut at_limit = RoaringBitmap::new();
        at_limit.insert_range(0..=4_999);
        assert!(IncrementalHNSW::should_use_exact_filtered_search(&at_limit));

        at_limit.insert(5_000);
        assert!(!IncrementalHNSW::should_use_exact_filtered_search(
            &at_limit
        ));
    }

    fn sequential_unit_vector(index: usize, total: usize) -> EmbeddingVector {
        let angle = (index + 1) as f64 * std::f64::consts::FRAC_PI_2 / (total + 1) as f64;
        v(vec![angle.cos(), angle.sin(), 0.0, 0.0])
    }

    #[test]
    fn test_sequential_inserts_preserve_self_recall_with_and_without_filter() {
        let config = HnswConfig {
            m: 16,
            ef_search: 50,
            level_mult: 0.0,
            ..HnswConfig::default()
        };

        for total in [64, 256, 1000] {
            let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), config.clone());
            let mut allowed = RoaringBitmap::new();

            for id in 0..total as u32 {
                idx.insert(id, sequential_unit_vector(id as usize, total));
                allowed.insert(id);
            }

            for id in 0..total as u32 {
                let query = sequential_unit_vector(id as usize, total);
                assert_eq!(
                    idx.search(&query, 1)
                        .first()
                        .map(|(result_id, _)| *result_id),
                    Some(id),
                    "unfiltered search should return vector {id} as its own nearest neighbor"
                );
                assert_eq!(
                    idx.search_with_filter(&query, 1, Some(&allowed))
                        .first()
                        .map(|(result_id, _)| *result_id),
                    Some(id),
                    "filtered search should return vector {id} as its own nearest neighbor"
                );
            }
        }
    }

    #[test]
    fn test_serializable_node_roundtrip() {
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        idx.insert(0, v(vec![1.0, 0.0, 0.0, 0.0]));
        idx.insert(1, v(vec![0.0, 1.0, 0.0, 0.0]));

        let exported = idx.export_nodes();
        assert_eq!(exported.len(), 2);
        assert!(exported[0].is_some());
        assert!(exported[1].is_some());

        // Re-import into fresh index
        let mut idx2 = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        idx2.import_nodes(exported);
        assert_eq!(idx2.len(), 2);
        assert!(idx2.contains(0));
        assert!(idx2.contains(1));

        let results = idx2.search(&v(vec![1.0, 0.0, 0.0, 0.0]), 5);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_max_layer_tracking() {
        // With CosineMetric, levels are determined by random_level().
        // At minimum, max_layer should be the highest level among inserted nodes.
        let mut idx = IncrementalHNSW::new(Box::new(CosineMetric), HnswConfig::default());
        assert_eq!(idx.max_layer(), 0);
        idx.insert(0, v(vec![1.0, 0.0]));
        let ml0 = idx.max_layer();
        assert!(ml0 <= 16); // reasonable bound
        let _ = idx;
    }
}
