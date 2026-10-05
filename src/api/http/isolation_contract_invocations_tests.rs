//! Isolation / integrator contract tests for `/v1/invocations` (#318).
//!
//! This file collects the route-level assertions that downstream integrators
//! rely on. Unit coverage for the store and the individual handlers lives in
//! `invocations_store_tests` / `invocations_tests`; do not duplicate those
//! here — add only end-to-end contract checks (or leave a named `#[ignore]`
//! stub with the mapping from docs/29).
//!
//! Mapping (docs/29 §9 ↔ test name) — filled as #314/#315/#317 land:
//!
//! | Contract | Status | Test |
//! | --- | --- | --- |
//! | Anonymous 401 on all 5 routes × STRICT on/off | partial (#317 events) | `invocations_routes_reject_unverified_callers_with_401` in `invocations_tests` |
//! | Cross-tenant / cross-project / missing id → byte-identical 404 | partial | `cross_scope_get_and_cancel_are_byte_identical_404`, `events_cross_scope_is_byte_identical_404` |
//! | Switch off → 503 `execution_disabled`, nothing persisted | done (#314) | `create_is_503_execution_disabled_by_default_and_persists_nothing` |
//! | Idempotent replay / conflict / concurrent create | TODO (#315) | — |
//! | Illegal transition / If-Match | partial (#316/#314) | `cancel_rules_owner_da_if_match_and_lifecycle` |
//! | SSE snapshot + terminal close | partial (#317) | `events_route_emits_snapshot_and_closes_on_terminal` |
//! | VAL-016 / VAL-017 usage on succeeded | partial (#317 helper) | `invocations_execution::tests::*` |

#![allow(dead_code)]

// Intentionally no executable tests yet in this file: the named coverage above
// already lives next to the handlers. Add new integrator-facing cases here as
// #315 / the full #317 bridge land, and keep the mapping table in sync.
