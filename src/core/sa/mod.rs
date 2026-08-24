mod agent;
mod execution;
mod intervention;
mod planning;
mod process;
mod stats;
mod types;

// Re-export all types so existing callers' imports continue to work
pub use agent::SupervisorAgent;
pub use types::*;

use std::sync::atomic::{AtomicU8, Ordering};

/// Last verify-first gate decision for Admin read-only metrics.
/// 0 = idle, 1 = already_done, 2 = needs_execution
static LAST_VERIFY_GATE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn record_verify_gate(needs_execution: bool) {
    LAST_VERIFY_GATE.store(if needs_execution { 2 } else { 1 }, Ordering::Relaxed);
}

/// Cheap Admin snapshot of the verify-first gate.
pub fn verify_first_runtime_snapshot() -> serde_json::Value {
    let last = match LAST_VERIFY_GATE.load(Ordering::Relaxed) {
        1 => "already_done",
        2 => "needs_execution",
        _ => "idle",
    };
    serde_json::json!({
        "enabled": true,
        "gate": "verify_aa_needs_execution",
        "last_gate": last,
    })
}

// Action handler registry (already exists, no changes needed)
mod actions;

#[cfg(test)]
mod tests;
