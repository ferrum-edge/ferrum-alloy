//! Incoming request ids come from any client.

#![no_main]

use ferrum_alloy_telemetry::RequestId;
use ferrum_alloy_telemetry::request_id::MAX_REQUEST_ID_BYTES;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(id) = RequestId::parse(text) {
        // Accepted ids are kept verbatim and stay inside Ferrum Edge's
        // `correlation_id` alphabet and length.
        assert_eq!(id.as_str(), text);
        assert!(!text.is_empty() && text.len() <= MAX_REQUEST_ID_BYTES);
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        );
    }
});
