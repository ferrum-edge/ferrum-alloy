//! Compile-time checks of the public API's error contract: every public error
//! type is a `std::error::Error` that can cross threads and async tasks.

use ferrum_alloy_diagnostics::edge_record::RecordError;
use ferrum_alloy_diagnostics::otlp::ImportError;
use ferrum_alloy_diagnostics::parse::ReportError;

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn public_errors_are_send_sync_static_errors() {
    assert_error::<ReportError>();
    assert_error::<ImportError>();
    assert_error::<RecordError>();
}
