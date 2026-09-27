//! Property tests: header parsers keep Ferrum Edge's accept/reject behavior,
//! and response body accounting finalizes exactly once with the outcome the
//! frame sequence implies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use ferrum_alloy_telemetry::metrics::{BODY_OUTCOMES, Metrics};
use ferrum_alloy_telemetry::request_id::MAX_REQUEST_ID_BYTES;
use ferrum_alloy_telemetry::trace_context::{parse_traceparent, validate_tracestate};
use ferrum_alloy_telemetry::{RequestId, TelemetryConfig, TelemetryLayer};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body::{Body, Frame};
use proptest::prelude::*;
use tower::{Layer, Service, service_fn};

/// Ferrum Edge v0.9.7 `OtelTracing::parse_traceparent`
/// (`src/plugins/otel_tracing.rs`), transcribed for differential testing.
fn edge_parse_traceparent(value: &str) -> Option<(&str, &str, &str)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_span_id = parts.next()?;
    let flags = parts.next()?;
    if !edge_is_lowercase_hex(version, 2)
        || !edge_is_lowercase_hex(trace_id, 32)
        || !edge_is_lowercase_hex(parent_span_id, 16)
        || !edge_is_lowercase_hex(flags, 2)
        || version == "ff"
        || trace_id.chars().all(|c| c == '0')
        || parent_span_id.chars().all(|c| c == '0')
    {
        return None;
    }
    if version == "00" && parts.next().is_some() {
        return None;
    }
    Some((trace_id, parent_span_id, flags))
}

fn edge_is_lowercase_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Ferrum Edge v0.9.7 `correlation_id` acceptance
/// (`src/plugins/correlation_id.rs`).
fn edge_accepts_correlation_id(value: &str) -> bool {
    value.len() <= 256
        && !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Header values are visible ASCII plus space and tab. Alloy trims only
/// space and tab, which for such values matches Edge's `str::trim`.
fn header_trim(value: &str) -> &str {
    value.trim_matches([' ', '\t'])
}

/// Accepted by Ferrum Edge's own parser tests (v0.9.7
/// `src/plugins/otel_tracing.rs` and `tests/unit/plugins/otel_tracing_tests.rs`).
const EDGE_ACCEPTS: &[&str] = &[
    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
    "00-abcdef1234567890abcdef1234567890-1234567890abcdef-01",
    "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01",
    "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
];

/// Rejected by the same Ferrum Edge tests.
const EDGE_REJECTS: &[&str] = &[
    "00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01",
    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
    "not-a-valid-traceparent",
    "invalid",
];

#[test]
fn edge_traceparent_fixtures_agree() {
    for value in EDGE_ACCEPTS {
        assert!(edge_parse_traceparent(value).is_some(), "model: {value}");
        assert!(parse_traceparent(value).is_ok(), "{value}");
    }
    for value in EDGE_REJECTS {
        assert!(edge_parse_traceparent(value).is_none(), "model: {value}");
        assert!(parse_traceparent(value).is_err(), "{value}");
    }
}

fn traceparent_like() -> impl Strategy<Value = String> {
    let lead = prop_oneof!["", " ", "\t"];
    let version = prop_oneof!["00", "01", "fe", "ff", "[0-9a-fA-F]{2}", "[0-9a-f-]{0,3}"];
    let trace = prop_oneof!["[0-9a-f]{32}", "0{32}", "[0-9a-fA-F]{30,33}"];
    let parent = prop_oneof!["[0-9a-f]{16}", "0{16}", "[0-9a-fA-F]{15,17}"];
    let flags = prop_oneof!["[0-9a-f]{2}", "[0-9a-zA-Z]{1,3}"];
    let tail = prop_oneof!["", "-[a-z0-9-]{0,8}", "[ \t]{1,2}", "[x_]"];
    let parts = (lead, version, trace, parent, flags, tail);
    parts.prop_map(join_traceparent)
}

fn join_traceparent(parts: (String, String, String, String, String, String)) -> String {
    let (lead, version, trace, parent, flags, tail) = parts;
    format!("{lead}{version}-{trace}-{parent}-{flags}{tail}")
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Data(u8),
    Trailers,
    Error,
}

/// A body that yields a fixed frame sequence, then its end.
struct Scripted {
    steps: Vec<Step>,
    next: usize,
    /// Like `Full`, report the end as soon as the last frame is out, so the
    /// server never polls for `None`.
    eager_end: bool,
}

impl Body for Scripted {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let Some(step) = self.steps.get(self.next).copied() else {
            return Poll::Ready(None);
        };
        self.next += 1;
        Poll::Ready(Some(match step {
            Step::Data(len) => Ok(Frame::data(Bytes::from(vec![b'x'; usize::from(len)]))),
            Step::Trailers => Ok(Frame::trailers(HeaderMap::new())),
            Step::Error => Err(std::io::Error::other("scripted failure")),
        }))
    }

    fn is_end_stream(&self) -> bool {
        self.eager_end && self.next >= self.steps.len()
    }
}

#[derive(Debug, Clone)]
struct Exchange {
    method: Method,
    status: StatusCode,
    steps: Vec<Step>,
    eager_end: bool,
    /// How many times the server polls the body before it stops (client
    /// disconnect, timeout, or shutdown) and drops it.
    polls: usize,
}

/// The outcome the exchange must record, derived from the frame sequence.
fn expected_outcome(exchange: &Exchange) -> &'static str {
    if exchange.status == StatusCode::SWITCHING_PROTOCOLS {
        return "upgraded";
    }
    if exchange.method == Method::HEAD || exchange.status == StatusCode::NO_CONTENT {
        return "not_sent";
    }
    let steps = &exchange.steps;
    if exchange.eager_end && steps.is_empty() {
        return "completed";
    }
    for next in 0..exchange.polls {
        let Some(step) = steps.get(next) else {
            return "completed";
        };
        match step {
            Step::Error => return "error",
            Step::Data(_) | Step::Trailers => {
                if exchange.eager_end && next + 1 == steps.len() {
                    return "completed";
                }
            }
        }
    }
    "cancelled"
}

/// Serves one request through the telemetry layer and drives its body the
/// way Hyper does: never past the end, never after an error.
fn serve(exchange: &Exchange) -> Arc<Metrics> {
    let layer = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let metrics = layer.metrics();
    let mut body = Some(Scripted {
        steps: exchange.steps.clone(),
        next: 0,
        eager_end: exchange.eager_end,
    });
    let status = exchange.status;
    let mut service = layer.layer(service_fn(move |_request: Request<()>| {
        let mut response = Response::new(body.take().unwrap());
        *response.status_mut() = status;
        async move { Ok::<_, Infallible>(response) }
    }));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(service.poll_ready(&mut cx).is_ready());
    let request = Request::builder()
        .method(exchange.method.clone())
        .uri("/items")
        .body(())
        .unwrap();
    let mut future = Box::pin(service.call(request));
    let Poll::Ready(Ok(response)) = future.as_mut().poll(&mut cx) else {
        panic!("a ready service responds on the first poll");
    };
    let mut body = Box::pin(response.into_body());
    for _ in 0..exchange.polls {
        if body.is_end_stream() {
            break;
        }
        match body.as_mut().poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(_))) | Poll::Pending => {}
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => break,
        }
    }
    drop(body);
    drop(future);
    metrics
}

fn exchange() -> impl Strategy<Value = Exchange> {
    let method = prop_oneof![4 => Just(Method::GET), 1 => Just(Method::HEAD)];
    let status = prop_oneof![
        6 => Just(StatusCode::OK),
        1 => Just(StatusCode::NO_CONTENT),
        1 => Just(StatusCode::SWITCHING_PROTOCOLS),
    ];
    let step = prop_oneof![
        4 => any::<u8>().prop_map(Step::Data),
        1 => Just(Step::Trailers),
        1 => Just(Step::Error),
    ];
    let steps = prop::collection::vec(step, 0..8);
    let parts = (method, status, steps, any::<bool>(), 0usize..12);
    parts.prop_map(|(method, status, steps, eager_end, polls)| Exchange {
        method,
        status,
        steps,
        eager_end,
        polls,
    })
}

proptest! {
    #[test]
    fn traceparent_matches_ferrum_edge(value in prop_oneof![
        traceparent_like(),
        "[0-9a-fA-F \\t-]{0,70}",
    ]) {
        let edge = edge_parse_traceparent(&value);
        let alloy = parse_traceparent(header_trim(&value));
        prop_assert_eq!(alloy.is_ok(), edge.is_some(), "{:?}", value);
        if let (Ok(parsed), Some((trace_id, parent_id, flags))) = (alloy, edge) {
            prop_assert_eq!(parsed.trace_id.to_hex(), trace_id);
            prop_assert_eq!(parsed.parent_id.to_hex(), parent_id);
            prop_assert_eq!(format!("{:02x}", parsed.flags), flags);
            // What Alloy propagates is itself accepted by Edge.
            let header = parsed.to_header_value();
            prop_assert!(edge_parse_traceparent(&header).is_some(), "{}", header);
        }
    }

    #[test]
    fn tracestate_normalization_is_stable(value in "[a-z0-9@=,_* \\t-]{0,80}") {
        if let Some(state) = validate_tracestate(&value) {
            prop_assert!(state.as_str().len() <= value.len());
            prop_assert_eq!(validate_tracestate(state.as_str()), Some(state));
        }
    }

    #[test]
    fn request_id_matches_ferrum_edge(value in prop_oneof![
        "[A-Za-z0-9._-]{0,300}",
        "[A-Za-z0-9._ /:+-]{0,40}",
    ]) {
        let accepted = RequestId::parse(&value);
        prop_assert_eq!(accepted.is_some(), edge_accepts_correlation_id(&value));
        prop_assert!(value.len() <= MAX_REQUEST_ID_BYTES || accepted.is_none());
        if let Some(id) = accepted {
            prop_assert_eq!(id.as_str(), value.as_str());
        }
    }

    #[test]
    fn untrusted_header_parsers_never_panic(value in any::<String>()) {
        let _ = parse_traceparent(&value);
        let _ = validate_tracestate(&value);
        let _ = RequestId::parse(&value);
    }

    #[test]
    fn body_accounting_finalizes_once_with_the_implied_outcome(exchange in exchange()) {
        let metrics = serve(&exchange);
        let total: u64 = BODY_OUTCOMES.iter().map(|l| metrics.body_outcomes.get(l)).sum();
        prop_assert_eq!(total, 1, "{:?}", exchange);
        let expected = expected_outcome(&exchange);
        prop_assert_eq!(metrics.body_outcomes.get(expected), 1, "{:?}", exchange);
        prop_assert_eq!(metrics.in_flight(), 0);
    }
}
