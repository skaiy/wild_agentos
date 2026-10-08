//! FIFO running caps, deadline / budget / `input_ref` enforcement (#331).
//!
//! Create already parses `deadline` / `budget` / `input_ref` / `agent_revision`
//! and rejects unsupported pins / unregistered schemes. This module enforces
//! those fields on the execution path and gates starts behind global,
//! per-tenant and per-scope **running** caps (FIFO by `created_at` within a
//! scope). All caps are in-memory and apply to this process only.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use super::invocations_store::{Invocation, InvocationBudget, InvocationUsage};

/// Hit request.budget.max_* during / after a run.
pub(crate) const BUDGET_EXCEEDED_ERROR_CODE: &str = "budget_exceeded";
/// `input_ref` bytes do not match the pinned sha256.
pub(crate) const INPUT_DIGEST_MISMATCH_ERROR_CODE: &str = "input_digest_mismatch";
/// Resolver returned an error / empty fetch before the digest check.
pub(crate) const INPUT_REF_FETCH_FAILED_ERROR_CODE: &str = "input_ref_fetch_failed";

/// Default global max **running** invocations (env override below).
pub(crate) const DEFAULT_MAX_RUNNING_GLOBAL: usize = 64;
/// Default per-tenant max **running** invocations, summed over all of the
/// tenant's projects (a quarter of the default global cap).
pub(crate) const DEFAULT_MAX_RUNNING_PER_TENANT: usize = 16;
/// Default per-scope (tenant+project) max **running** invocations.
pub(crate) const DEFAULT_MAX_RUNNING_PER_SCOPE: usize = 8;
/// Env: global running cap (≥ 1).
pub(crate) const MAX_RUNNING_GLOBAL_ENV: &str = "AGENTOS_INVOCATION_MAX_RUNNING_GLOBAL";
/// Env: per-tenant running cap (≥ 1).
pub(crate) const MAX_RUNNING_PER_TENANT_ENV: &str = "AGENTOS_INVOCATION_MAX_RUNNING_PER_TENANT";
/// Env: per-scope running cap (≥ 1).
pub(crate) const MAX_RUNNING_PER_SCOPE_ENV: &str = "AGENTOS_INVOCATION_MAX_RUNNING_PER_SCOPE";

/// Running-concurrency limits (distinct from `MAX_ACTIVE` non-terminal create cap).
///
/// A start needs room under all three caps. `per_tenant` stops one tenant from
/// filling the global cap by spreading invocations over many projects; the
/// effective cap for one scope is `min(per_scope, per_tenant, global)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvocationRunningLimits {
    pub global: usize,
    pub per_tenant: usize,
    pub per_scope: usize,
}

impl Default for InvocationRunningLimits {
    fn default() -> Self {
        Self {
            global: DEFAULT_MAX_RUNNING_GLOBAL,
            per_tenant: DEFAULT_MAX_RUNNING_PER_TENANT,
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
        if let Some(v) = positive(MAX_RUNNING_PER_TENANT_ENV) {
            limits.per_tenant = v;
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
    /// tenant_id → running count across all of that tenant's scopes.
    per_tenant: HashMap<String, usize>,
    per_scope: HashMap<RunningScope, usize>,
    /// id → scope it was admitted under. Release is keyed on this map, so a
    /// repeated or unknown release is a no-op and can never free a peer's slot.
    admitted: HashMap<String, RunningScope>,
}

impl FifoRunningSlots {
    pub(crate) fn new(limits: InvocationRunningLimits) -> Self {
        Self {
            limits,
            global: 0,
            per_tenant: HashMap::new(),
            per_scope: HashMap::new(),
            admitted: HashMap::new(),
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

    pub(crate) fn tenant_running(&self, tenant_id: &str) -> usize {
        self.per_tenant.get(tenant_id).copied().unwrap_or(0)
    }

    pub(crate) fn admitted_ids(&self) -> HashSet<String> {
        self.admitted.keys().cloned().collect()
    }

    /// Acquires one running slot when the global, per-tenant and per-scope
    /// caps all allow. Private: every slot must be owned by an admitted id so
    /// release can be matched one-to-one.
    fn try_acquire(&mut self, scope: &RunningScope) -> bool {
        if self.global >= self.limits.global
            || self.tenant_running(&scope.tenant_id) >= self.limits.per_tenant
            || self.scope_running(scope) >= self.limits.per_scope
        {
            return false;
        }
        self.global = self.global.saturating_add(1);
        *self.per_tenant.entry(scope.tenant_id.clone()).or_insert(0) += 1;
        *self.per_scope.entry(scope.clone()).or_insert(0) += 1;
        true
    }

    /// Atomically take a slot and mark `id` admitted (same lock as release).
    pub(crate) fn try_admit(&mut self, scope: &RunningScope, id: &str) -> bool {
        if self.admitted.contains_key(id) {
            return true;
        }
        if !self.try_acquire(scope) {
            return false;
        }
        self.admitted.insert(id.to_string(), scope.clone());
        true
    }

    /// Frees the slot owned by `id`. Idempotent: returns `false` (and changes
    /// nothing) when `id` holds no slot, e.g. on a second release. The scope
    /// decremented is the one recorded at admission, not a caller argument.
    pub(crate) fn release(&mut self, id: &str) -> bool {
        let Some(scope) = self.admitted.remove(id) else {
            return false;
        };
        self.global = self.global.saturating_sub(1);
        if let Some(count) = self.per_tenant.get_mut(&scope.tenant_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_tenant.remove(&scope.tenant_id);
            }
        }
        if let Some(count) = self.per_scope.get_mut(&scope) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_scope.remove(&scope);
            }
        }
        true
    }
}

/// Shared scheduler state: slots + wakeups when a slot frees or a peer is admitted.
#[derive(Debug)]
pub(crate) struct FifoScheduler {
    /// `std` mutex: critical sections are short and never await, which lets
    /// `release_now` run synchronously from `Drop` (no spawned task that a
    /// runtime teardown could drop, leaving the slot stuck).
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
        self.lock_slots().admitted_ids()
    }

    /// Take a slot + mark admitted under one lock; wakes peers so the next
    /// FIFO head (excluding admitted) can proceed under `per_scope > 1`.
    pub(crate) async fn try_admit(&self, scope: &RunningScope, id: &str) -> bool {
        let ok = self.lock_slots().try_admit(scope, id);
        if ok {
            self.notify.notify_waiters();
        }
        ok
    }

    /// Synchronous, idempotent release; safe to call from `Drop`. Wakes
    /// waiters only when a slot was actually freed.
    pub(crate) fn release_now(&self, id: &str) -> bool {
        let freed = self.lock_slots().release(id);
        if freed {
            self.notify.notify_waiters();
        }
        freed
    }

    /// Poison-tolerant lock: the ledger holds plain counters, so a panic in
    /// another holder cannot leave it logically torn.
    fn lock_slots(&self) -> MutexGuard<'_, FifoRunningSlots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn notify(&self) -> &Notify {
        &self.notify
    }

    #[allow(dead_code)]
    pub(crate) async fn snapshot(&self) -> (usize, InvocationRunningLimits) {
        let slots = self.lock_slots();
        (slots.global_running(), slots.limits())
    }

    #[cfg(test)]
    pub(crate) async fn scope_running_count(&self, scope: &RunningScope) -> usize {
        self.lock_slots().scope_running(scope)
    }

    #[cfg(test)]
    pub(crate) async fn tenant_running_count(&self, tenant_id: &str) -> usize {
        self.lock_slots().tenant_running(tenant_id)
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
            MAX_RUNNING_PER_TENANT_ENV => Some(" 5 ".into()),
            MAX_RUNNING_PER_SCOPE_ENV => Some("2".into()),
            _ => None,
        });
        assert_eq!(limits.global, 3);
        assert_eq!(limits.per_tenant, 5);
        assert_eq!(limits.per_scope, 2);
    }

    #[test]
    fn running_limits_default_and_invalid_per_tenant() {
        let defaults = InvocationRunningLimits::from_vars(|_| None);
        assert_eq!(defaults, InvocationRunningLimits::default());
        assert_eq!(defaults.per_tenant, DEFAULT_MAX_RUNNING_PER_TENANT);
        assert_eq!(DEFAULT_MAX_RUNNING_PER_TENANT, 16);
        for raw in ["0", "-1", "abc", ""] {
            let limits = InvocationRunningLimits::from_vars(|key| {
                (key == MAX_RUNNING_PER_TENANT_ENV).then(|| raw.to_string())
            });
            assert_eq!(
                limits.per_tenant, DEFAULT_MAX_RUNNING_PER_TENANT,
                "{raw:?} falls back to the default"
            );
        }
    }

    fn scope(tenant: &str, project: &str) -> RunningScope {
        RunningScope {
            tenant_id: tenant.into(),
            project_id: project.into(),
        }
    }

    #[test]
    fn per_tenant_cap_spans_projects_and_leaves_other_tenants_alone() {
        let mut slots = FifoRunningSlots::new(InvocationRunningLimits {
            global: 10,
            per_tenant: 2,
            per_scope: 2,
        });
        let a1 = scope("t1", "p1");
        let a2 = scope("t1", "p2");
        let a3 = scope("t1", "p3");
        let b1 = scope("t2", "p1");
        assert!(slots.try_admit(&a1, "a1"));
        assert!(slots.try_admit(&a2, "a2"));
        assert!(
            !slots.try_admit(&a3, "a3"),
            "third project of t1 is blocked by the per-tenant cap"
        );
        assert!(!slots.admitted_ids().contains("a3"));
        assert_eq!(slots.tenant_running("t1"), 2);
        assert!(slots.try_admit(&b1, "b1"), "another tenant is unaffected");
        assert!(slots.try_admit(&b1, "b2"));
        assert_eq!(slots.tenant_running("t2"), 2);
        assert_eq!(slots.global_running(), 4);

        assert!(slots.release("a1"));
        assert!(!slots.release("a1"), "double release frees nothing");
        assert_eq!(slots.tenant_running("t1"), 1);
        assert!(slots.try_admit(&a3, "a3"), "freed tenant slot is reusable");
        assert_eq!(slots.tenant_running("t1"), 2);

        for id in ["a2", "a3", "b1", "b2"] {
            assert!(slots.release(id));
        }
        assert_eq!(slots.global_running(), 0);
        assert_eq!(slots.tenant_running("t1"), 0);
        assert_eq!(slots.tenant_running("t2"), 0);
        assert!(slots.per_tenant.is_empty(), "zero counts are dropped");
    }

    #[test]
    fn effective_scope_cap_is_min_of_scope_and_tenant() {
        let mut slots = FifoRunningSlots::new(InvocationRunningLimits {
            global: 10,
            per_tenant: 1,
            per_scope: 4,
        });
        let a = scope("t1", "p1");
        assert!(slots.try_admit(&a, "a1"));
        assert!(!slots.try_admit(&a, "a2"), "per_tenant < per_scope wins");
        assert_eq!(slots.scope_running(&a), 1);
    }

    #[test]
    fn fifo_slots_respect_global_and_scope() {
        let mut slots = FifoRunningSlots::new(InvocationRunningLimits {
            global: 2,
            per_tenant: 2,
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
        assert!(slots.try_admit(&a, "a1"));
        assert!(!slots.try_admit(&a, "a2"), "per-scope cap");
        assert!(slots.try_admit(&b, "b1"), "other scope ok");
        assert!(!slots.try_admit(&b, "b2"), "global cap");
        assert!(slots.release("a1"));
        assert!(slots.try_admit(&a, "a2"), "slot freed");
    }

    #[test]
    fn fifo_release_is_idempotent_and_owner_keyed() {
        let mut slots = FifoRunningSlots::new(InvocationRunningLimits {
            global: 2,
            per_tenant: 2,
            per_scope: 2,
        });
        let a = RunningScope {
            tenant_id: "t1".into(),
            project_id: "p1".into(),
        };
        assert!(slots.try_admit(&a, "a1"));
        assert!(slots.try_admit(&a, "a2"));
        assert!(
            slots.try_admit(&a, "a1"),
            "re-admit of holder takes no slot"
        );
        assert_eq!(slots.global_running(), 2);
        assert!(slots.release("a1"));
        assert!(!slots.release("a1"), "double release is a no-op");
        assert!(!slots.release("never-admitted"), "unknown id is a no-op");
        assert_eq!(slots.global_running(), 1, "peer a2 keeps its slot");
        assert_eq!(slots.scope_running(&a), 1);
        assert!(slots.admitted_ids().contains("a2"));
        assert!(slots.release("a2"));
        assert_eq!(slots.global_running(), 0);
        assert_eq!(slots.scope_running(&a), 0);
    }

    #[test]
    fn scheduler_release_now_is_sync_and_idempotent() {
        // No Tokio runtime here: release must not depend on one.
        let scheduler = FifoScheduler::new(InvocationRunningLimits {
            global: 1,
            per_tenant: 1,
            per_scope: 1,
        });
        let a = RunningScope {
            tenant_id: "t1".into(),
            project_id: "p1".into(),
        };
        assert!(scheduler.lock_slots().try_admit(&a, "a1"));
        assert!(scheduler.release_now("a1"));
        assert!(!scheduler.release_now("a1"));
        assert_eq!(scheduler.lock_slots().global_running(), 0);
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
