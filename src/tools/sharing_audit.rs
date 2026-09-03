//! Share audit log — records all share operations without altering the SharingProtocol primary data source (HashMap)
//!
//! # Design Decisions
//!
//! - **Non-blocking**: audit is a side effect, failure does not block share operations
//! - **Memory-first**: writes to in-memory Vec, async flush to Oxigraph avoids SharingProtocol holding a Store reference
//! - **Append-only**: audit log is never modified or deleted
//!
//! # Deliberately Simplified
//!
//! - No index queries (low audit query volume, full scan is sufficient)
//! - No SPARQL inserts (keeps independent of Oxigraph)
//! - flush_to_store is a future option, currently memory-only

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The action categories that can be recorded in [`PrivilegedActionChain`].
///
/// `TenantMint` is intentionally available before tenant isolation is wired in,
/// so that mint events can use the same chain when that work lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivilegedActionKind {
    ToolCall,
    GraphWrite,
    TenantMint,
}

impl PrivilegedActionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolCall => "tool_call",
            Self::GraphWrite => "graph_write",
            Self::TenantMint => "tenant_mint",
        }
    }
}

/// A single, hash-linked record of a privileged action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegedActionEntry {
    pub kind: String,
    pub payload_hash: String,
    pub prev: Option<String>,
    pub hash: String,
}

/// Errors returned while appending to or verifying an action chain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrivilegedActionChainError {
    #[error("privileged action kind cannot be empty")]
    EmptyKind,
    #[error("privileged action payload hash cannot be empty")]
    EmptyPayloadHash,
    #[error("provided previous head does not match the current chain head")]
    PreviousHeadMismatch,
    #[error("chain link at entry {index} does not match its predecessor")]
    BrokenLink { index: usize },
    #[error("entry {index} hash does not match its contents")]
    InvalidHash { index: usize },
}

/// Append-only, in-memory hash chain for privileged actions.
///
/// Callers provide a hash of the action payload so raw privileged payloads do
/// not become part of the audit trail. The supplied predecessor must match the
/// current head, preventing accidental forks.
pub struct PrivilegedActionChain {
    entries: RwLock<Vec<PrivilegedActionEntry>>,
}

impl PrivilegedActionChain {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }

    /// Append an action and return its new chain head.
    pub fn append(
        &self,
        kind: impl AsRef<str>,
        payload_hash: impl AsRef<str>,
        prev: Option<&str>,
    ) -> Result<String, PrivilegedActionChainError> {
        let kind = kind.as_ref();
        let payload_hash = payload_hash.as_ref();
        if kind.is_empty() {
            return Err(PrivilegedActionChainError::EmptyKind);
        }
        if payload_hash.is_empty() {
            return Err(PrivilegedActionChainError::EmptyPayloadHash);
        }

        let mut entries = self.entries.write();
        let current_head = entries.last().map(|entry| entry.hash.as_str());
        if current_head != prev {
            return Err(PrivilegedActionChainError::PreviousHeadMismatch);
        }

        let prev = prev.map(ToOwned::to_owned);
        let hash = action_entry_hash(kind, payload_hash, prev.as_deref());
        entries.push(PrivilegedActionEntry {
            kind: kind.to_owned(),
            payload_hash: payload_hash.to_owned(),
            prev,
            hash: hash.clone(),
        });
        Ok(hash)
    }

    /// Return the current head without modifying the chain.
    pub fn head(&self) -> Option<String> {
        self.entries.read().last().map(|entry| entry.hash.clone())
    }

    /// Return a snapshot suitable for read-only inspection or export.
    pub fn entries(&self) -> Vec<PrivilegedActionEntry> {
        self.entries.read().clone()
    }

    /// Verify every link from genesis through the current head.
    pub fn verify(&self) -> Result<(), PrivilegedActionChainError> {
        verify_privileged_action_chain(&self.entries.read())
    }
}

impl Default for PrivilegedActionChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Verify an exported action-chain snapshot without modifying it.
///
/// This is intentionally independent of HTTP or UI code so a future Admin
/// surface can verify an exported chain directly.
pub fn verify_privileged_action_chain(
    entries: &[PrivilegedActionEntry],
) -> Result<(), PrivilegedActionChainError> {
    let mut expected_prev: Option<&str> = None;
    for (index, entry) in entries.iter().enumerate() {
        if entry.prev.as_deref() != expected_prev {
            return Err(PrivilegedActionChainError::BrokenLink { index });
        }

        let expected_hash = action_entry_hash(&entry.kind, &entry.payload_hash, expected_prev);
        if entry.hash != expected_hash {
            return Err(PrivilegedActionChainError::InvalidHash { index });
        }
        expected_prev = Some(&entry.hash);
    }
    Ok(())
}

fn action_entry_hash(kind: &str, payload_hash: &str, prev: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"wild-agentos:privileged-action:v1\0");
    hash_field(&mut hasher, kind.as_bytes());
    hash_field(&mut hasher, payload_hash.as_bytes());
    match prev {
        Some(prev) => hash_field(&mut hasher, prev.as_bytes()),
        None => hash_field(&mut hasher, &[]),
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Share event type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharingEvent {
    Created,
    Resolved,
    Revoked,
    Expired,
}

impl SharingEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Resolved => "Resolved",
            Self::Revoked => "Revoked",
            Self::Expired => "Expired",
        }
    }
}

/// Share audit entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharingAuditEntry {
    pub event_type: SharingEvent,
    pub share_id: String,
    pub source_agent: String,
    pub target_agent: String,
    pub node_iri: String,
    pub share_type: String,
    pub permission: String,
    pub ttl_seconds: Option<u64>,
    pub timestamp: DateTime<Utc>,
}

/// Share audit log
///
/// Stores all audit entries in an in-memory Vec, avoiding SharingProtocol holding an Oxigraph Store reference.
/// Can be exported via `flush_to_store()` when an external Store is available.
pub struct SharingAuditLog {
    entries: RwLock<Vec<SharingAuditEntry>>,
}

impl SharingAuditLog {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }

    /// Record an audit entry
    pub fn log(&self, entry: SharingAuditEntry) {
        self.entries.write().push(entry);
    }

    /// Log share creation
    #[allow(clippy::too_many_arguments)]
    pub fn log_share_created(
        &self,
        share_id: &str,
        source_agent: &str,
        target_agent: &str,
        node_iri: &str,
        share_type: &str,
        permission: &str,
        ttl_seconds: Option<u64>,
    ) {
        self.log(SharingAuditEntry {
            event_type: SharingEvent::Created,
            share_id: share_id.to_string(),
            source_agent: source_agent.to_string(),
            target_agent: target_agent.to_string(),
            node_iri: node_iri.to_string(),
            share_type: share_type.to_string(),
            permission: permission.to_string(),
            ttl_seconds,
            timestamp: Utc::now(),
        });
    }

    /// Log share resolution
    pub fn log_share_resolved(&self, share_id: &str, by_agent: &str) {
        self.log(SharingAuditEntry {
            event_type: SharingEvent::Resolved,
            share_id: share_id.to_string(),
            source_agent: String::new(),
            target_agent: by_agent.to_string(),
            node_iri: String::new(),
            share_type: String::new(),
            permission: String::new(),
            ttl_seconds: None,
            timestamp: Utc::now(),
        });
    }

    /// Log share revocation
    pub fn log_share_revoked(&self, share_id: &str) {
        self.log(SharingAuditEntry {
            event_type: SharingEvent::Revoked,
            share_id: share_id.to_string(),
            source_agent: String::new(),
            target_agent: String::new(),
            node_iri: String::new(),
            share_type: String::new(),
            permission: String::new(),
            ttl_seconds: None,
            timestamp: Utc::now(),
        });
    }

    /// Query share history received by an Agent
    pub fn query_shares_for_agent(&self, agent_iri: &str) -> Vec<SharingAuditEntry> {
        self.entries
            .read()
            .iter()
            .filter(|e| e.target_agent == agent_iri)
            .cloned()
            .collect()
    }

    /// Query share history for an IRI
    pub fn query_shares_for_node(&self, node_iri: &str) -> Vec<SharingAuditEntry> {
        self.entries
            .read()
            .iter()
            .filter(|e| e.node_iri == node_iri)
            .cloned()
            .collect()
    }

    /// Get all audit entries
    pub fn all_entries(&self) -> Vec<SharingAuditEntry> {
        self.entries.read().clone()
    }

    /// Number of audit entries
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Serialize in-memory audit entries to JSON (for external consumption)
    pub fn to_json(&self) -> serde_json::Value {
        let entries = self.entries.read();
        serde_json::to_value(&*entries).unwrap_or_default()
    }
}

impl Default for SharingAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_action_chain_appends_and_verifies() {
        let chain = PrivilegedActionChain::new();
        let tool_head = chain
            .append(PrivilegedActionKind::ToolCall.as_str(), "sha256:tool", None)
            .unwrap();
        let graph_head = chain
            .append(
                PrivilegedActionKind::GraphWrite.as_str(),
                "sha256:graph",
                Some(&tool_head),
            )
            .unwrap();
        let tenant_head = chain
            .append(
                PrivilegedActionKind::TenantMint.as_str(),
                "sha256:tenant",
                Some(&graph_head),
            )
            .unwrap();

        assert_eq!(chain.head(), Some(tenant_head));
        assert_eq!(chain.entries().len(), 3);
        assert!(chain.verify().is_ok());
        assert!(verify_privileged_action_chain(&chain.entries()).is_ok());
    }

    #[test]
    fn privileged_action_chain_rejects_a_forked_predecessor() {
        let chain = PrivilegedActionChain::new();
        chain.append("tool_call", "sha256:tool", None).unwrap();

        assert_eq!(
            chain.append("graph_write", "sha256:graph", None),
            Err(PrivilegedActionChainError::PreviousHeadMismatch)
        );
    }

    #[test]
    fn privileged_action_chain_detects_a_mutated_middle_byte() {
        let chain = PrivilegedActionChain::new();
        let first = chain.append("tool_call", "sha256:first", None).unwrap();
        let second = chain
            .append("graph_write", "sha256:second", Some(&first))
            .unwrap();
        chain
            .append("tenant_mint", "sha256:third", Some(&second))
            .unwrap();

        let mut entries = chain.entries();
        entries[1].payload_hash.replace_range(7..8, "x");

        assert_eq!(
            verify_privileged_action_chain(&entries),
            Err(PrivilegedActionChainError::InvalidHash { index: 1 })
        );
    }

    #[test]
    fn test_log_share_created() {
        let log = SharingAuditLog::new();
        log.log_share_created(
            "iri://share/abc",
            "iri://agent/pa",
            "iri://agent/da",
            "iri://task/123",
            "Projection",
            "Read",
            Some(3600),
        );
        assert_eq!(log.len(), 1);

        let entry = &log.all_entries()[0];
        assert_eq!(entry.event_type, SharingEvent::Created);
        assert_eq!(entry.source_agent, "iri://agent/pa");
        assert_eq!(entry.share_type, "Projection");
    }

    #[test]
    fn test_log_share_resolved() {
        let log = SharingAuditLog::new();
        log.log_share_resolved("iri://share/abc", "iri://agent/da");
        assert_eq!(log.len(), 1);
        assert_eq!(log.all_entries()[0].event_type, SharingEvent::Resolved);
    }

    #[test]
    fn test_log_share_revoked() {
        let log = SharingAuditLog::new();
        log.log_share_revoked("iri://share/abc");
        assert_eq!(log.len(), 1);
        assert_eq!(log.all_entries()[0].event_type, SharingEvent::Revoked);
    }

    #[test]
    fn test_query_by_agent() {
        let log = SharingAuditLog::new();
        log.log_share_created("s1", "src1", "agent:da1", "n1", "Full", "Read", None);
        log.log_share_created("s2", "src2", "agent:da2", "n2", "Proj", "Read", None);
        log.log_share_created("s3", "src3", "agent:da1", "n3", "Full", "Write", None);

        let results = log.query_shares_for_agent("agent:da1");
        assert_eq!(results.len(), 2);

        let results2 = log.query_shares_for_agent("nonexistent");
        assert!(results2.is_empty());
    }

    #[test]
    fn test_query_by_node() {
        let log = SharingAuditLog::new();
        log.log_share_created("s1", "src1", "tgt1", "iri://task/42", "Full", "Read", None);
        log.log_share_created("s2", "src2", "tgt2", "iri://task/99", "Proj", "Read", None);
        log.log_share_created("s3", "src3", "tgt3", "iri://task/42", "Full", "Write", None);

        let results = log.query_shares_for_node("iri://task/42");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_to_json() {
        let log = SharingAuditLog::new();
        log.log_share_created("s1", "a", "b", "n1", "Full", "Read", None);
        let json = log.to_json();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_empty_log() {
        let log = SharingAuditLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
        assert!(log.query_shares_for_agent("any").is_empty());
    }
}
