//! Per-tenant spend gates for billable kernel actions.
//!
//! The first metered path is LLM-initiated tool invocations. Usage is process
//! local and keyed only by verified [`IsolationClaims`]; it is deliberately not
//! a billing ledger or a persistence layer.

use dashmap::DashMap;
use thiserror::Error;

use crate::isolation::IsolationClaims;

/// Environment variable containing the maximum number of metered tool calls
/// permitted per tenant in this process. When unset, the gate is inactive so
/// existing callers that do not yet provide claims continue to work.
pub const TENANT_TOOL_CALL_CAP_ENV: &str = "AGENTOS_TENANT_TOOL_CALL_CAP";

/// A process-local, tenant-scoped hard cap for LLM-initiated tool invocations.
#[derive(Debug, Default)]
pub struct TenantSpendGate {
    cap: Option<u64>,
    configuration_error: Option<String>,
    usage_by_tenant: DashMap<String, u64>,
}

/// Explicit reasons a metered action was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TenantSpendError {
    #[error("metered action rejected: verified IsolationClaims are required")]
    MissingClaims,
    #[error("metered action rejected: tenant tool-call cap is misconfigured ({reason})")]
    MisconfiguredCap { reason: String },
    #[error("metered action rejected: tenant {tenant_id:?} reached its tool-call cap of {cap}")]
    CapExceeded { tenant_id: String, cap: u64 },
}

impl TenantSpendGate {
    /// Builds a gate from the configured process environment.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(TENANT_TOOL_CALL_CAP_ENV) {
            Ok(value) if value.trim().is_empty() => Self::new(None),
            Ok(value) => match value.parse::<u64>() {
                Ok(cap) => Self::new(Some(cap)),
                Err(_) => Self {
                    cap: None,
                    configuration_error: Some(format!(
                        "{TENANT_TOOL_CALL_CAP_ENV} must be a non-negative integer"
                    )),
                    usage_by_tenant: DashMap::new(),
                },
            },
            Err(std::env::VarError::NotPresent) => Self::new(None),
            Err(std::env::VarError::NotUnicode(_)) => Self {
                cap: None,
                configuration_error: Some(format!(
                    "{TENANT_TOOL_CALL_CAP_ENV} must be valid Unicode"
                )),
                usage_by_tenant: DashMap::new(),
            },
        }
    }

    /// Builds a gate with an explicit cap, primarily for embedding and tests.
    #[must_use]
    pub fn new(cap: Option<u64>) -> Self {
        Self {
            cap,
            configuration_error: None,
            usage_by_tenant: DashMap::new(),
        }
    }

    /// Reserves one billable tool invocation before it can execute.
    ///
    /// An unset cap leaves the gate inactive, including for callers without
    /// claims. Once a cap is configured, missing claims and exhausted caps
    /// fail closed. The `DashMap` entry lock makes the check-and-increment
    /// atomic for each tenant.
    pub fn reserve_tool_call(
        &self,
        claims: Option<&IsolationClaims>,
    ) -> Result<(), TenantSpendError> {
        let Some(claims) = claims else {
            return if self.cap.is_some() || self.configuration_error.is_some() {
                Err(TenantSpendError::MissingClaims)
            } else {
                Ok(())
            };
        };
        if let Some(reason) = &self.configuration_error {
            return Err(TenantSpendError::MisconfiguredCap {
                reason: reason.clone(),
            });
        }

        let tenant_id = claims.tenant_id();
        let mut usage = self
            .usage_by_tenant
            .entry(tenant_id.to_owned())
            .or_insert(0);
        if self.cap.is_some_and(|cap| *usage >= cap) {
            return Err(TenantSpendError::CapExceeded {
                tenant_id: tenant_id.to_owned(),
                cap: self.cap.expect("cap checked above"),
            });
        }
        *usage += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{TenantSpendError, TenantSpendGate};
    use crate::isolation::IsolationClaims;

    fn claims(tenant_id: &str) -> IsolationClaims {
        IsolationClaims::from_verified(tenant_id, "project", "actor").unwrap()
    }

    #[test]
    fn claims_under_cap_can_reserve_tool_calls() {
        let gate = TenantSpendGate::new(Some(2));
        let claims = claims("acme");

        assert!(gate.reserve_tool_call(Some(&claims)).is_ok());
        assert!(gate.reserve_tool_call(Some(&claims)).is_ok());
    }

    #[test]
    fn claims_over_cap_are_rejected() {
        let gate = TenantSpendGate::new(Some(1));
        let claims = claims("acme");
        gate.reserve_tool_call(Some(&claims)).unwrap();

        assert_eq!(
            gate.reserve_tool_call(Some(&claims)),
            Err(TenantSpendError::CapExceeded {
                tenant_id: "acme".to_string(),
                cap: 1,
            })
        );
    }

    #[test]
    fn missing_claims_are_allowed_when_cap_is_unset() {
        let gate = TenantSpendGate::new(None);

        assert!(gate.reserve_tool_call(None).is_ok());
    }

    #[test]
    fn missing_claims_are_rejected_when_cap_is_configured() {
        let gate = TenantSpendGate::new(Some(1));

        assert_eq!(
            gate.reserve_tool_call(None),
            Err(TenantSpendError::MissingClaims)
        );
    }
}
