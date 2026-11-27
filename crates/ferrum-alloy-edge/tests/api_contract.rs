//! Compile-time checks of the public API's error contract: every public error
//! type is a `std::error::Error` that can cross threads and async tasks.

use ferrum_alloy_edge::agents::AgentToolError;
use ferrum_alloy_edge::export::ExportError;
use ferrum_alloy_edge::manifest::ManifestError;

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn public_errors_are_send_sync_static_errors() {
    assert_error::<ManifestError>();
    assert_error::<ExportError>();
    assert_error::<AgentToolError>();
}
