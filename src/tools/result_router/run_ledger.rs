//! In-memory record of the quads one run wrote into a claims graph.
//!
//! Graphify writes into `graph://{tenant}/{project}`, which other sources also
//! use. The run guard deletes exactly the quads recorded here when the run
//! ends, so another source's triples in the same graph stay.

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

    pub(crate) fn take(&self, run_id: &str) -> Option<GraphifyRunRecord> {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id)
    }
}
