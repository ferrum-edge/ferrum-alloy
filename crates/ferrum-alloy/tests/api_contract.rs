//! Compile-time checks of the public API's error contract: every public error
//! type is a `std::error::Error` that can cross threads and async tasks.

use ferrum_alloy::config::ConfigError;
use ferrum_alloy::error::AlloyError;
use ferrum_alloy::extract::ValidationErrors;
use ferrum_alloy::files::ReadError;

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn public_errors_are_send_sync_static_errors() {
    assert_error::<AlloyError>();
    assert_error::<ConfigError>();
    assert_error::<ReadError>();
    assert_error::<ValidationErrors>();
    #[cfg(feature = "jwt")]
    assert_error::<ferrum_alloy::jwt::AuthError>();
    #[cfg(feature = "tls")]
    assert_error::<ferrum_alloy::tls::TlsError>();
}

/// `CheckError` is not a `std::error::Error` (see its `From` conversion), but
/// it crosses the readiness task boundary.
#[test]
fn check_errors_cross_tasks() {
    fn assert_send_sync<T: std::fmt::Display + Send + Sync + 'static>() {}
    assert_send_sync::<ferrum_alloy::health::CheckError>();
}
