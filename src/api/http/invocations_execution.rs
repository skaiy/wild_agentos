//! Invocation execution bridge (#317): cancellation registry, usage contract
//! for `succeeded`, and a stub post-create registration.
//!
//! Real `TaskExecutor` wiring (task_iri, event-bus driven transitions, budget /
//! deadline enforcement) is intentionally not in this first commit. Create
//! still leaves the resource `queued`; cancel signals a registered token so
//! the later bridge can observe it.
//!
//! The usage helpers are unit-tested here and will be called from the executor
//! completion path in a follow-up commit; allow dead_code until that wires them.
//!
//! # Isolation / projection context (H4 / #317 acceptance)
//!
//! When the real `TaskExecutor` bridge lands, an invocation-driven run MUST
//! receive a non-empty in-scope projection context. If isolation claims are
//! missing on the execution path, or projection returns empty because claims
//! were dropped, the task MUST fail closed (terminal `failed` with an explicit
//! resource mark such as `error.code = "projection_context_missing"`) — it
//! MUST NOT warn-and-succeed with empty context. Legacy agent_runner / SA /
//! scheduler paths that only `warn!` on missing claims are out of scope for
//! the invocations bridge; do not copy that behaviour here.
//!
//! TODO(#317): fail-closed when invocation execution cannot obtain in-scope
//! projection context; add a positive acceptance test that a task run via
//! invocation sees non-empty scoped projection output.
#![allow(dead_code)]

use std::sync::Arc;

use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

use super::invocations_store::{InvocationResult, InvocationUsage};

/// Incomplete `result.usage` when closing as `succeeded` (VAL-016 / VAL-017).
pub(crate) const INCOMPLETE_USAGE_ERROR_CODE: &str = "incomplete_usage";

/// In-flight cancellation tokens keyed by invocation id.
#[derive(Clone, Default)]
pub(crate) struct InvocationCancellationRegistry {
    tokens: Arc<DashMap<String, CancellationToken>>,
}

impl InvocationCancellationRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers a fresh token for `id`, replacing any previous entry.
    pub(crate) fn register(&self, id: impl Into<String>) -> CancellationToken {
        let token = CancellationToken::new();
        self.tokens.insert(id.into(), token.clone());
        token
    }

    /// Cancels the token for `id` if present. Returns whether a token existed.
    pub(crate) fn cancel(&self, id: &str) -> bool {
        match self.tokens.get(id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Removes the registry entry (terminal or abandoned). Does not cancel.
    pub(crate) fn remove(&self, id: &str) {
        self.tokens.remove(id);
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.tokens.contains_key(id)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.tokens.len()
    }
}

/// VAL-016: `succeeded` requires non-empty `model`, both token counts, and
/// integer `cost` (micro-USD). Provider / tool_calls remain optional.
pub(crate) fn usage_is_complete_for_succeeded(usage: &InvocationUsage) -> bool {
    let model_ok = usage.model.as_deref().is_some_and(|m| !m.trim().is_empty());
    model_ok
        && usage.input_tokens.is_some()
        && usage.output_tokens.is_some()
        && usage.cost.is_some()
}

/// Returns `Ok` when `result.usage` is complete enough for `succeeded`.
pub(crate) fn require_complete_usage_for_succeeded(
    result: &InvocationResult,
) -> Result<(), IncompleteUsage> {
    match result.usage.as_ref() {
        Some(usage) if usage_is_complete_for_succeeded(usage) => Ok(()),
        Some(_) => Err(IncompleteUsage::IncompleteFields),
        None => Err(IncompleteUsage::Missing),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncompleteUsage {
    Missing,
    IncompleteFields,
}

impl IncompleteUsage {
    pub(crate) fn as_error_code(self) -> &'static str {
        INCOMPLETE_USAGE_ERROR_CODE
    }

    pub(crate) fn as_message(self) -> &'static str {
        match self {
            Self::Missing => "succeeded requires result.usage",
            Self::IncompleteFields => {
                "succeeded requires result.usage.model, input_tokens, output_tokens, and cost"
            }
        }
    }
}

/// Applies a successful terminal transition only when usage is complete
/// (VAL-016 / VAL-017). Incomplete usage must not be persisted as `succeeded`.
pub(crate) fn succeeded_patch_or_reject(
    result: InvocationResult,
) -> Result<InvocationResult, IncompleteUsage> {
    require_complete_usage_for_succeeded(&result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::http::invocations_store::InvocationToolCallUsage;

    fn complete_usage() -> InvocationUsage {
        InvocationUsage {
            provider: Some("provider-x".into()),
            model: Some("model-y".into()),
            input_tokens: Some(10),
            output_tokens: Some(4),
            cost: Some(1_000),
            tool_calls: Some(vec![InvocationToolCallUsage {
                name: "search".into(),
                transport: Some("mcp".into()),
            }]),
        }
    }

    #[test]
    fn val016_complete_usage_is_accepted() {
        let result = InvocationResult {
            summary: "ok".into(),
            artifacts: vec![],
            usage: Some(complete_usage()),
        };
        assert!(succeeded_patch_or_reject(result).is_ok());
    }

    #[test]
    fn val016_missing_usage_is_rejected() {
        let result = InvocationResult {
            summary: "ok".into(),
            artifacts: vec![],
            usage: None,
        };
        assert_eq!(
            succeeded_patch_or_reject(result).unwrap_err(),
            IncompleteUsage::Missing
        );
    }

    #[test]
    fn val016_blank_model_or_missing_tokens_or_cost_is_rejected() {
        for mutate in [
            |u: &mut InvocationUsage| u.model = None,
            |u: &mut InvocationUsage| u.model = Some("  ".into()),
            |u: &mut InvocationUsage| u.input_tokens = None,
            |u: &mut InvocationUsage| u.output_tokens = None,
            |u: &mut InvocationUsage| u.cost = None,
        ] {
            let mut usage = complete_usage();
            mutate(&mut usage);
            let result = InvocationResult {
                summary: "ok".into(),
                artifacts: vec![],
                usage: Some(usage),
            };
            assert_eq!(
                succeeded_patch_or_reject(result).unwrap_err(),
                IncompleteUsage::IncompleteFields
            );
        }
    }

    #[test]
    fn val017_reported_usage_round_trips_fields() {
        let usage = complete_usage();
        let result = InvocationResult {
            summary: "done".into(),
            artifacts: vec![],
            usage: Some(usage.clone()),
        };
        let accepted = succeeded_patch_or_reject(result).unwrap();
        assert_eq!(accepted.usage.as_ref(), Some(&usage));
        assert_eq!(accepted.usage.as_ref().unwrap().cost, Some(1_000));
        assert_eq!(
            accepted.usage.as_ref().unwrap().model.as_deref(),
            Some("model-y")
        );
    }

    #[test]
    fn cancellation_registry_register_cancel_remove() {
        let registry = InvocationCancellationRegistry::new();
        let token = registry.register("inv_1");
        assert!(registry.contains("inv_1"));
        assert!(!token.is_cancelled());
        assert!(registry.cancel("inv_1"));
        assert!(token.is_cancelled());
        registry.remove("inv_1");
        assert!(!registry.contains("inv_1"));
        assert!(!registry.cancel("inv_missing"));
    }
}
