//! Thin, outbound-only A2A integration.
//!
//! This module is deliberately a client adapter: it does not expose inbound
//! A2A routes, Agent Cards, task storage, or a second execution lifecycle.

pub mod outbound;
