//! Compile-time checks of the public API's error contract: every public error
//! type is a `std::error::Error` that can cross threads and async tasks.

use ferrum_alloy_telemetry::context::MissingRequestContext;
use ferrum_alloy_telemetry::layer::TelemetryConfigError;
use ferrum_alloy_telemetry::peer::TrustedPeersError;
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
