//! The JSON log layer writes the same lines as the tracing-subscriber JSON
//! formatter it replaces, escapes every string correctly, and keeps the
//! access-log line format pinned by a snapshot.

#![cfg(feature = "subscriber")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use ferrum_alloy_telemetry::json::JsonLayer;
use ferrum_alloy_telemetry::{TelemetryConfig, TelemetryLayer};
use http::{Request, Response};
use http_body_util::{BodyExt, Empty, Full};
use serde::de::{MapAccess, Visitor};
use serde_json::Value;
use tower::{Layer as _, ServiceExt, service_fn};
use tracing::Subscriber;
use tracing::field::Empty as EmptyField;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(Arc::clone(&self.0))
    }
}

struct FixedTime;

impl FormatTime for FixedTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        w.write_str("2026-09-27T12:00:00.000000Z")
    }
}

/// A subscriber running Alloy's layer and the tracing-subscriber formatter
/// that `init::fmt_layer` used before it, side by side.
fn side_by_side() -> (impl Subscriber + Send + Sync, Capture, Capture) {
    let alloy = Capture::default();
    let upstream = Capture::default();
    let ours = JsonLayer::new()
        .with_timer(FixedTime)
        .with_writer(alloy.clone());
    let theirs = tracing_subscriber::fmt::layer()
        .with_target(true)
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .flatten_event(true)
        .with_timer(FixedTime)
        .with_writer(upstream.clone());
    let subscriber = tracing_subscriber::registry().with(ours).with(theirs);
    (subscriber, alloy, upstream)
}

fn alloy_only() -> (impl Subscriber + Send + Sync, Capture) {
    let alloy = Capture::default();
    let layer = JsonLayer::new()
        .with_timer(FixedTime)
        .with_writer(alloy.clone());
    (tracing_subscriber::registry().with(layer), alloy)
}

/// The single line Alloy's layer writes for one event with `value` as both
/// an event field and a field of the current span.
fn line_with(value: &str) -> String {
    let (subscriber, alloy) = alloy_only();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "t", "s", v = value);
        let _entered = span.enter();
        tracing::info!(target: "t", v = value);
    });
    let text = alloy.text();
    assert_eq!(text.matches('\n').count(), 1, "{text}");
    text
}

#[test]
fn lines_match_the_tracing_subscriber_json_formatter() {
    let (subscriber, alloy, upstream) = side_by_side();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "ferrum_alloy::server", address = "127.0.0.1:8080", "listening");

        let span = tracing::info_span!(
            target: "ferrum_alloy::http",
            parent: None,
            "http.server.request",
            otel.name = %"GET",
            http.route = EmptyField,
            count = 3u64,
            delta = -4i64,
            ratio = 0.25f64,
            ok = true,
            alloy.request_id = %"req-1",
            detail = ?"quote \" and\nnewline",
            raw = &b"\xff\x00\"ok"[..],
            wide = i128::MAX,
            untouched = EmptyField,
        );
        tracing::record_all!(
            span,
            http.route = "/items/{id}",
            count = 4u64,
            ratio = f64::NAN,
            otel.name = "GET /items/{id}",
        );
        span.record("count", 5u64);
        span.record("delta", 7i64);
        {
            let _entered = span.enter();
            tracing::info!(
                status = 200u16,
                route = %"/items/{id}",
                text = "quote \" backslash \\ slash / nul \0 bell \x07 del \x7f é 🚀 \u{2028}",
                "request finished"
            );
            tracing::warn!(
                target: "custom",
                parent: &span,
                big = u64::MAX,
                neg = i64::MIN,
                huge = 1e21,
                tiny = 1.5e-7,
                negative_zero = -0.0f64,
                nan = f64::NAN,
                inf = f64::INFINITY,
                raw = &b"\xff\xfe"[..],
                wide = i128::MIN,
                "explicit parent"
            );
            tracing::error!(parent: None, "explicit root");

            let child = tracing::debug_span!("child", name = "a name", "r#kind" = ?"raw");
            let _child = child.enter();
            tracing::debug!(message = "explicit message field");
            tracing::trace!(flag = false);
        }
        let empty = tracing::info_span!(target: "t", "empty");
        empty.in_scope(|| tracing::info!(target: "t", "in an empty span"));
        tracing::info!(target: "t", "no span");
    });

    let alloy = alloy.text();
    assert_eq!(alloy.lines().count(), 8, "{alloy}");
    assert_eq!(alloy, escape_line_separators(&upstream.text()));
    for line in alloy.lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
}

#[test]
fn every_character_is_escaped_like_serde_json() {
    let all: String = (0..=0x10_FFFFu32).filter_map(char::from_u32).collect();
    let (subscriber, alloy, upstream) = side_by_side();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "t", "s", v = all.as_str());
        let _entered = span.enter();
        tracing::info!(target: "t", v = all.as_str());
        tracing::info!(target: "t", v = %all);
    });
    let alloy = alloy.text();
    let expected = format!(
        "\"v\":{}",
        escape_line_separators(&serde_json::to_string(&all).unwrap())
    );
    // Two event fields, and the span field once per event.
    assert_eq!(alloy.matches(&expected).count(), 4);
    assert_eq!(alloy, escape_line_separators(&upstream.text()));
}

fn escape_line_separators(value: &str) -> String {
    value
        .replace('\u{85}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[test]
fn escapes_control_characters_quotes_and_backslashes() {
    let line = line_with(
        "\u{0}\u{1}\u{8}\t\n\u{b}\u{c}\r\u{1b}\u{1f} \"\\/\u{7f}é🚀\u{85}\u{2028}\u{2029}",
    );
    let expected = r#""v":"\u0000\u0001\b\t\n\u000b\f\r\u001b\u001f \"\\/"#.to_owned()
        + "\u{7f}é🚀\\u0085\\u2028\\u2029\"";
    assert_eq!(line.matches(&expected).count(), 2, "{line}");
    // The only raw newline ends the line.
    assert!(line.ends_with("}}\n"), "{line}");
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["span"]["v"], value["v"]);
}

#[test]
fn escapes_line_separators_next_to_multibyte_characters() {
    let line = line_with("é\u{2028}€\u{85}");
    assert!(line.contains(r#""v":"é\u2028€\u0085""#), "{line}");
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["span"]["v"], "é\u{2028}€\u{85}");
}

#[test]
fn escapes_debug_output_and_field_names() {
    let (subscriber, alloy) = alloy_only();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "t\"arget", { "we\"ird\nname" = ?"a\"b\nc" }, "m\u{1}");
    });
    assert_eq!(
        alloy.text(),
        concat!(
            r#"{"timestamp":"2026-09-27T12:00:00.000000Z","level":"INFO","#,
            r#""message":"m\u0001","we\"ird\nname":"\"a\\\"b\\nc\"","target":"t\"arget"}"#,
            "\n"
        )
    );
}

struct ReentrantDebug;

impl fmt::Debug for ReentrantDebug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        tracing::info!(target: "t", "nested from Debug");
        f.write_str("recorded")
    }
}

#[test]
fn recording_debug_fields_allows_reentrant_events() {
    let (subscriber, alloy) = alloy_only();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "t", "s", field = EmptyField);
        let _entered = span.enter();
        span.record("field", tracing::field::debug(ReentrantDebug));
        tracing::info!(target: "t", "outer");
    });
    let output = alloy.text();
    assert!(output.contains("nested from Debug"), "{output}");
    assert!(output.contains(r#""field":"recorded""#), "{output}");
}

#[test]
fn bytes_that_are_not_utf8_stay_valid_json() {
    let (subscriber, alloy, upstream) = side_by_side();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "t", "s", raw = &b"\xff\x00\"\\"[..]);
        let _entered = span.enter();
        tracing::info!(target: "t", raw = &b"\xff\x00\"\\"[..], empty = &b""[..]);
    });
    let alloy = alloy.text();
    assert!(
        alloy.contains(r#""raw":"[ff 00 22 5c]","empty":"[]""#),
        "{alloy}"
    );
    assert!(
        alloy.contains(r#""span":{"raw":[255,0,34,92],"name":"s"}"#),
        "{alloy}"
    );
    assert_eq!(alloy, upstream.text());
}

#[test]
fn rerecorded_span_fields_replace_earlier_values() {
    let (subscriber, alloy, upstream) = side_by_side();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "t", "s", n = 0u64, text = EmptyField);
        for n in 1..=2_000u64 {
            span.record("n", n);
            span.record("text", format!("value {n} {}", "x".repeat(20)).as_str());
        }
        let _entered = span.enter();
        tracing::info!(target: "t", "done");
    });
    let alloy = alloy.text();
    assert!(
        alloy.contains(r#""span":{"n":2000,"text":"value 2000 xxxxxxxxxxxxxxxxxxxx","name":"s"}"#),
        "{alloy}"
    );
    assert_eq!(alloy, upstream.text());
}

#[test]
fn access_log_lines_match_snapshot() {
    let (subscriber, alloy, upstream) = side_by_side();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "ferrum_alloy::server", address = "127.0.0.1:8080", "listening");

        // The request span and access event exactly as `TelemetryLayer`
        // creates and records them, with fixed values.
        let span = tracing::info_span!(
            target: "ferrum_alloy::http",
            parent: None,
            "http.server.request",
            otel.name = %"GET",
            otel.kind = "server",
            otel.status_code = EmptyField,
            http.request.method = "GET",
            http.route = EmptyField,
            url.scheme = EmptyField,
            url.path = EmptyField,
            client.address = EmptyField,
            user_agent.original = EmptyField,
            network.protocol.version = "1.1",
            http.response.status_code = EmptyField,
            error.type = EmptyField,
            trace_id = EmptyField,
            span_id = EmptyField,
            alloy.request_id = %"req-7f3a",
            alloy.trace.parent = "root",
            alloy.peer.trust = "untrusted",
            alloy.server.time_to_headers_ms = EmptyField,
            alloy.server.body_duration_ms = EmptyField,
            alloy.server.duration_ms = EmptyField,
            alloy.response.body.outcome = EmptyField,
            alloy.response.body.bytes = EmptyField,
            alloy.response.upgraded = EmptyField,
            alloy.admission.wait_ms = EmptyField,
        );
        tracing::record_all!(
            span,
            url.scheme = None::<&str>,
            url.path = Some("/items/42"),
            trace_id = %"4bf92f3577b34da6a3ce929d0e0e4736",
            span_id = %"00f067aa0ba902b7",
        );
        tracing::record_all!(
            span,
            http.response.status_code = 200u16,
            alloy.server.time_to_headers_ms = 1.25f64,
            http.route = Some("/items/{id}"),
            otel.name = Some("GET /items/{id}"),
            otel.status_code = None::<&str>,
        );
        tracing::record_all!(
            span,
            alloy.server.duration_ms = 2.5f64,
            alloy.server.body_duration_ms = Some(1.25f64),
            alloy.response.body.outcome = "completed",
            alloy.response.body.bytes = 60u64,
        );
        tracing::event!(
            target: "ferrum_alloy::access",
            parent: &span,
            tracing::Level::INFO,
            { "http.response.status_code" = 200u16,
              "http.route" = %"/items/{id}",
              "alloy.server.time_to_headers_ms" = Some(1.25f64),
              "alloy.server.duration_ms" = 2.5f64,
              "alloy.response.body.outcome" = "completed",
              "alloy.response.body.bytes" = 60u64,
              "alloy.response.trailers" = false },
            "request finished"
        );
    });

    let rendered = alloy.text();
    assert_eq!(rendered, upstream.text());
    let snapshot_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/json-access-log.expected.txt");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&snapshot_path, &rendered).unwrap();
    }
    let snapshot = std::fs::read_to_string(&snapshot_path).unwrap();
    assert_eq!(rendered, snapshot);
}

/// A JSON object's entries in their written order.
struct Ordered(Vec<(String, Value)>);

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> Visitor<'de> for Entries {
            type Value = Ordered;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, Value>()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }
        deserializer.deserialize_map(Entries)
    }
}

fn ordered(line: &str) -> Vec<(String, Value)> {
    let Ordered(entries) = serde_json::from_str(line).unwrap();
    entries
}

#[tokio::test]
async fn telemetry_layer_access_events_keep_the_schema() {
    let (subscriber, alloy, upstream) = side_by_side();
    let _default = tracing::subscriber::set_default(subscriber);
    let layer = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let service = layer.layer(service_fn(|_request: Request<Empty<Bytes>>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"hello"))))
    }));
    let request = Request::builder()
        .uri("/items/42")
        .body(Empty::new())
        .unwrap();
    let response = service.oneshot(request).await.unwrap();
    let body = response.into_body().collect().await.unwrap();
    assert_eq!(body.to_bytes().len(), 5);

    let ours = access_line(&alloy.text());
    let theirs = access_line(&upstream.text());
    let (ours_event, ours_span) = split_span(&ours);
    let (theirs_event, theirs_span) = split_span(&theirs);
    assert_eq!(keys(&ours_event), keys(&theirs_event));
    assert_eq!(
        keys(&ours_event),
        [
            "timestamp",
            "level",
            "message",
            "http.response.status_code",
            "http.route",
            "alloy.server.time_to_headers_ms",
            "alloy.server.duration_ms",
            "alloy.response.body.outcome",
            "alloy.response.body.bytes",
            "alloy.response.trailers",
            "target",
        ]
    );
    assert_eq!(keys(&ours_span), keys(&theirs_span));
    assert_eq!(keys(&ours_span).last().unwrap(), "name");
    let pairs = ours_event.iter().zip(&theirs_event);
    for ((key, left), (_, right)) in pairs.chain(ours_span.iter().zip(&theirs_span)) {
        assert!(close(left, right), "{key}: {left} vs {right}");
    }
    let line: Value = serde_json::from_str(&ours).unwrap();
    let span = &line["span"];
    assert_eq!(span["name"], "http.server.request");
    assert_eq!(span["alloy.response.body.outcome"], "completed");
    assert_eq!(span["alloy.response.body.bytes"], 5);
    assert_eq!(span["http.response.status_code"], 200);
}

fn access_line(text: &str) -> String {
    text.lines()
        .find(|line| line.contains(r#""target":"ferrum_alloy::access""#))
        .expect("access event")
        .to_owned()
}

type Entries = Vec<(String, Value)>;

/// The event's entries and its span's entries, each in written order.
fn split_span(line: &str) -> (Entries, Entries) {
    let at = line.find(r#","span":{"#).expect("span");
    let event = format!("{}}}", &line[..at]);
    let span = &line[at + r#","span":"#.len()..line.trim_end().len() - 1];
    (ordered(&event), ordered(span))
}

fn keys(entries: &[(String, Value)]) -> Vec<String> {
    entries.iter().map(|(key, _)| key.clone()).collect()
}

/// Equal, allowing the last-digit difference tracing-subscriber's re-parse of
/// span floats can introduce.
fn close(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(l), Some(r)) if left.is_f64() || right.is_f64() => {
            (l - r).abs() <= f64::EPSILON * l.abs().max(r.abs()) * 4.0
        }
        _ => left == right,
    }
}
