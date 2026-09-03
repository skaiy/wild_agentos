use std::path::Path;
use std::sync::Arc;

use wild_agent_os_core::isolation::IsolationClaims;
use wild_agent_os_core::memory::l0_store::L0Store;

pub fn writable_l0(l0_root: impl AsRef<Path>) -> Arc<L0Store> {
    let claims =
        IsolationClaims::from_verified("test-tenant", "test-project", "test-actor").unwrap();
    Arc::new(L0Store::open_for_claims(l0_root, &claims).unwrap())
}
