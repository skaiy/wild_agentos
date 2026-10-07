//! FIFO running caps, deadline / budget / `input_ref` enforcement (#331).
//!
//! Create already parses `deadline` / `budget` / `input_ref` / `agent_revision`
//! and rejects unsupported pins / unregistered schemes. This module enforces
//! those fields on the execution path and gates starts behind global +
//! per-scope **running** caps (FIFO by `created_at` within a scope).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify};

use super::invocations_store::{Invocation, InvocationBudget, InvocationUsage};

/// Hit request.budget.max_* during / after a run.
pub(crate) const BUDGET_EXCEEDED_ERROR_CODE: &str = "budget_exceeded";
/// `input_ref` bytes do not match the pinned sha256.
pub(crate) const INPUT_DIGEST_MISMATCH_ERROR_CODE: &str = "input_digest_mismatch";
/// Resolver returned an error / empty fetch before the digest check.
pub(crate) const INPUT_REF_FETCH_FAILED_ERROR_CODE: &str = "input_ref_fetch_failed";

/// Default global max **running** invocations (env override below).
pub(crate) const DEFAULT_MAX_RUNNING_GLOBAL: usize = 64;
/// Default per-scope (tenant+project) max **running** invocations.
pub(crate) const DEFAULT_MAX_RUNNING_PER_SCOPE: usize = 8;
/// Env: global running cap (≥ 1).
pub(crate) const MAX_RUNNING_GLOBAL_ENV: &str = "AGENTOS_INVOCATION_MAX_RUNNING_GLOBAL";
/// Env: per-scope running cap (≥ 1).
pub(crate) const MAX_RUNNING_PER_SCOPE_ENV: &str = "AGENTOS_INVOCATION_MAX_RUNNING_PER_SCOPE";

/// Running-concurrency limits (distinct from `MAX_ACTIVE` non-terminal create cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvocationRunningLimits {
    pub global: usize,
    pub per_scope: usize,
}

impl Default for InvocationRunningLimits {
    fn default() -> Self {
        Self {
            global: DEFAULT_MAX_RUNNING_GLOBAL,
            per_scope: DEFAULT_MAX_RUNNING_PER_SCOPE,
        }
    }
}

impl InvocationRunningLimits {
    pub(crate) fn from_env() -> Self {
        Self::from_vars(|key| std::env::var(key).ok())
    }

    pub(crate) fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let positive = |key: &str| {
            lookup(key)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .filter(|v| *v >= 1)
                .map(|v| usize::try_from(v).unwrap_or(usize::MAX))
        };
        let mut limits = Self::default();
        if let Some(v) = positive(MAX_RUNNING_GLOBAL_ENV) {
            limits.global = v;
        }
        if let Some(v) = positive(MAX_RUNNING_PER_SCOPE_ENV) {
            limits.per_scope = v;
        }
        limits
    }
}

/// `(tenant_id, project_id)` running-cap scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RunningScope {
    pub tenant_id: String,
    pub project_id: String,
}

impl RunningScope {
    pub(crate) fn from_invocation(invocation: &Invocation) -> Self {
        Self {
            tenant_id: invocation.tenant_id.clone(),
            project_id: invocation.project_id.clone(),
        }
    }
}

/// In-memory running slot ledger (admission control; store state is authoritative
/// for lifecycle, this ledger prevents over-starting before `queued→running`).
///
/// `admitted` holds ids that already own a slot but may still be `queued` in the
/// store until `queued→running` completes — peers must skip them when picking
/// the FIFO head, or the head blocks the whole scope until it finishes.
#[derive(Debug)]
pub(crate) struct FifoRunningSlots {
    limits: InvocationRunningLimits,
    global: usize,
    per_scope: HashMap<RunningScope, usize>,
    admitted: HashSet<String>,
}

impl FifoRunningSlots {
    pub(crate) fn new(limits: InvocationRunningLimits) -> Self {
        Self {
            limits,
            global: 0,
            per_scope: HashMap::new(),
            admitted: HashSet::new(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn limits(&self) -> InvocationRunningLimits {
        self.limits
    }

    #[allow(dead_code)]
    pub(crate) fn global_running(&self) -> usize {
        self.global
    }

    pub(crate) fn scope_running(&self, scope: &RunningScope) -> usize {
        self.per_scope.get(scope).copied().unwrap_or(0)
    }

    pub(crate) fn admitted_ids(&self) -> HashSet<String> {
        self.admitted.clone()
    }

    /// Acquires one running slot when both global and per-scope caps allow.
    pub(crate) fn try_acquire(&mut self, scope: &RunningScope) -> bool {
        let scope_count = self.scope_running(scope);
        if self.global >= self.limits.global || scope_count >= self.limits.per_scope {
            return false;
        }
        self.global = self.global.saturating_add(1);
        *self.per_scope.entry(scope.clone()).or_insert(0) += 1;
        true
    }

    /// Atomically take a slot and mark `id` admitted (same lock as release).
    pub(crate) fn try_admit(&mut self, scope: &RunningScope, id: &str) -> bool {
        if self.admitted.contains(id) {
            return true;
        }
        if !self.try_acquire(scope) {
            return false;
        }
        self.admitted.insert(id.to_string());
        true
    }

    pub(crate) fn release(&mut self, scope: &RunningScope, id: &str) {
        self.admitted.remove(id);
        if self.global > 0 {
            self.global -= 1;
        }
        if let Some(count) = self.per_scope.get_mut(scope) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_scope.remove(scope);
            }
        }
    }
}

/// Shared scheduler state: slots + wakeups when a slot frees or a peer is admitted.
#[derive(Debug)]
pub(crate) struct FifoScheduler {
    slots: Mutex<FifoRunningSlots>,
    notify: Notify,
}

impl FifoScheduler {
    pub(crate) fn new(limits: InvocationRunningLimits) -> Self {
        Self {
            slots: Mutex::new(FifoRunningSlots::new(limits)),
            notify: Notify::new(),
        }
    }

    pub(crate) fn with_defaults() -> Self {
        Self::new(InvocationRunningLimits::from_env())
    }

    /// Snapshot of ids that hold a slot (may still be store-`queued`).
    pub(crate) async fn admitted_ids(&self) -> HashSet<String> {
        self.slots.lock().await.admitted_ids()
    }

    /// Take a slot + mark admitted under one lock; wakes peers so the next
    /// FIFO head (excluding admitted) can proceed under `per_scope > 1`.
    pub(crate) async fn try_admit(&self, scope: &RunningScope, id: &str) -> bool {
        let ok = self.slots.lock().await.try_admit(scope, id);
        if ok {
            self.notify.notify_waiters();
        }
        ok
    }

    pub(crate) async fn release(&self, scope: &RunningScope, id: &str) {
        self.slots.lock().await.release(scope, id);
        self.notify.notify_waiters();
    }

    pub(crate) fn notify(&self) -> &Notify {
        &self.notify
    }

    #[allow(dead_code)]
    pub(crate) async fn snapshot(&self) -> (usize, InvocationRunningLimits) {
        let slots = self.slots.lock().await;
        (slots.global_running(), slots.limits())
    }

    #[cfg(test)]
    pub(crate) async fn scope_running_count(&self, scope: &RunningScope) -> usize {
        self.slots.lock().await.scope_running(scope)
    }
}

/// Pluggable `input_ref` scheme → resolver registry. v0.12 ships empty.
#[async_trait]
pub(crate) trait InputRefResolver: Send + Sync {
    async fn resolve(&self, uri: &str) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Default)]
pub(crate) struct InputRefRegistry {
    by_scheme: Arc<DashMap<String, Arc<dyn InputRefResolver>>>,
}

impl InputRefRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Test-only until a deployment hook registers schemes (v0.12 ships empty).
    #[cfg(test)]
    pub(crate) fn register(&self, scheme: impl Into<String>, resolver: Arc<dyn InputRefResolver>) {
        self.by_scheme.insert(scheme.into(), resolver);
    }

    pub(crate) fn has_scheme(&self, scheme: &str) -> bool {
        self.by_scheme.contains_key(scheme)
    }

    /// Extracts the scheme from `uri` (`scheme://…`). `None` if malformed.
    pub(crate) fn scheme_of(uri: &str) -> Option<&str> {
        let (scheme, rest) = uri.split_once("://")?;
        if scheme.is_empty() || rest.is_empty() {
            return None;
        }
        Some(scheme)
    }

    pub(crate) async fn resolve(&self, uri: &str) -> Result<Vec<u8>, String> {
        let scheme =
            Self::scheme_of(uri).ok_or_else(|| "input_ref.uri missing scheme".to_string())?;
        let resolver = self
            .by_scheme
            .get(scheme)
            .ok_or_else(|| format!("no resolver for scheme {scheme}"))?
            .clone();
        resolver.resolve(uri).await
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Fetches `input_ref` and checks the pinned digest.
/// Returns `Ok(bytes)` on match; typed error codes otherwise.
pub(crate) async fn fetch_and_verify_input_ref(
    registry: &InputRefRegistry,
    uri: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>, (&'static str, String)> {
    let bytes = registry
        .resolve(uri)
        .await
        .map_err(|message| (INPUT_REF_FETCH_FAILED_ERROR_CODE, message))?;
    let actual = sha256_hex(&bytes);
    if actual != expected_sha256 {
        return Err((
            INPUT_DIGEST_MISMATCH_ERROR_CODE,
            format!("input_ref digest mismatch: expected {expected_sha256}, got {actual}"),
        ));
    }
    Ok(bytes)
}

/// Whether reported `usage` violates any present budget member.
/// `max_tokens` counts `input_tokens + output_tokens` when both are known;
/// if only one side is present it is compared alone. Missing metered fields
/// for a present limit do **not** trip the limit (best-effort; fail only on
/// evidence of exceeding).
pub(crate) fn budget_is_exceeded(budget: &InvocationBudget, usage: &InvocationUsage) -> bool {
    if let Some(max_tokens) = budget.max_tokens {
        let used = usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens.unwrap_or(0));
        if (usage.input_tokens.is_some() || usage.output_tokens.is_some()) && used > max_tokens {
            return true;
        }
    }
    if let Some(max_tool_calls) = budget.max_tool_calls {
        let used = usage
            .tool_calls
            .as_ref()
            .map(|calls| calls.len() as u64)
            .unwrap_or(0);
        if usage.tool_calls.is_some() && used > max_tool_calls {
            return true;
        }
    }
    if let Some(max_cost) = budget.max_cost {
        if let Some(cost) = usage.cost {
            if cost > max_cost {
                return true;
            }
        }
    }
    false
}

/// Parses an RFC 3339 deadline into UTC. `None` if unparsable.
pub(crate) fn parse_deadline_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Duration until `deadline` from now; `None` if already past or unparsable.
pub(crate) fn duration_until_deadline(raw: &str) -> Option<std::time::Duration> {
    let deadline = parse_deadline_utc(raw)?;
    let now = Utc::now();
    if deadline <= now {
        return Some(std::time::Duration::ZERO);
    }
    let millis = (deadline - now).num_milliseconds();
    if millis <= 0 {
        return Some(std::time::Duration::ZERO);
    }
    Some(std::time::Duration::from_millis(millis as u64))
}

/// Whether `raw` deadline is already at/past server time.
pub(crate) fn deadline_already_due(raw: &str) -> bool {
    match duration_until_deadline(raw) {
        Some(d) => d.is_zero(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::http::invocations_store::InvocationToolCallUsage;

    struct MemoryResolver {
        body: Vec<u8>,
    }

    #[async_trait]
    impl InputRefResolver for MemoryResolver {
        async fn resolve(&self, _uri: &str) -> Result<Vec<u8>, String> {
            Ok(self.body.clone())
        }
    }

    #[test]
    fn running_limits_from_vars() {
        let limits = InvocationRunningLimits::from_vars(|key| match key {
            MAX_RUNNING_GLOBAL_ENV => Some("3".into()),
            MAX_RUNNING_PER_SCOPE_ENV => Some("2".into()),
            _ => None,
        });
        assert_eq!(limits.global, 3);
        assert_eq!(limits.per_scope, 2);
    }

    #[test]
    fn fifo_slots_respect_global_and_scope() {
        let mut slots = FifoRunningSlots::new(InvocationRunningLimits {
            global: 2,
            per_scope: 1,
        });
        let a = RunningScope {
            tenant_id: "t1".into(),
            project_id: "p1".into(),
        };
        let b = RunningScope {
            tenant_id: "t1".into(),
            project_id: "p2".into(),
        };
        assert!(slots.try_acquire(&a));
        assert!(!slots.try_acquire(&a), "per-scope cap");
        assert!(slots.try_acquire(&b), "other scope ok");
        assert!(!slots.try_acquire(&b), "global cap");
        slots.release(&a, "a1");
        assert!(slots.try_acquire(&a), "slot freed");
    }

    #[tokio::test]
    async fn input_ref_digest_match_and_mismatch() {
        let body = b"hello-input-ref";
        let registry = InputRefRegistry::new();
        registry.register(
            "mem",
            Arc::new(MemoryResolver {
                body: body.to_vec(),
            }),
        );
        let digest = sha256_hex(body);
        let ok = fetch_and_verify_input_ref(&registry, "mem://x", &digest)
            .await
            .unwrap();
        assert_eq!(ok, body);
        let err = fetch_and_verify_input_ref(&registry, "mem://x", &("0".repeat(64)))
            .await
            .unwrap_err();
        assert_eq!(err.0, INPUT_DIGEST_MISMATCH_ERROR_CODE);
    }

    #[test]
    fn budget_exceeded_on_tokens_cost_tools() {
        let budget = InvocationBudget {
            max_tokens: Some(10),
            max_tool_calls: Some(1),
            max_cost: Some(100),
        };
        let mut usage = InvocationUsage {
            input_tokens: Some(6),
            output_tokens: Some(5),
            cost: Some(50),
            tool_calls: Some(vec![InvocationToolCallUsage {
                name: "a".into(),
                transport: None,
            }]),
            ..Default::default()
        };
        assert!(budget_is_exceeded(&budget, &usage));
        usage.output_tokens = Some(3);
        assert!(!budget_is_exceeded(&budget, &usage));
        usage.cost = Some(101);
        assert!(budget_is_exceeded(&budget, &usage));
        usage.cost = Some(50);
        usage.tool_calls = Some(vec![
            InvocationToolCallUsage {
                name: "a".into(),
                transport: None,
            },
            InvocationToolCallUsage {
                name: "b".into(),
                transport: None,
            },
        ]);
        assert!(budget_is_exceeded(&budget, &usage));
    }

    #[test]
    fn budget_token_sum_saturates() {
        let budget = InvocationBudget {
            max_tokens: Some(u64::MAX - 1),
            ..Default::default()
        };
        let usage = InvocationUsage {
            input_tokens: Some(u64::MAX),
            output_tokens: Some(1),
            ..Default::default()
        };

        assert!(budget_is_exceeded(&budget, &usage));
    }

    #[test]
    fn scheme_of_uri() {
        assert_eq!(InputRefRegistry::scheme_of("mem://a/b"), Some("mem"));
        assert_eq!(InputRefRegistry::scheme_of("no-scheme"), None);
        assert_eq!(InputRefRegistry::scheme_of("://missing"), None);
    }
}
