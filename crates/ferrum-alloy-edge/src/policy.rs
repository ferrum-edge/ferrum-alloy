//! Gateway trust policy and request enforcement.
//!
//! Separate concerns stay separate:
//!
//! * *Transport trust* (is the direct peer the gateway?) comes only from the
//!   shared [`TrustClassifier`]: a verified mTLS identity or a configured
//!   network boundary.
//! * *Gateway-asserted identity* (`X-Consumer-Username`) is accepted only
//!   from a verified mTLS identity, never from network trust, and only when
//!   enabled. It is authentication performed by Edge; authorization remains
//!   the application's decision.
//! * Unverified copies of gateway-asserted headers are removed so handlers
//!   cannot trust a forged value by accident.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use ferrum_alloy_telemetry::peer::{PeerTrust, SharedClassifier};
use http::header::{CONTENT_TYPE, HeaderValue};
use http::{HeaderMap, Request, Response, StatusCode};
use tower_layer::Layer;
use tower_service::Service;

use crate::contract::{
    CONSUMER_CUSTOM_ID, CONSUMER_USERNAME, IDENTITY_HEADERS, MAX_IDENTITY_VALUE_BYTES,
};

/// How the service relates to Ferrum Edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeploymentMode {
    /// No gateway assumptions.
    #[default]
    Standalone,
    /// Use verified gateway metadata when present; accept direct requests.
    GatewayPreferred,
    /// Reject requests without a verified gateway identity (except exempt
    /// health paths).
    GatewayRequired,
}

/// Policy configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgePolicyConfig {
    /// Deployment mode.
    pub mode: DeploymentMode,
    /// Accept consumer identity headers from a verified gateway identity.
    pub accept_consumer_identity: bool,
}

/// Counters for gateway handling.
#[derive(Debug, Default)]
pub struct EdgeStats {
    /// Requests with a verified gateway identity.
    pub verified_gateway_requests: AtomicU64,
    /// Requests rejected by `gateway_required`.
    pub rejected_without_gateway: AtomicU64,
    /// Requests from which unverified identity headers were removed.
    pub stripped_unverified_identity: AtomicU64,
}

/// A compiled policy.
#[derive(Debug, Clone)]
pub struct EdgePolicy {
    config: EdgePolicyConfig,
    classifier: SharedClassifier,
    exempt_paths: Arc<Vec<String>>,
    stats: Arc<EdgeStats>,
}

impl EdgePolicy {
    /// Creates a policy using the service's trust classifier.
    pub fn new(config: EdgePolicyConfig, classifier: SharedClassifier) -> Self {
        Self {
            config,
            classifier,
            exempt_paths: Arc::new(Vec::new()),
            stats: Arc::new(EdgeStats::default()),
        }
    }

    /// Paths reachable without a gateway identity in `gateway_required`
    /// mode (health probes). Exact matches only.
    #[must_use]
    pub fn with_exempt_paths(mut self, paths: Vec<String>) -> Self {
        self.exempt_paths = Arc::new(paths);
        self
    }

    /// Counters.
    pub fn stats(&self) -> Arc<EdgeStats> {
        Arc::clone(&self.stats)
    }
}

/// Whether the request arrived through a verified gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayVerification {
    /// The direct peer presented a verified identity the service trusts.
    Verified {
        /// The matched identity (e.g. a SPIFFE id).
        identity: String,
    },
    /// The peer is inside a trusted network boundary but not identified.
    NetworkTrusted,
    /// No gateway trust.
    Unverified,
}

/// Metadata asserted by a verified gateway. Present only when the peer's
/// identity was verified and identity handoff is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayContext {
    /// Verified gateway identity.
    pub gateway_identity: String,
    /// `X-Consumer-Username`, when Edge authenticated the caller.
    pub consumer_username: Option<String>,
    /// `X-Consumer-Custom-Id`, when the consumer has one.
    pub consumer_custom_id: Option<String>,
    /// The client address Edge observed (rightmost `X-Forwarded-For` hop,
    /// which Edge writes itself).
    pub client_address: Option<IpAddr>,
}

#[cfg(feature = "axum")]
impl<S: Send + Sync> axum::extract::OptionalFromRequestParts<S> for GatewayContext {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<GatewayContext>().cloned())
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let text = first.to_str().ok()?;
    (!text.is_empty() && text.len() <= MAX_IDENTITY_VALUE_BYTES).then(|| text.to_owned())
}

fn forwarded_client(headers: &HeaderMap) -> Option<IpAddr> {
    let last = headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()?
        .to_str()
        .ok()?
        .rsplit(',')
        .next()?
        .trim();
    last.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// The problem type used for `gateway_required` rejections (kept in sync
/// with `ferrum_alloy::ProblemKind::GatewayRequired`).
pub const GATEWAY_REQUIRED_TYPE: &str = "tag:ferrumedge.com,2026:alloy/problem/gateway-required";

fn gateway_required<B: RejectionBody>() -> Response<B> {
    let body = format!(
        r#"{{"type":"{GATEWAY_REQUIRED_TYPE}","title":"Gateway required","status":403,"detail":"This service accepts requests only through its configured gateway."}}"#
    );
    let mut response = Response::new(B::from_json(body));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

/// Applies [`EdgePolicy`] to requests.
#[derive(Debug, Clone)]
pub struct EdgeLayer {
    policy: EdgePolicy,
}

impl EdgeLayer {
    /// Creates the layer.
    pub fn new(policy: EdgePolicy) -> Self {
        Self { policy }
    }
}

impl<S> Layer<S> for EdgeLayer {
    type Service = EdgeService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        EdgeService {
            inner,
            policy: self.policy.clone(),
        }
    }
}

/// Service produced by [`EdgeLayer`].
#[derive(Debug, Clone)]
pub struct EdgeService<S> {
    inner: S,
    policy: EdgePolicy,
}

/// Response body types the adapter can write its rejection into.
pub trait RejectionBody {
    /// Builds a body from JSON text.
    fn from_json(json: String) -> Self;
}

#[cfg(feature = "axum")]
impl RejectionBody for axum::body::Body {
    fn from_json(json: String) -> Self {
        axum::body::Body::from(json)
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for EdgeService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
    ResBody: RejectionBody + Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response<ResBody>, S::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        let policy = &self.policy;
        let trust = policy.classifier.classify(request.extensions());
        let verification = match &trust {
            PeerTrust::VerifiedIdentity(identity) => GatewayVerification::Verified {
                identity: identity.clone(),
            },
            PeerTrust::NetworkBoundary(_) => GatewayVerification::NetworkTrusted,
            PeerTrust::Untrusted => GatewayVerification::Unverified,
        };

        if policy.config.mode == DeploymentMode::GatewayRequired
            && !trust.is_verified_identity()
            && !policy
                .exempt_paths
                .iter()
                .any(|p| p == request.uri().path())
        {
            policy
                .stats
                .rejected_without_gateway
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(target: "ferrum_alloy::edge", peer_trust = trust.label(), "rejected request without a verified gateway identity");
            let response = gateway_required::<ResBody>();
            return Box::pin(async move { Ok(response) });
        }

        let identity_present = IDENTITY_HEADERS
            .iter()
            .any(|name| request.headers().contains_key(*name));
        match (&verification, policy.config.accept_consumer_identity) {
            (GatewayVerification::Verified { identity }, true) => {
                policy
                    .stats
                    .verified_gateway_requests
                    .fetch_add(1, Ordering::Relaxed);
                let headers = request.headers();
                let context = GatewayContext {
                    gateway_identity: identity.clone(),
                    consumer_username: header_value(headers, CONSUMER_USERNAME),
                    consumer_custom_id: header_value(headers, CONSUMER_CUSTOM_ID),
                    client_address: forwarded_client(headers),
                };
                request.extensions_mut().insert(context);
            }
            (GatewayVerification::Verified { .. }, false) => {
                policy
                    .stats
                    .verified_gateway_requests
                    .fetch_add(1, Ordering::Relaxed);
                strip(&mut request, identity_present, &policy.stats);
            }
            _ => strip(&mut request, identity_present, &policy.stats),
        }
        request.extensions_mut().insert(verification);
        Box::pin(self.inner.call(request))
    }
}

fn strip<B>(request: &mut Request<B>, present: bool, stats: &EdgeStats) {
    if !present {
        return;
    }
    for name in IDENTITY_HEADERS {
        request.headers_mut().remove(*name);
    }
    stats
        .stripped_unverified_identity
        .fetch_add(1, Ordering::Relaxed);
    tracing::debug!(target: "ferrum_alloy::edge", "removed gateway identity headers from a request without a verified gateway identity");
}
