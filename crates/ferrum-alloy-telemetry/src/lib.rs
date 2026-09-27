//! Independently usable request instrumentation for Tower and Axum services.
//!
//! This crate does not depend on the rest of Ferrum Alloy. An existing Axum
//! application can add [`TelemetryLayer`] and [`RecordRouteLayer`] to its own
//! router without adopting `AlloyApp`, and keeps its own runtime, subscriber,
//! and middleware.
//!
//! Nothing here installs a global subscriber or OpenTelemetry provider as a
//! side effect. Initialization helpers ([`init`]) are explicit calls that fail
//! instead of silently replacing an existing subscriber.

pub mod body;
pub mod cache;
pub mod context;
pub mod layer;
pub mod metrics;
pub mod operation;
pub mod peer;
pub mod request_id;
pub mod route;
pub mod trace_context;

mod otel_bridge;

pub use otel_bridge::exported_ids;

#[cfg(feature = "subscriber")]
pub mod init;

#[cfg(feature = "otel")]
pub mod otel;

pub use context::{RequestContext, TraceDecision};
pub use layer::{
    AcceptPolicy, BodyOutcome, RecordConfig, RequestIdConfig, ServerTimingPolicy, TelemetryConfig,
    TelemetryConfigError, TelemetryLayer, TelemetryService, TraceContextConfig,
};
pub use metrics::Metrics;
pub use peer::{PeerInfo, PeerTrust, TlsPeer, TrustClassifier, TrustedPeers, TrustedPeersConfig};
pub use request_id::RequestId;
pub use route::{RecordRouteLayer, RouteSlot};
