//! `traceparent` and `tracestate` values come from any client.

#![no_main]

use ferrum_alloy_telemetry::trace_context::{parse_traceparent, validate_tracestate};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(parsed) = parse_traceparent(text) {
        // An accepted value re-encodes as version 00 and parses back to the
        // same ids, as Ferrum Edge's own round-trip invariant requires.
        let rebuilt = parse_traceparent(&parsed.to_header_value());
        assert_eq!(
            rebuilt.map(|p| (p.trace_id, p.parent_id, p.sampled())),
            Ok((parsed.trace_id, parsed.parent_id, parsed.sampled()))
        );
    }
    if let Some(state) = validate_tracestate(text) {
        // The normalized list is valid, stable, and never longer than the input.
        assert!(state.as_str().len() <= text.len());
        assert_eq!(validate_tracestate(state.as_str()).as_ref(), Some(&state));
    }
});
