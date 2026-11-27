//! Compile-time checks of the public API's error contract: every public error
//! type is a `std::error::Error` that can cross threads and async tasks.

use ferrum_alloy::config::ConfigError;
use ferrum_alloy::error::AlloyError;
use ferrum_alloy::extract::ValidationErrors;
use ferrum_alloy::files::ReadError;
use ferrum_alloy::health::CheckError;

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn public_errors_are_send_sync_static_errors() {
    assert_error::<AlloyError>();
    assert_error::<ConfigError>();
    assert_error::<ReadError>();
    assert_error::<ValidationErrors>();
    assert_error::<CheckError>();
    #[cfg(feature = "jwt")]
    assert_error::<ferrum_alloy::jwt::AuthError>();
    #[cfg(feature = "tls")]
    assert_error::<ferrum_alloy::tls::TlsError>();
}

/// A readiness check's error propagates as a boxed error, and any error
/// converts into one explicitly.
#[test]
fn check_errors_are_errors() {
    let error = CheckError::from_error(std::io::Error::other("connection refused"));
    assert_eq!(error.message(), "connection refused");
    let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(error);
    assert_eq!(boxed.to_string(), "connection refused");
}
