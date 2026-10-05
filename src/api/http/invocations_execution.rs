//! Invocation execution bridge (#317): cancellation registry, usage contract
//! for `succeeded`, VAL-PROJ-CTX fail-closed, and the real `TaskExecutor` drive.
//!
//! # Isolation / projection context (H4 / #317 acceptance)
//!
//! An invocation-driven run MUST receive a non-empty in-scope projection
//! context. If isolation claims are missing on the execution path, or
//! projection returns empty because claims were dropped, the task MUST fail
//! closed (terminal `failed` with `error.code = "projection_context_missing"`)
//! — it MUST NOT warn-and-succeed with empty context. Legacy agent_runner /
//! SA / scheduler paths that only `warn!` on missing claims are out of scope
//! for the invocations bridge; do not copy that behaviour here.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::invocations::InvocationDispatcher;
use super::invocations_enforcement::{
    budget_is_exceeded, deadline_already_due, duration_until_deadline,
    fetch_and_verify_input_ref, FifoScheduler, InputRefRegistry, RunningScope,
    BUDGET_EXCEEDED_ERROR_CODE,
};
use super::invocations_store::{
    scrub_secret_shaped_text, Invocation, InvocationErrorInfo, InvocationResult, InvocationState,
    InvocationStore, InvocationUsage, TransitionPatch, DEADLINE_EXCEEDED_ERROR_CODE,
    PROJECTION_CONTEXT_MISSING_ERROR_CODE, TASK_INIT_FAILED_ERROR_CODE,
};
use super::{TaskExecSpec, TaskExecutor};
use crate::core::core_types::SemanticCore;
use crate::core::event_bus::EventBus;
use crate::isolation::IsolationClaims;

/// Incomplete `result.usage` when closing as `succeeded` (VAL-016 / VAL-017).
pub(crate) const INCOMPLETE_USAGE_ERROR_CODE: &str = "incomplete_usage";
/// Executor join failed with a panic / unexpected abort.
pub(crate) const EXECUTOR_PANIC_ERROR_CODE: &str = "executor_panic";
/// Executor finished (or was abandoned) without a TASK_COMPLETED / TASK_FAILED event.
pub(crate) const TERMINAL_EVENT_MISSING_ERROR_CODE: &str = "terminal_event_missing";
/// Soft upper bound for persisted result summaries (#317 scrub).
pub(crate) const MAX_RESULT_SUMMARY_CHARS: usize = 4_096;
/// Scoped frame used for the H4 projection gate (non-SPARQL, task-local).
pub(crate) const PROJECTION_GATE_FRAME: &str = "reference_only";

/// In-flight cancellation tokens keyed by invocation id only (no scope dim).
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

    #[cfg(test)]
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.tokens.contains_key(id)
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

/// Truncate + lightly scrub a result summary before persisting.
pub(crate) fn scrub_result_summary(summary: &str) -> String {
    let truncated: String = summary.chars().take(MAX_RESULT_SUMMARY_CHARS).collect();
    scrub_secret_shaped_text(&truncated)
}

/// Whether a projection JSON string counts as non-empty in-scope context (H4).
pub(crate) fn projection_context_is_nonempty(projected: &str) -> bool {
    let trimmed = projected.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed == "null" {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        // Non-JSON but non-empty text still counts as context.
        return true;
    };
    match value {
        Value::Null => false,
        Value::Object(map) => {
            if map.is_empty() {
                return false;
            }
            // Prefer artifacts: empty array means no in-scope nodes projected.
            if let Some(Value::Array(artifacts)) = map.get("artifacts") {
                return !artifacts.is_empty();
            }
            // Otherwise any object with an @id / task_iri / status is enough.
            map.contains_key("@id")
                || map.contains_key("task_iri")
                || map.contains_key("status")
                || map.len() > 1
        }
        Value::Array(items) => !items.is_empty(),
        Value::String(s) => !s.trim().is_empty(),
        _ => true,
    }
}

/// Gate that checks scoped projection context before the executor runs (H4).
#[async_trait]
pub(crate) trait ProjectionContextGate: Send + Sync {
    async fn ensure_nonempty(
        &self,
        core: &SemanticCore,
        task_iri: &str,
        claims: &IsolationClaims,
    ) -> Result<(), String>;
}

/// Production gate: scoped `reference_only` projection must yield artifacts.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ScopedProjectionGate;

#[async_trait]
impl ProjectionContextGate for ScopedProjectionGate {
    async fn ensure_nonempty(
        &self,
        core: &SemanticCore,
        task_iri: &str,
        claims: &IsolationClaims,
    ) -> Result<(), String> {
        match core
            .projection
            .project(task_iri, PROJECTION_GATE_FRAME, HashMap::new(), claims)
            .await
        {
            Ok(projected) if projection_context_is_nonempty(&projected) => Ok(()),
            Ok(_) => Err("scoped projection returned empty context".into()),
            Err(error) => Err(format!("scoped projection failed: {error}")),
        }
    }
}

/// Instantiates [`IsolationClaims`] from a persisted invocation's scope fields.
pub(crate) fn claims_from_invocation(invocation: &Invocation) -> Result<IsolationClaims, String> {
    IsolationClaims::from_verified(
        invocation.tenant_id.as_str(),
        invocation.project_id.as_str(),
        invocation.actor_id.as_str(),
    )
    .map_err(|e| format!("invalid persisted claims: {e}"))
}

/// Prompt text used for `init_task_with_claims` / `TaskExecSpec`.
pub(crate) fn prompt_from_invocation(invocation: &Invocation) -> String {
    if let Some(prompt) = invocation
        .request
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return prompt.to_string();
    }
    if let Some(input) = invocation.request.input.as_ref() {
        return input.to_string();
    }
    String::new()
}

/// Parses usage (+ summary) out of a TASK_* event payload.
pub(crate) fn parse_terminal_payload(payload: &str) -> (String, Option<InvocationUsage>) {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return (scrub_result_summary(payload), None);
    };
    let summary = value
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let usage = value
        .get("usage")
        .cloned()
        .and_then(|u| serde_json::from_value::<InvocationUsage>(u).ok());
    (scrub_result_summary(&summary), usage)
}

/// Production dispatcher: FIFO running caps → input_ref → init task →
/// projection gate → TaskExecutor → state drive (#317 / #331).
pub(crate) struct InvocationExecutionBridge {
    store: Arc<InvocationStore>,
    cancellations: InvocationCancellationRegistry,
    core: Arc<SemanticCore>,
    executor: Arc<dyn TaskExecutor>,
    events: Arc<EventBus>,
    shutdown: CancellationToken,
    projection_gate: Arc<dyn ProjectionContextGate>,
    scheduler: Arc<FifoScheduler>,
    input_refs: InputRefRegistry,
}

impl InvocationExecutionBridge {
    pub(crate) fn new(
        store: Arc<InvocationStore>,
        cancellations: InvocationCancellationRegistry,
        core: Arc<SemanticCore>,
        executor: Arc<dyn TaskExecutor>,
        shutdown: CancellationToken,
        projection_gate: Arc<dyn ProjectionContextGate>,
    ) -> Self {
        Self::new_with_enforcement(
            store,
            cancellations,
            core,
            executor,
            shutdown,
            projection_gate,
            Arc::new(FifoScheduler::with_defaults()),
            InputRefRegistry::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_enforcement(
        store: Arc<InvocationStore>,
        cancellations: InvocationCancellationRegistry,
        core: Arc<SemanticCore>,
        executor: Arc<dyn TaskExecutor>,
        shutdown: CancellationToken,
        projection_gate: Arc<dyn ProjectionContextGate>,
        scheduler: Arc<FifoScheduler>,
        input_refs: InputRefRegistry,
    ) -> Self {
        let events = core.events.clone();
        Self {
            store,
            cancellations,
            core,
            executor,
            events,
            shutdown,
            projection_gate,
            scheduler,
            input_refs,
        }
    }

    /// Convenience constructor with the default scoped projection gate.
    pub(crate) fn with_default_gate(
        store: Arc<InvocationStore>,
        cancellations: InvocationCancellationRegistry,
        core: Arc<SemanticCore>,
        executor: Arc<dyn TaskExecutor>,
        shutdown: CancellationToken,
    ) -> Self {
        Self::new(
            store,
            cancellations,
            core,
            executor,
            shutdown,
            Arc::new(ScopedProjectionGate),
        )
    }

    pub(crate) fn scheduler(&self) -> Arc<FifoScheduler> {
        self.scheduler.clone()
    }

    pub(crate) fn input_refs(&self) -> &InputRefRegistry {
        &self.input_refs
    }
}

impl InvocationDispatcher for InvocationExecutionBridge {
    fn dispatch(&self, invocation: &Invocation) {
        let token = self.cancellations.register(&invocation.id);
        let store = self.store.clone();
        let cancellations = self.cancellations.clone();
        let core = self.core.clone();
        let executor = self.executor.clone();
        let events = self.events.clone();
        let shutdown = self.shutdown.clone();
        let gate = self.projection_gate.clone();
        let scheduler = self.scheduler.clone();
        let input_refs = self.input_refs.clone();
        let invocation = invocation.clone();
        let id = invocation.id.clone();
        let scope = RunningScope::from_invocation(&invocation);

        tokio::spawn(async move {
            // Deadline while queued/running: cancel token; run path maps to
            // failed/deadline_exceeded (not user-cancelled).
            if invocation.request.deadline.is_some() {
                let store_dl = store.clone();
                let inv_dl = invocation.clone();
                let token_dl = token.clone();
                let shutdown_dl = shutdown.clone();
                let scheduler_dl = scheduler.clone();
                tokio::spawn(async move {
                    watch_deadline(store_dl, inv_dl, token_dl, shutdown_dl, scheduler_dl).await;
                });
            }

            let run = admit_and_run(
                store,
                core,
                executor,
                events,
                gate,
                scheduler,
                input_refs,
                invocation,
                token.clone(),
                scope,
            );
            tokio::pin!(run);
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    token.cancel();
                    run.await;
                }
                _ = &mut run => {}
            }
            cancellations.remove(&id);
        });
    }
}

/// Waits for a running slot (per-scope FIFO by `created_at`), then executes;
/// releases the slot and wakes waiters when done.
#[allow(clippy::too_many_arguments)]
async fn admit_and_run(
    store: Arc<InvocationStore>,
    core: Arc<SemanticCore>,
    executor: Arc<dyn TaskExecutor>,
    events: Arc<EventBus>,
    gate: Arc<dyn ProjectionContextGate>,
    scheduler: Arc<FifoScheduler>,
    input_refs: InputRefRegistry,
    invocation: Invocation,
    cancellation: CancellationToken,
    scope: RunningScope,
) {
    loop {
        let claims = match claims_from_invocation(&invocation) {
            Ok(c) => c,
            Err(_) => return,
        };
        let Ok(current) = store.get_for_claims(&claims, &invocation.id).await else {
            return;
        };
        if current.state != InvocationState::Queued {
            return;
        }

        if cancellation.is_cancelled() {
            // Deadline watcher writes failed; user cancel route writes cancelled.
            // Only backstop a due deadline that left us still queued.
            if invocation
                .request
                .deadline
                .as_deref()
                .is_some_and(deadline_already_due)
            {
                let _ = fail_pre_execution(
                    &store,
                    &invocation,
                    DEADLINE_EXCEEDED_ERROR_CODE,
                    "deadline exceeded while queued",
                )
                .await;
            }
            return;
        }

        // Per-scope FIFO: only the oldest queued in this scope may acquire.
        let oldest = store
            .oldest_queued_in_scope(&scope.tenant_id, &scope.project_id)
            .await;
        if oldest.as_ref().map(|inv| inv.id.as_str()) != Some(invocation.id.as_str()) {
            tokio::select! {
                _ = scheduler.notify().notified() => {}
                _ = cancellation.cancelled() => {}
            }
            continue;
        }

        if scheduler.try_acquire(&scope).await {
            break;
        }

        tokio::select! {
            _ = scheduler.notify().notified() => {}
            _ = cancellation.cancelled() => {}
        }
    }

    run_invocation(
        store,
        core,
        executor,
        events,
        gate,
        input_refs,
        invocation,
        cancellation,
    )
    .await;

    scheduler.release(&scope).await;
}

async fn watch_deadline(
    store: Arc<InvocationStore>,
    invocation: Invocation,
    token: CancellationToken,
    shutdown: CancellationToken,
    scheduler: Arc<FifoScheduler>,
) {
    let Some(raw) = invocation.request.deadline.clone() else {
        return;
    };
    let wait = duration_until_deadline(&raw).unwrap_or(std::time::Duration::ZERO);
    tokio::select! {
        _ = tokio::time::sleep(wait) => {}
        _ = shutdown.cancelled() => return,
        _ = token.cancelled() => return,
    }

    let Ok(claims) = claims_from_invocation(&invocation) else {
        return;
    };
    let Ok(current) = store.get_for_claims(&claims, &invocation.id).await else {
        return;
    };
    if current.state.is_terminal() {
        return;
    }

    // Signal the executor / admit loop first, then persist failed.
    token.cancel();
    let _ = fail_pre_execution(
        &store,
        &invocation,
        DEADLINE_EXCEEDED_ERROR_CODE,
        "deadline exceeded",
    )
    .await;
    // Wake FIFO waiters (a queued peer may now be eligible; this id is terminal).
    scheduler.notify().notify_waiters();
}

async fn run_invocation(
    store: Arc<InvocationStore>,
    core: Arc<SemanticCore>,
    executor: Arc<dyn TaskExecutor>,
    events: Arc<EventBus>,
    gate: Arc<dyn ProjectionContextGate>,
    input_refs: InputRefRegistry,
    invocation: Invocation,
    cancellation: CancellationToken,
) {
    if cancellation.is_cancelled() {
        // Cancel / deadline raced ahead.
        return;
    }

    // Resolve input_ref before task init (digest mismatch → queued→failed).
    if let Some(input_ref) = invocation.request.input_ref.as_ref() {
        match fetch_and_verify_input_ref(&input_refs, &input_ref.uri, &input_ref.sha256).await {
            Ok(_bytes) => {}
            Err((code, message)) => {
                let _ = fail_pre_execution(&store, &invocation, code, &message).await;
                return;
            }
        }
    }

    let claims = match claims_from_invocation(&invocation) {
        Ok(claims) => claims,
        Err(message) => {
            let _ = fail_pre_execution(
                &store,
                &invocation,
                PROJECTION_CONTEXT_MISSING_ERROR_CODE,
                &message,
            )
            .await;
            return;
        }
    };

    let prompt = prompt_from_invocation(&invocation);
    let task_iri = match core
        .init_task_with_claims(&prompt, None, None, None, None, &claims)
        .await
    {
        Ok(task_iri) => task_iri,
        Err(error) => {
            let _ = fail_pre_execution(
                &store,
                &invocation,
                TASK_INIT_FAILED_ERROR_CODE,
                &format!("failed to init task: {error}"),
            )
            .await;
            return;
        }
    };

    if let Err(message) = gate.ensure_nonempty(&core, &task_iri, &claims).await {
        let _ = fail_pre_execution(
            &store,
            &invocation,
            PROJECTION_CONTEXT_MISSING_ERROR_CODE,
            &message,
        )
        .await;
        return;
    }

    if cancellation.is_cancelled() {
        return;
    }

    // queued → running (+ bind task_iri)
    if let Err(error) = store
        .transition_for_claims(
            &claims,
            &invocation.id,
            None,
            InvocationState::Running,
            TransitionPatch {
                task_iri: Some(task_iri.clone()),
                ..TransitionPatch::default()
            },
        )
        .await
    {
        tracing::warn!(
            invocation_id = %invocation.id,
            error = %error,
            "invocation could not enter running"
        );
        return;
    }

    // Subscribe before spawning the executor so early events are not missed.
    let mut rx = events.subscribe();

    let spec = TaskExecSpec {
        prompt,
        task_iri: task_iri.clone(),
        include_thought: true,
        include_tool_calls: true,
        cancellation: cancellation.clone(),
        isolation_claims: claims.clone(),
    };

    let exec_task_iri = task_iri.clone();
    let exec_events = events.clone();
    let mut execution = tokio::spawn(async move {
        executor.execute(spec).await;
    });

    let mut terminal_event = None;
    let mut executor_done = false;

    while terminal_event.is_none() {
        tokio::select! {
            biased;
            _ = cancellation.cancelled(), if !executor_done => {
                // Requested cancel: wait for the executor to unwind, then
                // prefer a real terminal event if one arrived; else cancelled.
                let join = (&mut execution).await;
                if let Ok(event) = rx.try_recv() {
                    if event.task_iri == task_iri
                        && (event.event_type == "TASK_COMPLETED"
                            || event.event_type == "TASK_FAILED")
                    {
                        terminal_event = Some(event);
                        break;
                    }
                }
                // Drain a few more events briefly.
                for _ in 0..8 {
                    match rx.try_recv() {
                        Ok(event)
                            if event.task_iri == task_iri
                                && (event.event_type == "TASK_COMPLETED"
                                    || event.event_type == "TASK_FAILED") =>
                        {
                            terminal_event = Some(event);
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                if terminal_event.is_some() {
                    break;
                }
                if let Err(error) = join {
                    if error.is_panic() {
                        let _ = fail_running(
                            &store,
                            &claims,
                            &invocation.id,
                            EXECUTOR_PANIC_ERROR_CODE,
                            &format!("task executor panicked: {error}"),
                            None,
                        )
                        .await;
                        return;
                    }
                }
                // Deadline watcher may already have written failed/deadline_exceeded.
                if let Ok(current) = store.get_for_claims(&claims, &invocation.id).await {
                    if current.state.is_terminal() {
                        return;
                    }
                    if current
                        .error
                        .as_ref()
                        .is_some_and(|e| e.code == DEADLINE_EXCEEDED_ERROR_CODE)
                    {
                        return;
                    }
                }
                let _ = store
                    .transition_for_claims(
                        &claims,
                        &invocation.id,
                        None,
                        InvocationState::Cancelled,
                        TransitionPatch::default(),
                    )
                    .await;
                return;
            }
            join = &mut execution, if !executor_done => {
                executor_done = true;
                match join {
                    Ok(()) => {}
                    Err(error) => {
                        let _ = exec_events
                            .emit(
                                &exec_task_iri,
                                "TASK_FAILED",
                                "invocation",
                                &serde_json::json!({
                                    "status": "failed",
                                    "summary": format!(
                                        "task executor terminated unexpectedly: {error}"
                                    ),
                                })
                                .to_string(),
                            )
                            .await;
                    }
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if event.task_iri != task_iri {
                            continue;
                        }
                        if event.event_type == "TASK_COMPLETED"
                            || event.event_type == "TASK_FAILED"
                        {
                            terminal_event = Some(event);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                }
            }
        }

        // Executor finished but terminal event not yet received: keep
        // receiving for a short window, then fail closed.
        if executor_done && terminal_event.is_none() {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(event))
                    if event.task_iri == task_iri
                        && (event.event_type == "TASK_COMPLETED"
                            || event.event_type == "TASK_FAILED") =>
                {
                    terminal_event = Some(event);
                }
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }

    let Some(event) = terminal_event else {
        let current = store.get_for_claims(&claims, &invocation.id).await.ok();
        if current.as_ref().is_some_and(|inv| inv.state.is_terminal()) {
            return;
        }
        let _ = fail_running(
            &store,
            &claims,
            &invocation.id,
            TERMINAL_EVENT_MISSING_ERROR_CODE,
            "execution ended without a terminal task event",
            None,
        )
        .await;
        return;
    };

    apply_terminal_event(&store, &claims, &invocation, &event).await;
}

async fn apply_terminal_event(
    store: &InvocationStore,
    claims: &IsolationClaims,
    invocation: &Invocation,
    event: &crate::core::event_bus::Event,
) {
    let id = invocation.id.as_str();
    let (summary, usage) = parse_terminal_payload(&event.payload);
    if event.event_type == "TASK_COMPLETED" {
        // Budget gate before VAL-016 succeeded write (#331).
        if let (Some(budget), Some(usage_ref)) = (invocation.request.budget.as_ref(), usage.as_ref())
        {
            if budget_is_exceeded(budget, usage_ref) {
                let _ = fail_running(
                    store,
                    claims,
                    id,
                    BUDGET_EXCEEDED_ERROR_CODE,
                    "request.budget limit exceeded",
                    usage.clone(),
                )
                .await;
                return;
            }
        }
        let result = InvocationResult {
            summary,
            artifacts: vec![],
            usage,
        };
        match succeeded_patch_or_reject(result) {
            Ok(result) => {
                let _ = store
                    .transition_for_claims(
                        claims,
                        id,
                        None,
                        InvocationState::Succeeded,
                        TransitionPatch {
                            result: Some(result),
                            ..TransitionPatch::default()
                        },
                    )
                    .await;
            }
            Err(incomplete) => {
                let _ = fail_running(
                    store,
                    claims,
                    id,
                    incomplete.as_error_code(),
                    incomplete.as_message(),
                    None,
                )
                .await;
            }
        }
    } else {
        let _ = fail_running(
            store,
            claims,
            id,
            "task_failed",
            if summary.is_empty() {
                "task failed"
            } else {
                &summary
            },
            usage,
        )
        .await;
    }
}

async fn fail_pre_execution(
    store: &InvocationStore,
    invocation: &Invocation,
    code: &str,
    message: &str,
) -> Result<(), super::invocations_store::InvocationStoreError> {
    let claims = claims_from_invocation(invocation)
        .map_err(super::invocations_store::InvocationStoreError::Persistence)?;
    // Prefer queued → failed when still queued; otherwise running → failed.
    let current = store.get_for_claims(&claims, &invocation.id).await?;
    let next = InvocationState::Failed;
    if current.state.is_terminal() {
        return Ok(());
    }
    store
        .transition_for_claims(
            &claims,
            &invocation.id,
            None,
            next,
            TransitionPatch {
                error: Some(InvocationErrorInfo::new(code, message)),
                ..TransitionPatch::default()
            },
        )
        .await
        .map(|_| ())
}

async fn fail_running(
    store: &InvocationStore,
    claims: &IsolationClaims,
    id: &str,
    code: &str,
    message: &str,
    usage: Option<InvocationUsage>,
) -> Result<(), super::invocations_store::InvocationStoreError> {
    let current = store.get_for_claims(claims, id).await?;
    if current.state.is_terminal() {
        return Ok(());
    }
    // If still queued somehow, the permits_with path handles approved codes.
    let result = usage.map(|usage| InvocationResult {
        summary: String::new(),
        artifacts: vec![],
        usage: Some(usage),
    });
    store
        .transition_for_claims(
            claims,
            id,
            None,
            InvocationState::Failed,
            TransitionPatch {
                error: Some(InvocationErrorInfo::new(code, message)),
                result,
                ..TransitionPatch::default()
            },
        )
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::http::invocations_store::{InvocationErrorInfo, InvocationToolCallUsage};

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

    #[test]
    fn projection_context_rejects_empty_shapes() {
        assert!(!projection_context_is_nonempty(""));
        assert!(!projection_context_is_nonempty("{}"));
        assert!(!projection_context_is_nonempty(
            r#"{"task_iri":"iri://t","frame":"reference_only","artifacts":[]}"#
        ));
        assert!(projection_context_is_nonempty(
            r#"{"task_iri":"iri://t","frame":"reference_only","artifacts":[{"@id":"iri://t"}]}"#
        ));
    }

    #[test]
    fn error_message_is_scrubbed() {
        let err = InvocationErrorInfo::new("x", "leak api_key=secret here");
        assert!(err.message.contains("[redacted]"));
        assert!(!err.message.to_ascii_lowercase().contains("api_key"));
    }

    #[test]
    fn scrub_result_summary_truncates_and_redacts() {
        let long = "x".repeat(MAX_RESULT_SUMMARY_CHARS + 50);
        assert_eq!(
            scrub_result_summary(&long).chars().count(),
            MAX_RESULT_SUMMARY_CHARS
        );
        assert!(scrub_result_summary("leak api_key=secret").contains("[redacted]"));
    }

    #[test]
    fn parse_terminal_payload_reads_usage() {
        let payload = serde_json::json!({
            "status": "succeeded",
            "summary": "ok",
            "usage": {
                "model": "m",
                "input_tokens": 1,
                "output_tokens": 2,
                "cost": 3
            }
        })
        .to_string();
        let (summary, usage) = parse_terminal_payload(&payload);
        assert_eq!(summary, "ok");
        let usage = usage.expect("usage");
        assert_eq!(usage.model.as_deref(), Some("m"));
        assert_eq!(usage.cost, Some(3));
    }
}
