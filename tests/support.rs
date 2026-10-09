use std::path::Path;
use std::sync::Arc;

use wild_agent_os_core::isolation::IsolationClaims;
use wild_agent_os_core::memory::l0_store::{L0Store, TenantL0Registry};

pub fn writable_l0(l0_root: impl AsRef<Path>) -> Arc<L0Store> {
    let claims =
        IsolationClaims::from_verified("test-tenant", "test-project", "test-actor").unwrap();
    TenantL0Registry::new(l0_root.as_ref())
        .get_or_open(&claims)
        .unwrap()
}
