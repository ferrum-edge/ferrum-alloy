//! Compile-time checks of the public API's error contract: every public error
//! type is a `std::error::Error` that can cross threads and async tasks.

use ferrum_alloy_telemetry::context::MissingRequestContext;
use ferrum_alloy_telemetry::layer::TelemetryConfigError;
use ferrum_alloy_telemetry::peer::{PeerTrust, TrustedPeersError};
use ferrum_alloy_telemetry::trace_context::TraceContextError;

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn public_errors_are_send_sync_static_errors() {
    assert_error::<TelemetryConfigError>();
    assert_error::<TrustedPeersError>();
    assert_error::<TraceContextError>();
    assert_error::<MissingRequestContext>();
    #[cfg(feature = "subscriber")]
    assert_error::<ferrum_alloy_telemetry::init::InitError>();
    #[cfg(feature = "otel")]
    assert_error::<ferrum_alloy_telemetry::otel::OtelError>();
    #[cfg(feature = "x509")]
    assert_error::<ferrum_alloy_telemetry::peer::PeerCertificateError>();
}

/// `PeerTrust` is `#[non_exhaustive]`: only the variants that are trusted by
/// name count as trusted, so a classification added later fails closed.
#[test]
fn only_named_trusted_classifications_are_trusted() {
    let identity = PeerTrust::VerifiedIdentity("spiffe://ferrum.test/ns/edge/sa/gateway".into());
    let network = PeerTrust::NetworkBoundary(std::net::IpAddr::from([127, 0, 0, 1]));
    assert!(identity.is_trusted());
    assert!(identity.is_verified_identity());
    assert!(network.is_trusted());
    assert!(!network.is_verified_identity());
    assert!(!PeerTrust::Untrusted.is_trusted());
    assert!(!PeerTrust::Untrusted.is_verified_identity());
}
