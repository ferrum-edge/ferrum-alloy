//! Optional Ferrum Edge adapter for Alloy services.
//!
//! * [`contract`]: the Edge v0.9.7 headers, tokens, and span attributes this
//!   adapter uses, each traceable to Edge source.
//! * [`policy`]: gateway trust modes, verified consumer-identity handoff, and
//!   removal of unverified gateway-asserted headers.
//! * [`manifest`] and [`export`]: a PROPOSED service manifest and generation of
//!   reviewable Edge file-mode and GitForgeOps resources.
//!
//! The adapter depends on the telemetry crate (for transport trust) and the
//! diagnostics crate, never on the `ferrum-alloy` umbrella crate or on Edge's
//! implementation.

pub mod contract;
pub mod export;
pub mod manifest;
pub mod policy;
pub mod yaml;

pub use policy::{
    DeploymentMode, EdgeLayer, EdgePolicy, EdgePolicyConfig, EdgeService, EdgeStats,
    GatewayContext, GatewayVerification,
};
