//! In-memory record of the quads one run wrote into a claims graph.
//!
//! Graphify writes into `graph://{tenant}/{project}`, which other sources also
//! use. The run guard deletes the quads only this run recorded. A triple with
//! the same subject, predicate, and object that another live run also recorded
//! stays until that run ends. Another source's triples in the same graph stay.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::isolation::IsolationClaims;
use crate::knowledge_graph::types::RdfQuad;

pub(crate) struct GraphifyRunRecord {
    pub claims: IsolationClaims,
    pub quads: Vec<RdfQuad>,
}

#[derive(Default)]
pub(crate) struct GraphifyRunLedger {
    runs: Mutex<HashMap<String, GraphifyRunRecord>>,
}

impl GraphifyRunLedger {
    pub(crate) fn record(&self, run_id: &str, claims: &IsolationClaims, quads: Vec<RdfQuad>) {
        if run_id.is_empty() || quads.is_empty() {
            return;
        }
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get_mut(run_id) {
            Some(record) => record.quads.extend(quads),
            None => {
                runs.insert(
                    run_id.to_string(),
                    GraphifyRunRecord {
                        claims: claims.clone(),
                        quads,
                    },
                );
            }
        }
    }

    /// Remove `run_id`'s record and return the quads no other live run recorded.
    ///
    /// Deletion is by triple value. Two runs in the same project can graphify
    /// the same subject, predicate, and object; deleting that value when the
    /// first run ends would remove a triple the other run still uses. Those
    /// shared quads are left out of the returned record. This run's marker is
    /// unique, so it is always included.
    pub(crate) fn take_exclusive(&self, run_id: &str) -> Option<GraphifyRunRecord> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = runs.remove(run_id)?;
        record.quads.retain(|quad| {
            !runs
                .values()
                .any(|other| other.quads.iter().any(|kept| same_quad(kept, quad)))
        });
        Some(record)
    }
}

fn same_quad(left: &RdfQuad, right: &RdfQuad) -> bool {
    left.subject == right.subject
        && left.predicate == right.predicate
        && left.object == right.object
        && left.graph == right.graph
}
