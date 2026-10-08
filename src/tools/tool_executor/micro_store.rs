//! Owner-scoped storage for generated result readers (#311).
//!
//! The ResultRouter keeps large tool results and exposes them to the model
//! through generated readers (`read_full_result_{call_id}`, `query_{type}`,
//! `get_entity_details`, `expand_relation`). The executor is shared by every
//! run, agent and tenant, so both the stored result and the reader context are
//! keyed by a [`MicroToolOwner`]: verified tenant/project plus the run id and
//! agent id from the runtime. A reader is only visible to the run and agent
//! that produced it; any other caller sees exactly what it would see for a
//! reader that never existed.
//!
//! Entries are removed when the run ends, expire after a TTL, and are capped
//! in total (oldest first).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::MicroToolContext;
use crate::isolation::IsolationClaims;

/// Default lifetime of a stored result and its readers.
pub(crate) const DEFAULT_MICRO_TOOL_TTL: Duration = Duration::from_secs(60 * 60);
/// Override for [`DEFAULT_MICRO_TOOL_TTL`], in seconds.
pub(crate) const MICRO_TOOL_TTL_ENV: &str = "AGENTOS_MICRO_TOOL_TTL_SECS";
/// Upper bound for readers and for stored results, each, across all owners.
pub(crate) const MAX_MICRO_TOOL_ENTRIES: usize = 1024;

/// Who a generated reader and its stored result belong to.
///
/// Built only by the runtime: tenant/project come from verified claims (empty
/// when the run has none), run id from the run guard, agent id from the
/// running agent. Never derived from model output.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MicroToolOwner {
    pub tenant_id: String,
    pub project_id: String,
    pub run_id: String,
    pub agent_id: String,
}

impl MicroToolOwner {
    pub fn new(claims: Option<&IsolationClaims>, run_id: &str, agent_id: &str) -> Self {
        let (tenant_id, project_id) = claims
            .map(|claims| {
                (
                    claims.tenant_id().to_string(),
                    claims.project_id().to_string(),
                )
            })
            .unwrap_or_default();
        Self {
            tenant_id,
            project_id,
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    /// Storage key for one result:
    /// `iri://tool-result/{tenant}/{project}/{run}/{agent}/{call_id}`.
    /// Every segment is escaped, so no value can spell another owner's key.
    pub fn storage_key(&self, call_id: &str) -> String {
        format!(
            "iri://tool-result/{}/{}/{}/{}/{}",
            escape_segment(&self.tenant_id),
            escape_segment(&self.project_id),
            escape_segment(&self.run_id),
            escape_segment(&self.agent_id),
            escape_segment(call_id),
        )
    }
}

fn escape_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn ttl() -> Duration {
    std::env::var(MICRO_TOOL_TTL_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_MICRO_TOOL_TTL)
}

struct ReaderEntry {
    context: MicroToolContext,
    seq: u64,
    created: Instant,
}

struct DataEntry {
    owner: MicroToolOwner,
    value: Value,
    seq: u64,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct MicroToolStore {
    readers: HashMap<(MicroToolOwner, String), ReaderEntry>,
    data: HashMap<String, DataEntry>,
    next_seq: u64,
    /// Test-only clock advance; always zero outside tests.
    clock_skew: Duration,
}

impl MicroToolStore {
    fn now(&self) -> Instant {
        Instant::now() + self.clock_skew
    }

    fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn prune(&mut self, now: Instant) {
        let ttl = ttl();
        self.readers
            .retain(|_, entry| now.duration_since(entry.created) < ttl);
        self.data
            .retain(|_, entry| now.duration_since(entry.created) < ttl);
        while self.readers.len() > MAX_MICRO_TOOL_ENTRIES {
            let Some(oldest) = self
                .readers
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.readers.remove(&oldest);
        }
        while self.data.len() > MAX_MICRO_TOOL_ENTRIES {
            let Some(oldest) = self
                .data
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.data.remove(&oldest);
        }
    }

    pub(crate) fn store_data(&mut self, owner: &MicroToolOwner, storage_key: &str, value: Value) {
        let now = self.now();
        let seq = self.next_seq();
        self.data.insert(
            storage_key.to_string(),
            DataEntry {
                owner: owner.clone(),
                value,
                seq,
                created: now,
            },
        );
        self.prune(now);
    }

    pub(crate) fn register_reader(&mut self, tool_name: &str, context: MicroToolContext) {
        let now = self.now();
        let seq = self.next_seq();
        self.readers.insert(
            (context.owner.clone(), tool_name.to_string()),
            ReaderEntry {
                context,
                seq,
                created: now,
            },
        );
        self.prune(now);
    }

    /// The reader `tool_name` of `owner` with its stored result, if both
    /// exist, have not expired and belong to `owner`.
    pub(crate) fn lookup(
        &self,
        owner: &MicroToolOwner,
        tool_name: &str,
    ) -> Option<(MicroToolContext, Value)> {
        let ttl = ttl();
        let now = self.now();
        let reader = self
            .readers
            .get(&(owner.clone(), tool_name.to_string()))
            .filter(|entry| now.duration_since(entry.created) < ttl)?;
        let data = self
            .data
            .get(&reader.context.storage_key)
            .filter(|entry| entry.owner == *owner && now.duration_since(entry.created) < ttl)?;
        Some((reader.context.clone(), data.value.clone()))
    }

    pub(crate) fn has_reader(&self, owner: &MicroToolOwner, tool_name: &str) -> bool {
        self.lookup(owner, tool_name).is_some()
    }

    /// The owner's live readers, oldest first.
    pub(crate) fn readers_for(&self, owner: &MicroToolOwner) -> Vec<(String, MicroToolContext)> {
        let ttl = ttl();
        let now = self.now();
        let mut readers: Vec<(u64, String, MicroToolContext)> = self
            .readers
            .iter()
            .filter(|((reader_owner, _), entry)| {
                reader_owner == owner && now.duration_since(entry.created) < ttl
            })
            .map(|((_, name), entry)| (entry.seq, name.clone(), entry.context.clone()))
            .collect();
        readers.sort_by_key(|(seq, _, _)| *seq);
        readers
            .into_iter()
            .map(|(_, name, context)| (name, context))
            .collect()
    }

    /// Forget every reader of this name (an external tool now owns it).
    pub(crate) fn remove_readers_named(&mut self, tool_name: &str) {
        self.readers.retain(|(_, name), _| name != tool_name);
    }

    pub(crate) fn remove_run(&mut self, run_id: &str) {
        self.readers.retain(|(owner, _), _| owner.run_id != run_id);
        self.data.retain(|_, entry| entry.owner.run_id != run_id);
    }

    pub(crate) fn counts(&self) -> (usize, usize) {
        (self.readers.len(), self.data.len())
    }

    /// Move this store's clock forward, so existing entries age by `by`.
    #[cfg(test)]
    pub(crate) fn advance_clock_for_test(&mut self, by: Duration) {
        self.clock_skew += by;
    }
}

/// Shared handle to the executor's micro-tool store, used by the run guard
/// to drop a finished run's readers and results.
#[derive(Clone, Default)]
pub struct MicroToolStoreHandle(pub(crate) Arc<parking_lot::RwLock<MicroToolStore>>);

impl MicroToolStoreHandle {
    /// Remove every reader and stored result of `run_id`.
    pub fn remove_run(&self, run_id: &str) {
        self.0.write().remove_run(run_id);
    }

    /// `(readers, stored results)` across all owners.
    pub fn counts(&self) -> (usize, usize) {
        self.0.read().counts()
    }
}

/// Model-facing description for a generated reader.
pub(crate) fn reader_description(tool_name: &str, context: &MicroToolContext) -> String {
    if tool_name.starts_with("read_full_result_") {
        format!("Read full tool result. call_id: {}", context.call_id)
    } else if tool_name.starts_with("query_") {
        format!(
            "Query entity types: {:?}. call_id: {}",
            context.entity_types, context.call_id
        )
    } else if tool_name.starts_with("get_entity_details_") {
        format!("Get entity details. call_id: {}", context.call_id)
    } else {
        format!("Micro-tool: {}", tool_name)
    }
}

pub(crate) fn reader_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "offset": {"type": "integer", "description": "Starting offset"},
            "limit": {"type": "integer", "description": "Max results to return"}
        }
    })
}

/// Execute a reader against its stored result (paging and filters only).
pub(crate) fn read_stored_result(
    tool_name: &str,
    context: &MicroToolContext,
    stored: &Value,
    input: &Value,
) -> Value {
    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    let limit = input["limit"].as_u64().unwrap_or(100) as usize;
    let content = stored.get("content").and_then(|v| v.as_str());

    if tool_name.starts_with("read_full_result_") {
        if let Some(content) = content {
            let lines: Vec<&str> = content.lines().collect();
            let selected: Vec<String> = lines
                .iter()
                .skip(offset)
                .take(limit)
                .map(|l| l.to_string())
                .collect();
            return json!({
                "content": selected.join("\n"),
                "total_lines": lines.len(),
                "offset": offset,
                "returned": selected.len(),
                "call_id": context.call_id,
            });
        }
    } else if tool_name.starts_with("query_") {
        if let Some(content) = content {
            let query_type = input["entity_type"].as_str().unwrap_or("");
            let keyword = input["keyword"].as_str().unwrap_or("");
            let mut results = Vec::new();
            if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                if let Some(arr) = parsed.as_array() {
                    for item in arr.iter().skip(offset).take(limit) {
                        let type_match = query_type.is_empty()
                            || item
                                .get("type")
                                .and_then(|v| v.as_str())
                                .map(|t| t.contains(query_type))
                                .unwrap_or(false);
                        let keyword_match = keyword.is_empty()
                            || item
                                .to_string()
                                .to_lowercase()
                                .contains(&keyword.to_lowercase());
                        if type_match && keyword_match {
                            results.push(item.clone());
                        }
                    }
                }
            }
            return json!({
                "results": results,
                "count": results.len(),
                "call_id": context.call_id,
            });
        }
    } else if tool_name.starts_with("get_entity_details_") {
        let entity_id = input["entity_id"].as_str().unwrap_or("");
        if let Some(content) = content {
            if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                if let Some(arr) = parsed.as_array() {
                    for item in arr {
                        if item.get("id").and_then(|v| v.as_str()) == Some(entity_id) {
                            return json!({
                                "entity": item,
                                "call_id": context.call_id,
                            });
                        }
                    }
                }
            }
        }
        return json!({
            "error": "Entity not found",
            "entity_id": entity_id,
            "call_id": context.call_id,
        });
    }

    json!({
        "data": stored,
        "call_id": context.call_id,
    })
}
