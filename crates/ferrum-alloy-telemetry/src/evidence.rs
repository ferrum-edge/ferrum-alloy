//! Hand-off of finished-request evidence to an application-owned store.
//!
//! With [`TelemetryLayer::with_evidence_sink`](crate::TelemetryLayer::with_evidence_sink),
//! the layer passes one [`RequestEvidence`] per instrumented request to an
//! [`EvidenceSink`] when the request finalizes: at body completion, error,
//! or drop, exactly once. The evidence holds only values the layer already
//! measures and labels. It never holds header values, the raw path or query,
//! bodies, peer addresses, or certificate identities.
//!
//! A request is attributed to a tenant only when the application says so
//! through the request's [`TenantTag`]. The layer inserts the tag into
//! request extensions only while a sink is installed.
//!
//! ```
//! use ferrum_alloy_telemetry::evidence::TenantTag;
//!
//! // In a handler or middleware that has authenticated the caller:
//! let tag = TenantTag::default();
//! assert!(tag.set("acme"));
//! assert!(!tag.set("other"), "the first tag wins");
//! assert_eq!(tag.get(), Some("acme"));
//! ```

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::context::TraceDecision;
use crate::layer::BodyOutcome;
use crate::peer::PeerTrust;
use crate::request_id::{RequestId, RequestIdSource};
use crate::trace_context::{SpanId, TraceId};

/// Maximum length of a tenant tag in bytes.
pub const MAX_TENANT_BYTES: usize = 128;

/// Returns `true` when `tenant` is a valid tenant tag: 1 to
/// [`MAX_TENANT_BYTES`] bytes of `[A-Za-z0-9._:@-]`.
pub fn valid_tenant(tenant: &str) -> bool {
    !tenant.is_empty()
        && tenant.len() <= MAX_TENANT_BYTES
        && tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

/// The tenant a request belongs to, set by application code once it has
/// authenticated the caller.
///
/// Clones share one slot. The first valid [`TenantTag::set`] wins, so code
/// that runs later cannot move a request to another tenant.
#[derive(Clone, Default)]
pub struct TenantTag(Arc<OnceLock<Arc<str>>>);

impl TenantTag {
    /// Attributes the request to `tenant`. Returns `false`, and changes
    /// nothing, when `tenant` is not a valid tag (see [`valid_tenant`]) or
    /// the request already has a tenant.
    pub fn set(&self, tenant: &str) -> bool {
        valid_tenant(tenant) && self.0.set(Arc::from(tenant)).is_ok()
    }

    /// The tenant, once set.
    pub fn get(&self) -> Option<&str> {
        self.0.get().map(|tenant| tenant.as_ref())
    }

    pub(crate) fn shared(&self) -> Option<Arc<str>> {
        self.0.get().cloned()
    }
}

impl fmt::Debug for TenantTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TenantTag").field(&self.get()).finish()
    }
}

/// Reads the request's tag. Without an evidence sink the layer inserts none,
/// and the extractor returns a detached tag whose value is never read.
#[cfg(feature = "axum")]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for TenantTag {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let tag = parts.extensions.get::<TenantTag>().cloned();
        Ok(tag.unwrap_or_default())
    }
}

/// Who chose a request's id, from most to least trustworthy.
///
/// An evidence store keeps the records of each origin apart, so an id a
/// caller chose can neither join nor evict the records of a request whose id
/// this process generated or a trusted peer supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RequestIdOrigin {
    /// This process generated it, so no other request shares it. Includes
    /// ids that replaced an invalid or untrusted incoming one.
    Generated,
    /// A peer the trust classifier trusts supplied it, such as Ferrum Edge.
    /// The peer may have kept a value its own client chose.
    TrustedPeer,
    /// A caller the trust classifier does not trust supplied it, which only
    /// `request_id.accept_incoming = any` allows.
    UntrustedCaller,
}

impl RequestIdOrigin {
    /// The origin of an id chosen as `source` for a request from a peer
    /// classified as `trust`.
    pub fn new(source: RequestIdSource, trust: &PeerTrust) -> Self {
        match source {
            RequestIdSource::Accepted if trust.is_trusted() => Self::TrustedPeer,
            RequestIdSource::Accepted => Self::UntrustedCaller,
            RequestIdSource::Generated
            | RequestIdSource::ReplacedInvalid
            | RequestIdSource::ReplacedUntrusted => Self::Generated,
        }
    }

    /// Stable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::TrustedPeer => "trusted_peer",
            Self::UntrustedCaller => "untrusted_caller",
        }
    }
}

/// What the telemetry layer measured about one finalized request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RequestEvidence {
    /// The request id the layer chose.
    pub request_id: RequestId,
    /// Who chose the request id.
    pub request_id_origin: RequestIdOrigin,
    /// Trace id of the request's trace.
    pub trace_id: TraceId,
    /// Span id of Alloy's server span.
    pub span_id: SpanId,
    /// The tenant the application tagged the request with, if any.
    pub tenant: Option<Arc<str>>,
    /// The matched route template, when the router matched one.
    pub route: Option<Arc<str>>,
    /// The response status, when response headers were produced.
    pub status: Option<u16>,
    /// Middleware entry until response headers were produced
    /// (`alloy.server.time_to_headers`), when they were.
    pub time_to_headers: Option<Duration>,
    /// Response headers produced until the body ended, failed, or was
    /// dropped (`alloy.server.body_duration`), when headers were produced.
    pub body_duration: Option<Duration>,
    /// Middleware entry until finalization (`alloy.server.duration`).
    pub duration: Duration,
    /// How the response ended.
    pub outcome: BodyOutcome,
    /// How the trace was rooted.
    pub trace_decision: TraceDecision,
    /// Transport trust label of the direct peer (`PeerTrust::label`).
    pub peer_trust: &'static str,
}

impl RequestEvidence {
    /// Evidence of a request that finalized before it produced response
    /// headers: a generated request id, no tenant, route, or status, zero
    /// duration, outcome [`BodyOutcome::CancelledBeforeHeaders`], a root
    /// trace, and an untrusted peer.
    ///
    /// Intended for testing a sink. Fields may be added in any release, so
    /// set any other field on the returned value.
    pub fn new(request_id: RequestId, trace_id: TraceId, span_id: SpanId) -> Self {
        Self {
            request_id,
            request_id_origin: RequestIdOrigin::Generated,
            trace_id,
            span_id,
            tenant: None,
            route: None,
            status: None,
            time_to_headers: None,
            body_duration: None,
            duration: Duration::ZERO,
            outcome: BodyOutcome::CancelledBeforeHeaders,
            trace_decision: TraceDecision::Root,
            peer_trust: PeerTrust::Untrusted.label(),
        }
    }
}

/// Receives the evidence of every finalized request.
pub trait EvidenceSink: Send + Sync + fmt::Debug + 'static {
    /// Called once per request, on the task or thread that finalized it.
    /// It must not block.
    fn record(&self, evidence: RequestEvidence);
}

/// Per-request state kept until finalization.
#[derive(Debug)]
pub(crate) struct Pending {
    pub(crate) sink: Arc<dyn EvidenceSink>,
    pub(crate) request_id: RequestId,
    pub(crate) request_id_origin: RequestIdOrigin,
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: SpanId,
    pub(crate) tenant: TenantTag,
    pub(crate) trace_decision: TraceDecision,
    pub(crate) peer_trust: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_tags_are_bounded_and_set_once() {
        assert!(valid_tenant("acme"));
        assert!(valid_tenant("org:acme@eu-1.prod_2"));
        assert!(!valid_tenant(""));
        assert!(!valid_tenant("a b"));
        assert!(!valid_tenant("a/b"));
        assert!(valid_tenant(&"a".repeat(MAX_TENANT_BYTES)));
        assert!(!valid_tenant(&"a".repeat(MAX_TENANT_BYTES + 1)));

        let tag = TenantTag::default();
        let clone = tag.clone();
        assert!(!tag.set("not valid"));
        assert_eq!(tag.get(), None);
        assert!(clone.set("acme"));
        assert!(!tag.set("other"));
        assert_eq!(tag.get(), Some("acme"));
        assert_eq!(format!("{tag:?}"), "TenantTag(Some(\"acme\"))");
    }

    #[test]
    fn only_an_accepted_id_has_its_peer_as_its_origin() {
        let trusted = PeerTrust::NetworkBoundary([10, 0, 0, 5].into());
        let untrusted = PeerTrust::Untrusted;
        for trust in [&trusted, &untrusted] {
            for source in [
                RequestIdSource::Generated,
                RequestIdSource::ReplacedInvalid,
                RequestIdSource::ReplacedUntrusted,
            ] {
                let origin = RequestIdOrigin::new(source, trust);
                assert_eq!(origin, RequestIdOrigin::Generated, "{source:?}");
            }
        }
        let origin = RequestIdOrigin::new(RequestIdSource::Accepted, &trusted);
        assert_eq!(origin, RequestIdOrigin::TrustedPeer);
        assert_eq!(origin.as_str(), "trusted_peer");
        let origin = RequestIdOrigin::new(RequestIdSource::Accepted, &untrusted);
        assert_eq!(origin, RequestIdOrigin::UntrustedCaller);
        assert_eq!(origin.as_str(), "untrusted_caller");
        assert_eq!(RequestIdOrigin::Generated.as_str(), "generated");
    }
}
