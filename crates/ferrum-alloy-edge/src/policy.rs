//! Gateway trust policy and request enforcement.
//!
//! Separate concerns stay separate:
//!
//! * *Transport trust* (is the direct peer the gateway?) comes only from the
//!   shared [`TrustClassifier`](ferrum_alloy_telemetry::peer::TrustClassifier): a verified mTLS identity or a configured
//!   network boundary.
//! * *Gateway-asserted identity* (`X-Consumer-Username`,
//!   `X-Authenticated-Identity`) is accepted only from a verified mTLS
//!   identity, never from network trust, and only when enabled. It is
//!   authentication performed by Edge; authorization remains the
//!   application's decision.
//! * Every gateway-asserted request header of the pinned Edge vocabulary
//!   (the whole `x-consumer-*` namespace,
//!   `X-Authenticated-Identity`, `X-Geo-Country`, `X-Ferrum-Hops`, and the
//!   mesh principal headers, in any letter case and with `_` for `-`), and
//!   the `x-path-param-*` captures Edge writes ([`is_gateway_written_header`]),
//!   are removed unless the peer is a verified gateway identity and identity
//!   handoff is enabled, so handlers cannot trust a forged value by accident.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use ferrum_alloy_telemetry::peer::{ClassifiedPeer, PeerTrust, SharedClassifier};
use http::header::{CONTENT_TYPE, HeaderValue};
use http::{HeaderMap, Request, Response, StatusCode};
use ipnet::IpNet;
use tower_layer::Layer;
use tower_service::Service;

use crate::contract::{
    AUTHENTICATED_IDENTITY, CONSUMER_CUSTOM_ID, CONSUMER_USERNAME, MAX_IDENTITY_VALUE_BYTES,
    is_gateway_written_header,
};

/// How the service relates to Ferrum Edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeploymentMode {
    /// No gateway assumptions.
    #[default]
    Standalone,
    /// Use verified gateway metadata when present; accept direct requests.
    GatewayPreferred,
    /// Reject requests without a verified gateway identity, except on exempt
    /// paths. Exempt only paths whose handlers are safe for unauthenticated
    /// callers, such as status-only health probes.
    GatewayRequired,
}

/// Policy configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EdgePolicyConfig {
    /// Deployment mode.
    pub mode: DeploymentMode,
    /// Accept Edge's gateway-asserted headers (consumer and external
    /// identity, geo country, mesh principals) from a verified gateway
    /// identity. Off, or from any other peer, they are removed.
    pub accept_consumer_identity: bool,
    /// Edge's `FERRUM_TRUSTED_PROXIES`: the proxies (load balancers, CDNs)
    /// Edge trusts in front of itself. [`GatewayContext::client_address`]
    /// repeats Edge's own `X-Forwarded-For` resolution with this list, so it
    /// must equal Edge's. Empty (the default) when clients connect to Edge
    /// directly, and when Edge resolves the client from
    /// `FERRUM_REAL_IP_HEADER`, which Alloy cannot repeat.
    pub trusted_proxies: Vec<IpNet>,
}

/// Counters for gateway handling.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct EdgeStats {
    /// Requests with a verified gateway identity.
    pub verified_gateway_requests: AtomicU64,
    /// Requests rejected by `gateway_required`.
    pub rejected_without_gateway: AtomicU64,
    /// Requests from which unverified gateway-asserted headers were removed.
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
    ///
    /// Exempt only paths served by a handler that is safe for
    /// unauthenticated callers. An exempt path that falls through to an
    /// application route or fallback bypasses the gateway requirement.
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
///
/// Later releases may add values. Code that grants anything on the basis of
/// gateway verification must match the variants it accepts by name and treat
/// every other value, including variants it does not know, as unverified
/// (fail closed).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct GatewayContext {
    /// Verified gateway identity.
    pub gateway_identity: String,
    /// `X-Consumer-Username`, when Edge authenticated the caller as a
    /// mapped Consumer.
    pub consumer_username: Option<String>,
    /// `X-Consumer-Custom-Id`, when the consumer has one.
    pub consumer_custom_id: Option<String>,
    /// `X-Authenticated-Identity` (Edge v0.9.15 and later): the identity an
    /// external auth flow such as `jwks_auth`/OIDC authenticated when it
    /// mapped no Consumer. Edge sends it instead of `X-Consumer-Username`,
    /// never with it.
    pub authenticated_identity: Option<String>,
    /// The client Edge resolved from `X-Forwarded-For`. Edge forwards the
    /// chain it received from a trusted proxy with the address it accepted
    /// the connection from appended (`build_xff_value`), so Alloy repeats
    /// Edge's resolution (`client_ip::resolve_client_ip_field_lines`) with
    /// [`EdgePolicyConfig::trusted_proxies`]: walking right to left, the first
    /// address outside the trusted list is the client; a malformed entry
    /// there, or a chain of trusted addresses only, yields Edge's peer, as in
    /// Edge. With an empty list this is Edge's peer. It is not Edge's resolved
    /// client when Edge uses `FERRUM_REAL_IP_HEADER`, or when a non-UTF-8
    /// `X-Forwarded-For` field line holds the malformed entry at the deciding
    /// boundary (Edge resolves its peer, drops the whole non-UTF-8 field line
    /// when forwarding, and Alloy can then select an earlier untrusted address
    /// that survived forwarding).
    pub client_address: Option<IpAddr>,
    /// Edge's immediate peer: the rightmost `X-Forwarded-For` hop, which Edge
    /// writes itself. It is the client only when nothing sits in front of
    /// Edge; behind a load balancer it is the balancer.
    pub edge_peer_address: Option<IpAddr>,
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

/// Every `X-Forwarded-For` entry, in wire order across field lines, as raw
/// bytes with surrounding optional whitespace removed and empty entries
/// skipped, the way Edge's `resolve_client_ip_field_lines` reads them: one
/// entry that is not ASCII (obs-text) or not an address does not discard
/// the others.
fn forwarded_for_entries(headers: &HeaderMap) -> Vec<&[u8]> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .map(|entry| entry.trim_ascii_start().trim_ascii_end())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// An entry as an address, `None` when it is not one.
fn entry_address(entry: &[u8]) -> Option<IpAddr> {
    std::str::from_utf8(entry).ok()?.parse().ok()
}

/// Edge's immediate peer: the rightmost entry, which Edge writes itself
/// (canonical, so an IPv4-mapped spelling reads as IPv4, as Edge reports
/// client identities).
fn edge_peer(entries: &[&[u8]]) -> Option<IpAddr> {
    let hop = entries.last()?;
    let hop = hop
        .strip_prefix(b"[")
        .and_then(|rest| rest.strip_suffix(b"]"))
        .unwrap_or(hop);
    entry_address(hop).map(|ip| ip.to_canonical())
}

/// Whether Edge's trusted-proxy set (`util::cidr::CidrSet`) contains `ip`.
/// An IPv4-mapped IPv6 network with a prefix of at least 96 is the
/// equivalent IPv4 network (a shorter one matches nothing); a mapped query
/// matches an IPv4 network by its embedded address; a native IPv4 query
/// never matches an IPv6 network.
fn trusted_proxy(trusted: &[IpNet], ip: IpAddr) -> bool {
    trusted.iter().any(|network| {
        let network = match network {
            IpNet::V6(v6) => match v6.addr().to_ipv4_mapped() {
                Some(v4) if v6.prefix_len() >= 96 => {
                    match ipnet::Ipv4Net::new(v4, v6.prefix_len() - 96) {
                        Ok(v4) => IpNet::V4(v4.trunc()),
                        Err(_) => return false,
                    }
                }
                Some(_) => return false,
                None => *network,
            },
            IpNet::V4(_) => *network,
        };
        match (network, ip) {
            (IpNet::V4(network), IpAddr::V4(addr)) => network.contains(&addr),
            (IpNet::V4(network), IpAddr::V6(addr)) => addr
                .to_ipv4_mapped()
                .is_some_and(|v4| network.contains(&v4)),
            (IpNet::V6(network), IpAddr::V6(addr)) => network.contains(&addr),
            (IpNet::V6(_), IpAddr::V4(_)) => false,
        }
    })
}

/// Edge's `resolve_client_ip_field_lines` applied to the chain Edge
/// forwarded: the last entry that is not a trusted proxy decides. A valid
/// address there is the client (canonical, as Edge's `canonical_ip_string`);
/// a malformed one, or no untrusted entry at all, falls back to Edge's peer,
/// which Edge would have used. Since Edge appended its peer, an untrusted
/// peer (whose inbound chain Edge dropped) is the last untrusted entry, as in
/// Edge.
fn resolved_client(entries: &[&[u8]], trusted: &[IpNet], peer: Option<IpAddr>) -> Option<IpAddr> {
    let mut client = None;
    for entry in entries {
        match entry_address(entry) {
            Some(ip) if trusted_proxy(trusted, ip) => {}
            Some(ip) => client = Some(ip.to_canonical()),
            None => client = None,
        }
    }
    client.or(peer)
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
        // Reuse the transport's per-connection classification when this
        // policy's classifier computed it, exactly like the telemetry
        // layer, so both judge the request by the same single
        // classification. A classification from a different (possibly
        // broader) classifier is recomputed under this policy's rules:
        // cached trust must never bypass an independently configured
        // policy (#214).
        let trust = match request
            .extensions()
            .get::<ClassifiedPeer>()
            .and_then(|classified| classified.trust_for(&policy.classifier))
        {
            Some(trust) => trust.clone(),
            None => policy.classifier.classify(request.extensions()),
        };
        let verification = match &trust {
            PeerTrust::VerifiedIdentity(identity) => GatewayVerification::Verified {
                identity: identity.clone(),
            },
            PeerTrust::NetworkBoundary(_) => GatewayVerification::NetworkTrusted,
            // Untrusted, and any trust a later telemetry release adds that
            // this adapter does not know: never gateway trust.
            _ => GatewayVerification::Unverified,
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

        let assertion_present = request
            .headers()
            .keys()
            .any(|name| is_gateway_written_header(name.as_str()));
        match (&verification, policy.config.accept_consumer_identity) {
            (GatewayVerification::Verified { identity }, true) => {
                policy
                    .stats
                    .verified_gateway_requests
                    .fetch_add(1, Ordering::Relaxed);
                let headers = request.headers();
                let entries = forwarded_for_entries(headers);
                let edge_peer_address = edge_peer(&entries);
                let client_address =
                    resolved_client(&entries, &policy.config.trusted_proxies, edge_peer_address);
                let context = GatewayContext {
                    gateway_identity: identity.clone(),
                    consumer_username: header_value(headers, CONSUMER_USERNAME),
                    consumer_custom_id: header_value(headers, CONSUMER_CUSTOM_ID),
                    authenticated_identity: header_value(headers, AUTHENTICATED_IDENTITY),
                    client_address,
                    edge_peer_address,
                };
                request.extensions_mut().insert(context);
            }
            (GatewayVerification::Verified { .. }, false) => {
                policy
                    .stats
                    .verified_gateway_requests
                    .fetch_add(1, Ordering::Relaxed);
                strip(&mut request, assertion_present, &policy.stats);
            }
            _ => strip(&mut request, assertion_present, &policy.stats),
        }
        request.extensions_mut().insert(verification);
        Box::pin(self.inner.call(request))
    }
}

fn strip<B>(request: &mut Request<B>, present: bool, stats: &EdgeStats) {
    if !present {
        return;
    }
    let forged: Vec<_> = request
        .headers()
        .keys()
        .filter(|name| is_gateway_written_header(name.as_str()))
        .cloned()
        .collect();
    for name in forged {
        request.headers_mut().remove(name);
    }
    stats
        .stripped_unverified_identity
        .fetch_add(1, Ordering::Relaxed);
    tracing::debug!(target: "ferrum_alloy::edge", "removed gateway-asserted headers from a request without a verified gateway identity");
}

/// Parity with a real Ferrum Edge: CI sends each case of
/// `fixtures/edge-parity/forwarded-for.json` through Edge with the same
/// trusted proxies and checks that Edge forwards `forwarded` and resolves
/// `client`. Alloy must resolve the same client from `forwarded`, except in a
/// recorded `known_divergence`.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod edge_parity_tests {
    use super::*;

    #[test]
    fn the_edge_parity_fixture_resolves_like_edge() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/edge-parity/forwarded-for.json");
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let trusted: Vec<IpNet> = fixture["trusted_proxies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cidr| cidr.as_str().unwrap().parse().unwrap())
            .collect();
        let default_peer = fixture["edge_peer"].as_str().unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 15, "{}", cases.len());
        let (mut untrusted_peers, mut divergences) = (0, 0);
        for case in cases {
            let forwarded = case["forwarded"].as_str().unwrap();
            let peer_address: IpAddr = case["peer"]
                .as_str()
                .unwrap_or(default_peer)
                .parse()
                .unwrap();
            if !trusted_proxy(&trusted, peer_address) {
                // Edge drops an untrusted peer's chain and forwards the peer.
                untrusted_peers += 1;
                assert_eq!(forwarded, peer_address.to_string(), "{case}");
            } else if let Some(inbound) = case["x_forwarded_for"].as_str() {
                assert_eq!(
                    forwarded.strip_suffix(&format!(", {peer_address}")),
                    Some(inbound),
                    "Edge appends its trusted peer to the inbound chain"
                );
            }
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", HeaderValue::from_str(forwarded).unwrap());
            let entries = forwarded_for_entries(&headers);
            let peer = edge_peer(&entries);
            assert_eq!(peer, Some(peer_address), "{forwarded}");
            let client = resolved_client(&entries, &trusted, peer);
            let edge_client: IpAddr = case["client"].as_str().unwrap().parse().unwrap();
            match case["known_divergence"].as_str() {
                None => assert_eq!(client, Some(edge_client), "{case}"),
                Some(_) => {
                    divergences += 1;
                    let alloy_client: IpAddr =
                        case["alloy_client"].as_str().unwrap().parse().unwrap();
                    assert_eq!(client, Some(alloy_client), "{case}");
                    assert_ne!(alloy_client, edge_client, "{case}");
                }
            }
        }
        assert!(untrusted_peers >= 2, "the untrusted-peer gate is exercised");
        assert_eq!(divergences, 1, "documented in docs/configuration.md");
    }
}
