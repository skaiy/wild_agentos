//! Governed emergent-tool lifecycle.
//!
//! This module keeps generated tool definitions out of the production skill
//! registry until every stage has both a successful gate verdict and an
//! explicit human promotion. Records are tenant-scoped even when the backing
//! L0 store is already opened with verified tenant claims.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::memory::l0_store::L0Store;
use crate::CoreError;

const PREFIX: &str = "iri://governance/emergent-tool/";

/// Lifecycle states for a generated tool. Promotion is deliberately linear.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmergentToolState {
    Proposed,
    SessionEnabled,
    TenantCandidate,
    Published,
    Rejected,
}

impl EmergentToolState {
    fn next(self) -> Option<Self> {
        match self {
            Self::Proposed => Some(Self::SessionEnabled),
            Self::SessionEnabled => Some(Self::TenantCandidate),
            Self::TenantCandidate => Some(Self::Published),
            Self::Published | Self::Rejected => None,
        }
    }
}

/// The gate run immediately before a human promotion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmergentToolGateScope {
    Session,
    TenantCandidate,
    Publish,
}

impl EmergentToolGateScope {
    fn for_target(target: EmergentToolState) -> Result<Self, CoreError> {
        match target {
            EmergentToolState::SessionEnabled => Ok(Self::Session),
            EmergentToolState::TenantCandidate => Ok(Self::TenantCandidate),
            EmergentToolState::Published => Ok(Self::Publish),
            EmergentToolState::Proposed | EmergentToolState::Rejected => {
                Err(CoreError::ValidationFailed {
                    message: "Only promotable lifecycle states have a gate".to_string(),
                })
            }
        }
    }
}

/// A gate verdict contains only review metadata; generated source is never
/// copied into audit data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmergentToolGateVerdict {
    pub passed: bool,
    pub evaluator: String,
    pub summary: String,
    pub evaluated_at: DateTime<Utc>,
}

impl EmergentToolGateVerdict {
    pub fn passed(evaluator: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            passed: true,
            evaluator: evaluator.into(),
            summary: summary.into(),
            evaluated_at: Utc::now(),
        }
    }

    pub fn rejected(evaluator: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            passed: false,
            evaluator: evaluator.into(),
            summary: summary.into(),
            evaluated_at: Utc::now(),
        }
    }
}

/// Adapter boundary for an external sandbox/test/judge implementation.
///
/// The kernel does not execute generated code itself. Implementations may
/// delegate to an isolated sandbox provider; without one, callers must not
/// promote the tool.
pub trait EmergentToolGate: Send + Sync {
    fn evaluate(
        &self,
        candidate: &EmergentToolCandidate,
        scope: EmergentToolGateScope,
    ) -> Result<EmergentToolGateVerdict, CoreError>;
}

/// Definition submitted by a generator. It remains data until publication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmergentToolCandidate {
    pub tool_name: String,
    pub definition: serde_json::Value,
}

/// Durable audit trail for a human-controlled state transition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmergentToolPromotion {
    pub from: EmergentToolState,
    pub to: EmergentToolState,
    pub approver: String,
    pub gate_scope: EmergentToolGateScope,
    pub gate: EmergentToolGateVerdict,
    pub promoted_at: DateTime<Utc>,
}

/// A tenant-bound generated tool and its immutable promotion history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmergentToolRecord {
    pub tool_id: String,
    pub tenant_id: String,
    pub session_id: String,
    pub candidate: EmergentToolCandidate,
    pub state: EmergentToolState,
    pub promotions: Vec<EmergentToolPromotion>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// L0-backed graded store for generated tools.
///
/// Production construction must use an L0 store opened through
/// `L0Store::open_for_claims`; the explicit tenant field is a second isolation
/// check that prevents accidental cross-tenant use by application code.
pub struct EmergentToolStore {
    l0: Arc<L0Store>,
    tenant_id: String,
}

impl EmergentToolStore {
    pub fn new(l0: Arc<L0Store>, tenant_id: impl Into<String>) -> Result<Self, CoreError> {
        let tenant_id = tenant_id.into();
        if tenant_id.trim().is_empty() {
            return Err(CoreError::ValidationFailed {
                message: "Emergent tool store requires a verified tenant id".to_string(),
            });
        }
        Ok(Self { l0, tenant_id })
    }

    /// Stores a proposal only. This method cannot enable or publish a tool.
    pub fn propose(
        &self,
        session_id: impl Into<String>,
        candidate: EmergentToolCandidate,
    ) -> Result<EmergentToolRecord, CoreError> {
        let session_id = session_id.into();
        if session_id.trim().is_empty() || candidate.tool_name.trim().is_empty() {
            return Err(CoreError::ValidationFailed {
                message: "Emergent tool proposals require non-empty session and tool names"
                    .to_string(),
            });
        }
        let now = Utc::now();
        let record = EmergentToolRecord {
            tool_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: self.tenant_id.clone(),
            session_id,
            candidate,
            state: EmergentToolState::Proposed,
            promotions: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        self.save(&record)?;
        Ok(record)
    }

    /// Returns no record for another tenant, avoiding cross-tenant enumeration.
    pub fn get(&self, tool_id: &str) -> Result<Option<EmergentToolRecord>, CoreError> {
        let Some(entry) = self.l0.retrieve(&format!("{PREFIX}{tool_id}"))? else {
            return Ok(None);
        };
        let record: EmergentToolRecord =
            serde_json::from_str(&entry.content).map_err(|error| CoreError::StorageError {
                message: format!("Failed to deserialize emergent tool record: {error}"),
            })?;
        Ok((record.tenant_id == self.tenant_id).then_some(record))
    }

    /// Runs the required gate and, only after it passes, records a named human
    /// approval for the one legal next state. There is no direct publish API.
    pub fn approve_and_promote(
        &self,
        tool_id: &str,
        approver: &str,
        gate: &dyn EmergentToolGate,
    ) -> Result<EmergentToolRecord, CoreError> {
        if approver.trim().is_empty() {
            return Err(CoreError::ValidationFailed {
                message: "Emergent tool promotion requires a non-empty human approver".to_string(),
            });
        }
        let mut record = self.get(tool_id)?.ok_or_else(|| CoreError::SkillNotFound {
            iri: "Emergent tool not found in tenant scope".to_string(),
        })?;
        let target = record
            .state
            .next()
            .ok_or_else(|| CoreError::ValidationFailed {
                message: "Rejected or published emergent tools cannot be promoted".to_string(),
            })?;
        let gate_scope = EmergentToolGateScope::for_target(target)?;
        let verdict = gate.evaluate(&record.candidate, gate_scope)?;
        if !verdict.passed {
            return Err(CoreError::ValidationFailed {
                message: format!(
                    "{} gate rejected promotion: {}",
                    verdict.evaluator, verdict.summary
                ),
            });
        }
        record.promotions.push(EmergentToolPromotion {
            from: record.state,
            to: target,
            approver: approver.trim().to_string(),
            gate_scope,
            gate: verdict,
            promoted_at: Utc::now(),
        });
        record.state = target;
        record.updated_at = Utc::now();
        self.save(&record)?;
        Ok(record)
    }

    /// Human rejection is terminal and cannot publish or execute the tool.
    pub fn reject(&self, tool_id: &str, reviewer: &str) -> Result<EmergentToolRecord, CoreError> {
        if reviewer.trim().is_empty() {
            return Err(CoreError::ValidationFailed {
                message: "Emergent tool rejection requires a non-empty human reviewer".to_string(),
            });
        }
        let mut record = self.get(tool_id)?.ok_or_else(|| CoreError::SkillNotFound {
            iri: "Emergent tool not found in tenant scope".to_string(),
        })?;
        if matches!(
            record.state,
            EmergentToolState::Published | EmergentToolState::Rejected
        ) {
            return Err(CoreError::ValidationFailed {
                message: "Published or rejected emergent tools are terminal".to_string(),
            });
        }
        record.state = EmergentToolState::Rejected;
        record.updated_at = Utc::now();
        self.save(&record)?;
        Ok(record)
    }

    fn save(&self, record: &EmergentToolRecord) -> Result<(), CoreError> {
        let content = serde_json::to_string(record).map_err(|error| CoreError::StorageError {
            message: format!("Failed to serialize emergent tool record: {error}"),
        })?;
        self.l0
            .store(&format!("{PREFIX}{}", record.tool_id), &content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PassingGate;
    impl EmergentToolGate for PassingGate {
        fn evaluate(
            &self,
            _candidate: &EmergentToolCandidate,
            scope: EmergentToolGateScope,
        ) -> Result<EmergentToolGateVerdict, CoreError> {
            Ok(EmergentToolGateVerdict::passed(
                "sandbox-judge",
                format!("{scope:?} checks passed"),
            ))
        }
    }

    struct RejectingGate;
    impl EmergentToolGate for RejectingGate {
        fn evaluate(
            &self,
            _candidate: &EmergentToolCandidate,
            _scope: EmergentToolGateScope,
        ) -> Result<EmergentToolGateVerdict, CoreError> {
            Ok(EmergentToolGateVerdict::rejected(
                "sandbox-judge",
                "test failed",
            ))
        }
    }

    fn store(tenant: &str) -> EmergentToolStore {
        let dir = tempfile::tempdir().unwrap();
        // Keep the directory alive for the fixture by leaking this test-only
        // path; redb owns its file handles until the store is dropped.
        let path = dir.keep();
        EmergentToolStore::new(
            Arc::new(L0Store::new(path.to_str().unwrap()).unwrap()),
            tenant,
        )
        .unwrap()
    }

    fn candidate() -> EmergentToolCandidate {
        EmergentToolCandidate {
            tool_name: "generated_search".into(),
            definition: serde_json::json!({"kind": "tool"}),
        }
    }

    #[test]
    fn publication_requires_three_gated_human_promotions() {
        let store = store("tenant-a");
        let proposed = store.propose("session-1", candidate()).unwrap();
        assert_eq!(proposed.state, EmergentToolState::Proposed);

        let session = store
            .approve_and_promote(&proposed.tool_id, "reviewer-1", &PassingGate)
            .unwrap();
        assert_eq!(session.state, EmergentToolState::SessionEnabled);
        let tenant = store
            .approve_and_promote(&session.tool_id, "reviewer-2", &PassingGate)
            .unwrap();
        assert_eq!(tenant.state, EmergentToolState::TenantCandidate);
        let published = store
            .approve_and_promote(&tenant.tool_id, "reviewer-3", &PassingGate)
            .unwrap();
        assert_eq!(published.state, EmergentToolState::Published);
        assert_eq!(published.promotions.len(), 3);
    }

    #[test]
    fn failed_gate_cannot_bypass_to_publish() {
        let store = store("tenant-a");
        let proposed = store.propose("session-1", candidate()).unwrap();
        assert!(store
            .approve_and_promote(&proposed.tool_id, "reviewer", &RejectingGate)
            .is_err());
        assert_eq!(
            store.get(&proposed.tool_id).unwrap().unwrap().state,
            EmergentToolState::Proposed
        );
    }

    #[test]
    fn tenant_scope_hides_other_tenant_records() {
        let dir = tempfile::tempdir().unwrap();
        let l0 = Arc::new(L0Store::new(dir.path().to_str().unwrap()).unwrap());
        let tenant_a = EmergentToolStore::new(l0.clone(), "tenant-a").unwrap();
        let tenant_b = EmergentToolStore::new(l0, "tenant-b").unwrap();
        let record = tenant_a.propose("session-1", candidate()).unwrap();

        assert!(tenant_b.get(&record.tool_id).unwrap().is_none());
        assert!(tenant_b
            .approve_and_promote(&record.tool_id, "reviewer", &PassingGate)
            .is_err());
    }
}
