//! Per-run accounting of the usage the LLM gateway reports (#337).
//!
//! A run gets its own [`RunUsageMeter`] and a gateway handle bound to it
//! ([`super::unified_gateway::UnifiedGateway::with_usage_meter`]). Every LLM
//! call made through that handle — planning calls, agent turns, streaming and
//! non-streaming — records what the upstream reported. Nothing is shared with
//! other runs, so concurrent runs never mix their counts.
//!
//! The meter never invents numbers: a call whose upstream reported no usage is
//! counted as such, and a call without a gateway-reported cost stays without
//! one. Callers decide what an incomplete snapshot means (the invocation
//! bridge fails closed).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// What one upstream LLM call reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallUsage {
    /// Model the upstream says served the call (falls back to the requested
    /// model when the response does not name one).
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cost reported by the upstream/gateway in USD (`usage.cost`), if any.
    pub reported_cost_usd: Option<f64>,
}

/// Token totals of one model within a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelTokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Aggregated usage of one run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunUsageSnapshot {
    /// Calls that completed with a 2xx response.
    pub calls: u64,
    /// Calls whose upstream reported no usage at all (tokens unknown).
    pub calls_without_usage: u64,
    /// Calls with usage but without a gateway-reported cost.
    pub calls_without_cost: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Sum of gateway-reported costs, in USD, over calls that reported one.
    pub reported_cost_usd: f64,
    pub per_model: BTreeMap<String, ModelTokens>,
    /// Model of the most recent call (tie-breaker for [`Self::primary_model`]).
    pub last_model: Option<String>,
}

impl RunUsageSnapshot {
    /// Tokens are complete only when at least one call happened and every
    /// call reported usage.
    pub fn tokens_complete(&self) -> bool {
        self.calls > 0 && self.calls_without_usage == 0
    }

    /// Every call reported a gateway cost.
    pub fn gateway_cost_complete(&self) -> bool {
        self.tokens_complete() && self.calls_without_cost == 0
    }

    /// The model that consumed the most tokens in this run; ties go to the
    /// most recently used model.
    pub fn primary_model(&self) -> Option<String> {
        let max = self
            .per_model
            .values()
            .map(|t| t.input_tokens + t.output_tokens)
            .max()?;
        let mut leaders = self
            .per_model
            .iter()
            .filter(|(_, t)| t.input_tokens + t.output_tokens == max)
            .map(|(m, _)| m.clone());
        let first = leaders.next()?;
        let rest: Vec<String> = leaders.collect();
        if rest.is_empty() {
            return Some(first);
        }
        match &self.last_model {
            Some(last) if *last == first || rest.contains(last) => Some(last.clone()),
            _ => Some(first),
        }
    }
}

/// Collects [`CallUsage`] for exactly one run.
#[derive(Debug, Default)]
pub struct RunUsageMeter {
    state: Mutex<RunUsageSnapshot>,
}

impl RunUsageMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a call that completed; `usage` is `None` when the upstream
    /// reported none.
    pub fn record(&self, requested_model: &str, usage: Option<CallUsage>) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.calls += 1;
        let Some(usage) = usage else {
            s.calls_without_usage += 1;
            return;
        };
        let model = if usage.model.trim().is_empty() {
            requested_model.to_string()
        } else {
            usage.model.clone()
        };
        s.input_tokens += usage.input_tokens;
        s.output_tokens += usage.output_tokens;
        match usage.reported_cost_usd {
            Some(cost) if cost.is_finite() && cost >= 0.0 => s.reported_cost_usd += cost,
            _ => s.calls_without_cost += 1,
        }
        let entry = s.per_model.entry(model.clone()).or_default();
        entry.input_tokens += usage.input_tokens;
        entry.output_tokens += usage.output_tokens;
        s.last_model = Some(model);
    }

    pub fn snapshot(&self) -> RunUsageSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(model: &str, input: u64, output: u64, cost: Option<f64>) -> Option<CallUsage> {
        Some(CallUsage {
            model: model.into(),
            input_tokens: input,
            output_tokens: output,
            reported_cost_usd: cost,
        })
    }

    #[test]
    fn sums_calls_and_tracks_missing_usage_and_cost() {
        let meter = RunUsageMeter::new();
        meter.record("m", call("m", 10, 2, Some(0.001)));
        meter.record("m", call("", 5, 1, None));
        let s = meter.snapshot();
        assert_eq!((s.calls, s.input_tokens, s.output_tokens), (2, 15, 3));
        assert!(s.tokens_complete());
        assert!(!s.gateway_cost_complete());
        meter.record("m", None);
        assert!(!meter.snapshot().tokens_complete());
    }

    #[test]
    fn primary_model_is_the_heaviest_then_the_latest() {
        let meter = RunUsageMeter::new();
        meter.record("a", call("a", 100, 0, None));
        meter.record("b", call("b", 10, 0, None));
        assert_eq!(meter.snapshot().primary_model().as_deref(), Some("a"));
        meter.record("b", call("b", 90, 0, None));
        // Tie at 100 tokens: the most recent model wins.
        assert_eq!(meter.snapshot().primary_model().as_deref(), Some("b"));
    }

    #[test]
    fn empty_run_has_no_complete_usage() {
        let s = RunUsageMeter::new().snapshot();
        assert!(!s.tokens_complete());
        assert_eq!(s.primary_model(), None);
    }
}
