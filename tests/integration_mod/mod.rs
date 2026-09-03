// Integration tests for Agent OS core components
use std::path::Path;
use std::sync::Arc;

use wild_agent_os_core::isolation::IsolationClaims;
use wild_agent_os_core::memory::l0_store::L0Store;

fn writable_l0(l0_root: impl AsRef<Path>) -> Arc<L0Store> {
    let claims =
        IsolationClaims::from_verified("test-tenant", "test-project", "test-actor").unwrap();
    Arc::new(L0Store::open_for_claims(l0_root, &claims).unwrap())
}

mod test_comprehensive;
mod test_deepseek;
mod test_e2e;
mod test_e2e_autonomous;
mod test_e2e_programming;
mod test_e2e_research;
mod test_gateway;
mod test_jsonld;
mod test_memory;
mod test_result_router;
mod test_sa;
mod test_skill_graph;
