//! Edge diagnostic-ref records and client observations are read from
//! operator-supplied files and log captures.

#![no_main]

use ferrum_alloy_diagnostics::edge_record::{bind_record, parse_observation};
use ferrum_alloy_diagnostics::model::Confidence;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // First line: the client observation. Rest: the record to bind.
    let (observation_bytes, record) = match data.iter().position(|&b| b == b'\n') {
        Some(split) => (&data[..split], &data[split + 1..]),
        None => (data, data),
    };
    let Ok(observation) = parse_observation(observation_bytes) else {
        return;
    };
    // Parsing the same observation again yields the same reference.
    if let Ok(again) = parse_observation(observation_bytes) {
        assert_eq!(again.reference(), observation.reference());
    }
    let Ok(bound) = bind_record(record, &observation) else {
        return;
    };
    let finding = bound.finding();
    // A bound record cannot authenticate itself.
    assert_ne!(finding.confidence, Confidence::Confirmed);
    assert!(!finding.does_not_prove.is_empty());
    // Binding depends only on its inputs.
    let rebound = bind_record(record, &observation).expect("binding is deterministic");
    assert_eq!(rebound.known_vocabulary(), bound.known_vocabulary());
    assert_eq!(rebound.finding(), finding);
});
