//! Bounded, low-cardinality request metrics with Prometheus text exposition.
//!
//! Labels are limited to the normalized HTTP method, the route *template*
//! (never a raw path), and the status code. The number of distinct series is
//! capped; requests beyond the cap are recorded in a single overflow series so
//! hostile or unexpected input cannot grow memory without bound.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Histogram bucket upper bounds in seconds (OpenTelemetry's recommended
/// `http.server.request.duration` buckets).
pub const DURATION_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Default maximum number of `(method, route, status)` series.
pub const DEFAULT_MAX_SERIES: usize = 2_000;

/// Label used when the series cap is reached.
pub const OVERFLOW_LABEL: &str = "__overflow__";

/// A fixed-bucket histogram.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; DURATION_BUCKETS.len()],
    count: AtomicU64,
    sum_nanos: AtomicU64,
}

impl Histogram {
    /// Records one duration.
    pub fn record(&self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        for (bound, bucket) in DURATION_BUCKETS.iter().zip(&self.buckets) {
            if seconds <= *bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Number of recorded values.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Route template -> (method, status) -> series. Keyed by route first so a
/// lookup borrows the route `&str` without allocating.
type SeriesMap = HashMap<Arc<str>, HashMap<(&'static str, u16), Arc<Series>>>;
/// `(method, route, status)` and its series, used when rendering.
type SeriesEntry = ((&'static str, Arc<str>, u16), Arc<Series>);

#[derive(Debug, Default)]
struct Series {
    duration: Histogram,
    time_to_headers: Histogram,
}

/// A labeled counter family with a fixed, known label set.
#[derive(Debug)]
pub struct LabeledCounter {
    labels: &'static [&'static str],
    values: Vec<AtomicU64>,
}

impl LabeledCounter {
    fn new(labels: &'static [&'static str]) -> Self {
        Self {
            labels,
            values: labels.iter().map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// Increments the counter for `label`. Unknown labels are ignored.
    pub fn inc(&self, label: &str) {
        if let Some(index) = self.labels.iter().position(|l| *l == label) {
            self.values[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Adds `n` to the counter for `label`.
    pub fn add(&self, label: &str, n: u64) {
        if let Some(index) = self.labels.iter().position(|l| *l == label) {
            self.values[index].fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Current value for `label`.
    pub fn get(&self, label: &str) -> u64 {
        self.labels
            .iter()
            .position(|l| *l == label)
            .map_or(0, |index| self.values[index].load(Ordering::Relaxed))
    }

    fn iter(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.labels
            .iter()
            .zip(&self.values)
            .map(|(label, value)| (*label, value.load(Ordering::Relaxed)))
    }
}

/// Response body outcome labels.
pub const BODY_OUTCOMES: &[&str] = &[
    "completed",
    "error",
    "cancelled",
    "not_sent",
    "upgraded",
    "cancelled_before_headers",
    "service_error",
];

/// Trace context decision labels.
pub const TRACE_DECISIONS: &[&str] = &[
    "accepted_remote",
    "root",
    "rerooted_untrusted",
    "rerooted_invalid",
    "ignored_by_policy",
];

/// Request id decision labels.
pub const REQUEST_ID_DECISIONS: &[&str] = &[
    "accepted",
    "generated",
    "replaced_invalid",
    "replaced_untrusted",
];

/// Reasons request-specific response headers were withheld.
pub const SUPPRESSION_REASONS: &[&str] = &["shared_cacheable"];

/// Telemetry loss reasons.
pub const TELEMETRY_LOSS_REASONS: &[&str] =
    &["queue_full", "byte_budget", "export_failed", "shutdown"];

/// The metrics registry.
#[derive(Debug)]
pub struct Metrics {
    series: RwLock<SeriesMap>,
    series_len: std::sync::atomic::AtomicUsize,
    overflow: Arc<Series>,
    max_series: usize,
    in_flight: AtomicI64,
    /// Response body outcomes.
    pub body_outcomes: LabeledCounter,
    /// Trace context decisions.
    pub trace_decisions: LabeledCounter,
    /// Request id decisions.
    pub request_id_decisions: LabeledCounter,
    /// Response headers withheld for cache safety.
    pub header_suppressions: LabeledCounter,
    /// Requests that passed through a second, duplicate telemetry layer.
    pub duplicate_instrumentation: AtomicU64,
    /// Spans lost before export, by reason (OpenTelemetry export only).
    pub telemetry_spans_lost: LabeledCounter,
    /// Spans successfully exported.
    pub telemetry_spans_exported: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::with_max_series(DEFAULT_MAX_SERIES)
    }
}

impl Metrics {
    /// Creates a registry with a custom series cap.
    pub fn with_max_series(max_series: usize) -> Self {
        Self {
            series: RwLock::new(HashMap::new()),
            series_len: std::sync::atomic::AtomicUsize::new(0),
            overflow: Arc::new(Series::default()),
            max_series,
            in_flight: AtomicI64::new(0),
            body_outcomes: LabeledCounter::new(BODY_OUTCOMES),
            trace_decisions: LabeledCounter::new(TRACE_DECISIONS),
            request_id_decisions: LabeledCounter::new(REQUEST_ID_DECISIONS),
            header_suppressions: LabeledCounter::new(SUPPRESSION_REASONS),
            duplicate_instrumentation: AtomicU64::new(0),
            telemetry_spans_lost: LabeledCounter::new(TELEMETRY_LOSS_REASONS),
            telemetry_spans_exported: AtomicU64::new(0),
        }
    }

    pub(crate) fn request_started(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn request_finished(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Requests whose response has not been finalized yet (including
    /// responses whose body is still streaming).
    pub fn in_flight(&self) -> i64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    fn series_for(&self, method: &'static str, route: &str, status: u16) -> Arc<Series> {
        if let Ok(map) = self.series.read()
            && let Some(series) = map.get(route).and_then(|by| by.get(&(method, status)))
        {
            return Arc::clone(series);
        }
        let Ok(mut map) = self.series.write() else {
            return Arc::clone(&self.overflow);
        };
        if let Some(series) = map.get(route).and_then(|by| by.get(&(method, status))) {
            return Arc::clone(series);
        }
        if self.series_len.load(Ordering::Relaxed) >= self.max_series {
            return Arc::clone(&self.overflow);
        }
        let series = Arc::new(Series::default());
        map.entry(Arc::from(route))
            .or_default()
            .insert((method, status), Arc::clone(&series));
        self.series_len.fetch_add(1, Ordering::Relaxed);
        series
    }

    pub(crate) fn record_time_to_headers(
        &self,
        method: &'static str,
        route: &str,
        status: u16,
        elapsed: Duration,
    ) {
        self.series_for(method, route, status)
            .time_to_headers
            .record(elapsed);
    }

    pub(crate) fn record_duration(
        &self,
        method: &'static str,
        route: &str,
        status: u16,
        elapsed: Duration,
    ) {
        self.series_for(method, route, status)
            .duration
            .record(elapsed);
    }

    /// Total finalized requests for one series (tests and diagnostics).
    pub fn request_count(&self, method: &str, route: &str, status: u16) -> u64 {
        let Ok(map) = self.series.read() else {
            return 0;
        };
        map.get(route)
            .and_then(|by| by.iter().find(|((m, s), _)| *m == method && *s == status))
            .map_or(0, |(_, series)| series.duration.count())
    }

    /// Total requests recorded in the overflow series.
    pub fn overflow_count(&self) -> u64 {
        self.overflow.duration.count()
    }

    /// Number of distinct series currently tracked.
    pub fn series_len(&self) -> usize {
        self.series_len.load(Ordering::Relaxed)
    }

    /// Renders every metric in Prometheus text exposition format 0.0.4.
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        let mut entries: Vec<SeriesEntry> = self
            .series
            .read()
            .map(|m| {
                m.iter()
                    .flat_map(|(route, by)| {
                        by.iter().map(move |((method, status), series)| {
                            ((*method, Arc::clone(route), *status), Arc::clone(series))
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| (a.0.0, &*a.0.1, a.0.2).cmp(&(b.0.0, &*b.0.1, b.0.2)));
        let overflow_key: (&'static str, Arc<str>, u16) =
            (OVERFLOW_LABEL, Arc::from(OVERFLOW_LABEL), 0);

        for (name, help, pick) in [
            (
                "http_server_request_duration_seconds",
                "Alloy middleware entry until the response body was finalized (completed, failed, or dropped).",
                (|s: &Series| &s.duration) as fn(&Series) -> &Histogram,
            ),
            (
                "ferrum_alloy_server_time_to_headers_seconds",
                "Alloy middleware entry until the service produced response headers.",
                (|s: &Series| &s.time_to_headers) as fn(&Series) -> &Histogram,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} histogram");
            let rows = entries
                .iter()
                .map(|(k, s)| (k, &**s))
                .chain(std::iter::once((&overflow_key, &*self.overflow)));
            for (key, series) in rows {
                let histogram = pick(series);
                if histogram.count() == 0 {
                    continue;
                }
                let labels = format!(
                    "http_request_method=\"{}\",http_route=\"{}\",http_response_status_code=\"{}\"",
                    escape(key.0),
                    escape(&key.1),
                    key.2
                );
                for (bound, bucket) in DURATION_BUCKETS.iter().zip(&histogram.buckets) {
                    let _ = writeln!(
                        out,
                        "{name}_bucket{{{labels},le=\"{bound}\"}} {}",
                        bucket.load(Ordering::Relaxed)
                    );
                }
                let count = histogram.count();
                let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {count}");
                let sum = histogram.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9;
                let _ = writeln!(out, "{name}_sum{{{labels}}} {sum}");
                let _ = writeln!(out, "{name}_count{{{labels}}} {count}");
            }
        }

        let _ = writeln!(
            out,
            "# HELP http_server_active_requests Requests whose response has not been finalized, including streaming bodies.\n# TYPE http_server_active_requests gauge\nhttp_server_active_requests {}",
            self.in_flight()
        );
        for (name, help, family, label) in [
            (
                "ferrum_alloy_response_body_outcomes_total",
                "Response body finalization outcomes.",
                &self.body_outcomes,
                "outcome",
            ),
            (
                "ferrum_alloy_trace_context_decisions_total",
                "How incoming trace context was handled.",
                &self.trace_decisions,
                "decision",
            ),
            (
                "ferrum_alloy_request_id_decisions_total",
                "How request ids were chosen.",
                &self.request_id_decisions,
                "decision",
            ),
            (
                "ferrum_alloy_response_header_suppressions_total",
                "Request-specific response headers withheld for cache safety.",
                &self.header_suppressions,
                "reason",
            ),
            (
                "ferrum_alloy_telemetry_spans_lost_total",
                "Spans lost before export, by reason.",
                &self.telemetry_spans_lost,
                "reason",
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            for (value, count) in family.iter() {
                let _ = writeln!(out, "{name}{{{label}=\"{value}\"}} {count}");
            }
        }
        let _ = writeln!(
            out,
            "# HELP ferrum_alloy_duplicate_instrumentation_total Requests seen by a second telemetry layer (passed through without double counting).\n# TYPE ferrum_alloy_duplicate_instrumentation_total counter\nferrum_alloy_duplicate_instrumentation_total {}",
            self.duplicate_instrumentation.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP ferrum_alloy_telemetry_spans_exported_total Spans accepted by the exporter.\n# TYPE ferrum_alloy_telemetry_spans_exported_total counter\nferrum_alloy_telemetry_spans_exported_total {}",
            self.telemetry_spans_exported.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP ferrum_alloy_metric_series Distinct request metric series (capped).\n# TYPE ferrum_alloy_metric_series gauge\nferrum_alloy_metric_series {}",
            self.series_len()
        );
        out
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Normalizes a method to one of the nine standard methods or `_OTHER`
/// (OpenTelemetry HTTP semantic conventions).
pub fn method_label(method: &http::Method) -> &'static str {
    match *method {
        http::Method::GET => "GET",
        http::Method::HEAD => "HEAD",
        http::Method::POST => "POST",
        http::Method::PUT => "PUT",
        http::Method::DELETE => "DELETE",
        http::Method::CONNECT => "CONNECT",
        http::Method::OPTIONS => "OPTIONS",
        http::Method::TRACE => "TRACE",
        http::Method::PATCH => "PATCH",
        _ => "_OTHER",
    }
}
