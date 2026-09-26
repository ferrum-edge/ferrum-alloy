//! Transport identity and peer trust classification.
//!
//! Trust is derived only from the transport: a client certificate verified by
//! the TLS stack, or a source address inside a deliberately configured network
//! boundary (for example a sidecar on loopback). Forwarded headers such as
//! `X-Forwarded-For` are never consulted.
//!
//! Peer trust only decides whether *propagation metadata* (trace context,
//! request ids) is honored and whether gateway-asserted headers are believed.
//! It never authenticates an end user or authorizes an operation.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use http::Extensions;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// Transport facts about the connection a request arrived on.
///
/// Servers insert this into request extensions. Alloy's own server does so
/// automatically; applications that terminate TLS themselves can insert it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerInfo {
    /// Remote socket address of the direct peer.
    pub remote_addr: Option<SocketAddr>,
    /// TLS peer identity, when the connection used TLS.
    pub tls: Option<TlsPeer>,
}

/// Identity presented by a TLS client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsPeer {
    /// `true` only when the TLS stack verified the client certificate chain
    /// against configured trust anchors. Unverified certificates never
    /// establish identity.
    pub client_cert_verified: bool,
    /// The single `spiffe://` URI SAN, when present exactly once.
    pub spiffe_id: Option<String>,
    /// DNS SANs of the leaf certificate.
    pub dns_names: Vec<String>,
}

/// Why a peer is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTrust {
    /// A verified client certificate matched a configured identity.
    VerifiedIdentity(String),
    /// The source address is inside a configured trusted network boundary.
    NetworkBoundary(IpAddr),
    /// Not trusted.
    Untrusted,
}

impl PeerTrust {
    /// Returns `true` for either trusted variant.
    pub fn is_trusted(&self) -> bool {
        !matches!(self, Self::Untrusted)
    }

    /// Returns `true` only for a verified transport identity.
    pub fn is_verified_identity(&self) -> bool {
        matches!(self, Self::VerifiedIdentity(_))
    }

    /// Stable label for spans and metrics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::VerifiedIdentity(_) => "verified_identity",
            Self::NetworkBoundary(_) => "network_boundary",
            Self::Untrusted => "untrusted",
        }
    }
}

/// Classifies the peer of a request.
pub trait TrustClassifier: Send + Sync + fmt::Debug + 'static {
    /// Decides trust from request extensions (never from headers).
    fn classify(&self, extensions: &Extensions) -> PeerTrust;
}

/// Trusts nobody. The default.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrustNobody;

impl TrustClassifier for TrustNobody {
    fn classify(&self, _extensions: &Extensions) -> PeerTrust {
        PeerTrust::Untrusted
    }
}

/// Trusted peer configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedPeersConfig {
    /// Exact identities accepted from verified client certificates:
    /// `spiffe://...` ids, or `dns:<name>` for a DNS SAN.
    #[serde(default)]
    pub identities: Vec<String>,
    /// Source networks treated as a trusted termination boundary. Use only
    /// when the network path is isolated (for example loopback from a
    /// sidecar) and direct access is prevented.
    #[serde(default)]
    pub networks: Vec<IpNet>,
}

/// Why a trusted-peer configuration was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrustedPeersError {
    /// An identity is neither a SPIFFE id nor `dns:<name>`.
    #[error("trusted identity {0:?} must be a spiffe:// id or dns:<name>")]
    InvalidIdentity(String),
    /// A network would trust every address.
    #[error("trusted network {0} would trust every address; list specific networks")]
    TooBroad(IpNet),
}

/// Matches verified identities and trusted networks.
#[derive(Debug, Clone, Default)]
pub struct TrustedPeers {
    spiffe_ids: Vec<String>,
    dns_names: Vec<String>,
    networks: Vec<IpNet>,
}

impl TrustedPeers {
    /// Validates a configuration.
    pub fn new(config: &TrustedPeersConfig) -> Result<Self, TrustedPeersError> {
        let mut peers = Self::default();
        for identity in &config.identities {
            if let Some(rest) = identity.strip_prefix("spiffe://") {
                if rest.is_empty() || rest.contains(char::is_whitespace) {
                    return Err(TrustedPeersError::InvalidIdentity(identity.clone()));
                }
                peers.spiffe_ids.push(identity.clone());
            } else if let Some(name) = identity.strip_prefix("dns:") {
                if name.is_empty() || name.contains(char::is_whitespace) {
                    return Err(TrustedPeersError::InvalidIdentity(identity.clone()));
                }
                peers.dns_names.push(name.to_ascii_lowercase());
            } else {
                return Err(TrustedPeersError::InvalidIdentity(identity.clone()));
            }
        }
        for network in &config.networks {
            if network.prefix_len() == 0 {
                return Err(TrustedPeersError::TooBroad(*network));
            }
            peers.networks.push(network.trunc());
        }
        Ok(peers)
    }

    /// Returns `true` when no identity or network is configured.
    pub fn is_empty(&self) -> bool {
        self.spiffe_ids.is_empty() && self.dns_names.is_empty() && self.networks.is_empty()
    }

    /// Classifies a peer.
    pub fn classify_peer(&self, peer: &PeerInfo) -> PeerTrust {
        if let Some(tls) = &peer.tls
            && tls.client_cert_verified
        {
            if let Some(id) = &tls.spiffe_id
                && self.spiffe_ids.iter().any(|allowed| allowed == id)
            {
                return PeerTrust::VerifiedIdentity(id.clone());
            }
            for name in &tls.dns_names {
                let name = name.to_ascii_lowercase();
                if self.dns_names.contains(&name) {
                    return PeerTrust::VerifiedIdentity(format!("dns:{name}"));
                }
            }
        }
        if let Some(addr) = peer.remote_addr {
            let ip = canonical_ip(addr.ip());
            if self.networks.iter().any(|net| net.contains(&ip)) {
                return PeerTrust::NetworkBoundary(ip);
            }
        }
        PeerTrust::Untrusted
    }
}

/// Maps IPv4-mapped IPv6 addresses to IPv4 so `127.0.0.1/8` matches
/// `::ffff:127.0.0.1` from a dual-stack listener.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

impl TrustClassifier for TrustedPeers {
    fn classify(&self, extensions: &Extensions) -> PeerTrust {
        match peer_info(extensions) {
            Some(peer) => self.classify_peer(&peer),
            None => PeerTrust::Untrusted,
        }
    }
}

/// Reads [`PeerInfo`] from extensions, falling back to axum's
/// `ConnectInfo<SocketAddr>` for the remote address.
pub fn peer_info(extensions: &Extensions) -> Option<PeerInfo> {
    if let Some(peer) = extensions.get::<PeerInfo>() {
        return Some(peer.clone());
    }
    #[cfg(feature = "axum")]
    if let Some(axum::extract::ConnectInfo(addr)) =
        extensions.get::<axum::extract::ConnectInfo<SocketAddr>>()
    {
        return Some(PeerInfo {
            remote_addr: Some(*addr),
            tls: None,
        });
    }
    None
}

/// A shared classifier handle.
pub type SharedClassifier = Arc<dyn TrustClassifier>;
