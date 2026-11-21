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

/// [`DURATION_BUCKETS`] in nanoseconds, for integer bucket assignment.
const DURATION_BUCKET_NANOS: [u64; DURATION_BUCKETS.len()] = [
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    75_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    750_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    7_500_000_000,
    10_000_000_000,
];

/// A fixed-bucket histogram. Buckets hold per-bucket (non-cumulative)
/// counts, with one more bucket after the last finite bound for longer
/// observations, so recording touches one bucket; rendering sums them into
/// the cumulative counts Prometheus expects. The count is the sum of every
/// bucket rather than a separate counter, so one snapshot of the buckets
/// yields cumulative buckets, `+Inf`, and `_count` from the same
/// observations even while requests are being recorded. Cache-line aligned
/// so the two histograms in a series (and neighboring series) do not share
/// lines.
#[derive(Debug, Default)]
#[repr(align(64))]
pub struct Histogram {
    buckets: [AtomicU64; DURATION_BUCKETS.len() + 1],
    sum_nanos: AtomicU64,
}

/// One read of a [`Histogram`]'s buckets: per-bucket counts, the last for
/// observations past every finite bound.
struct HistogramSnapshot {
    buckets: [u64; DURATION_BUCKETS.len() + 1],
}

impl HistogramSnapshot {
    fn count(&self) -> u64 {
        self.buckets.iter().copied().fold(0, u64::wrapping_add)
    }
}

impl Histogram {
    /// Records one duration.
    pub fn record(&self, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        // `partition_point` is `DURATION_BUCKETS.len()` past the last bound,
        // which is the overflow bucket.
        let bucket = DURATION_BUCKET_NANOS.partition_point(|&bound| nanos > bound);
        if let Some(bucket) = self.buckets.get(bucket) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Number of recorded values.
    pub fn count(&self) -> u64 {
        self.snapshot().count()
    }

    fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
        }
    }
}

/// Route template -> (method, status) -> series. Keyed by route first so a
/// lookup borrows the route `&str` without allocating.
type SeriesMap = HashMap<Arc<str>, HashMap<(&'static str, u16), Arc<Series>>>;
/// `(method, route, status)` and its series, used when rendering.
type SeriesEntry = ((&'static str, Arc<str>, u16), Arc<Series>);

#[derive(Debug, Default)]
pub(crate) struct Series {
    duration: Histogram,
    time_to_headers: Histogram,
}

impl Series {
    pub(crate) fn record_time_to_headers(&self, elapsed: Duration) {
        self.time_to_headers.record(elapsed);
    }

    pub(crate) fn record_duration(&self, elapsed: Duration) {
        self.duration.record(elapsed);
    }
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

    pub(crate) fn series_for(&self, method: &'static str, route: &str, status: u16) -> Arc<Series> {
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
                let snapshot = histogram.snapshot();
                let count = snapshot.count();
                if count == 0 {
                    continue;
                }
                let labels = format!(
                    "http_request_method=\"{}\",http_route=\"{}\",http_response_status_code=\"{}\"",
                    escape(key.0),
                    escape(&key.1),
                    key.2
                );
                let mut cumulative = 0u64;
                for (bound, bucket) in DURATION_BUCKETS.iter().zip(&snapshot.buckets) {
                    cumulative = cumulative.wrapping_add(*bucket);
                    let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
                }
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

fn escape(value: &str) -> std::borrow::Cow<'_, str> {
    if value.bytes().any(|b| matches!(b, b'\\' | b'"' | b'\n')) {
        std::borrow::Cow::Owned(
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n"),
        )
    } else {
        std::borrow::Cow::Borrowed(value)
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn bucket_nanos_match_the_second_bounds() {
        for (seconds, nanos) in DURATION_BUCKETS.iter().zip(DURATION_BUCKET_NANOS) {
            assert_eq!((seconds * 1e9) as u64, nanos);
        }
    }

    #[test]
    fn rendered_buckets_are_cumulative_with_inclusive_bounds() {
        let metrics = Metrics::default();
        let series = metrics.series_for("GET", "/items/{id}", 200);
        // 3 ms (first bucket), 5 ms (exactly the first bound), 300 ms
        // (the 0.5 bucket), 30 s (past every bound).
        for ms in [3, 5, 300, 30_000] {
            series.record_duration(Duration::from_millis(ms));
        }
        let out = metrics.render_prometheus();
        let bucket = |le: &str| {
            out.lines()
                .find(|l| {
                    l.starts_with("http_server_request_duration_seconds_bucket")
                        && l.contains(&format!("le=\"{le}\""))
                })
                .and_then(|l| l.rsplit(' ').next())
                .unwrap()
                .to_owned()
        };
        assert_eq!(bucket("0.005"), "2", "both sub-5ms observations: {out}");
        assert_eq!(bucket("0.25"), "2");
        assert_eq!(bucket("0.5"), "3");
        assert_eq!(bucket("10"), "3");
        assert_eq!(bucket("+Inf"), "4");
        assert!(out.contains("_count{http_request_method=\"GET\""), "{out}");
    }

    /// Checks every rendered histogram series in one exposition: cumulative
    /// buckets never decrease, `+Inf` equals `_count`, and no finite bucket
    /// exceeds it. Returns the `_count` of each series, by name and labels.
    fn assert_consistent_histograms(out: &str) -> Vec<(String, u64)> {
        let mut counts = Vec::new();
        let mut previous: Option<(String, u64)> = None;
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            let (series, value) = line.rsplit_once(' ').unwrap();
            if let Some((head, le)) = series.split_once(",le=\"") {
                let value: u64 = value.parse().unwrap();
                let key = head.to_owned();
                if let Some((prev_key, prev)) = &previous
                    && *prev_key == key
                {
                    assert!(value >= *prev, "bucket decreased: {line}\n{out}");
                }
                previous = Some((key.clone(), value));
                if le == "+Inf\"}" {
                    let labels = head.split_once('{').unwrap().1;
                    let name = head.split_once("_bucket").unwrap().0;
                    let count_line = format!("{name}_count{{{labels}}} ");
                    let count: u64 = out
                        .lines()
                        .find_map(|l| l.strip_prefix(&count_line))
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert_eq!(value, count, "+Inf != _count for {head}\n{out}");
                    counts.push((format!("{name}{{{labels}"), count));
                }
            } else {
                previous = None;
            }
        }
        counts
    }

    /// The only state a recorder can leave between its two updates is a
    /// bucket that counts an observation whose duration is not yet in
    /// `_sum`. Because the count is the bucket total, a scrape at that moment
    /// still renders consistent buckets, `+Inf`, and `_count` for both
    /// latency families (#316). With the old separate count, the same pause
    /// rendered a finite bucket above `+Inf`.
    #[test]
    fn a_scrape_between_a_recorders_updates_is_consistent() {
        let metrics = Metrics::default();
        let series = metrics.series_for("GET", "/items/{id}", 200);
        for histogram in [&series.duration, &series.time_to_headers] {
            histogram.record(Duration::from_millis(3));
            histogram.record(Duration::from_millis(300));
        }
        let before = assert_consistent_histograms(&metrics.render_prometheus());
        // A recorder paused right after the bucket increment of a 75 ms
        // (finite) and then a 30 s (past every bound) observation.
        for bucket in [4, DURATION_BUCKETS.len()] {
            for histogram in [&series.duration, &series.time_to_headers] {
                histogram.buckets[bucket].fetch_add(1, Ordering::Relaxed);
            }
            let out = metrics.render_prometheus();
            let counts = assert_consistent_histograms(&out);
            assert_eq!(counts.len(), 2, "{out}");
            for ((name, count), (_, earlier)) in counts.iter().zip(&before) {
                let added = if bucket == 4 { 1 } else { 2 };
                assert_eq!(*count, earlier + added, "{name}\n{out}");
                let last_finite = format!("{},le=\"10\"}} ", name.replacen('{', "_bucket{", 1));
                let finite = out
                    .lines()
                    .find(|l| l.starts_with(&last_finite))
                    .and_then(|l| l.rsplit_once(' '))
                    .map(|(_, v)| v.parse::<u64>().unwrap())
                    .unwrap();
                assert_eq!(finite, earlier + 1, "{name}\n{out}");
            }
        }
    }

    /// [`assert_consistent_histograms`], plus: with only finite durations
    /// recorded, the last finite bucket equals `+Inf` (and `_count`).
    fn assert_scrape(out: &str) {
        assert_consistent_histograms(out);
        let mut last_finite = None;
        for line in out.lines() {
            if line.contains(",le=\"10\"} ") {
                last_finite = line.rsplit_once(' ').map(|(_, v)| v.to_owned());
            } else if line.contains(",le=\"+Inf\"} ") {
                let total = line.rsplit_once(' ').map(|(_, v)| v.to_owned());
                assert_eq!(last_finite.take(), total, "{line}\n{out}");
            }
        }
    }

    /// Sets the flag when dropped, so recorder threads stop even when an
    /// assertion fails while they run.
    struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Recording on several threads while scraping repeatedly never renders
    /// a series whose buckets disagree with its count (#316), for both
    /// latency histograms and the overflow series. Only finite durations
    /// are recorded while scraping, so no observation past the last bound
    /// can hide a finite bucket running ahead of the count.
    #[test]
    fn concurrent_recording_and_scraping_render_consistent_histograms() {
        const THREADS: usize = 4;
        const SCRAPES: usize = 500;
        let metrics = Arc::new(Metrics::with_max_series(1));
        let durations = [
            Duration::from_millis(3),
            Duration::from_millis(300),
            Duration::from_secs(10),
        ];
        // Both routes and both families are populated before scraping, and
        // the second route exceeds the one-series cap, so the overflow
        // series is exercised too.
        for route in ["/a", "/b"] {
            let series = metrics.series_for("GET", route, 200);
            series.record_time_to_headers(durations[0]);
            series.record_duration(durations[0]);
        }
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Batches each recorder has completed; scraping starts once every
        // recorder has completed one and ends only after every recorder has
        // completed more, so recording overlaps the scrapes. Waits are
        // bounded, so a recorder that panics fails the test instead of
        // hanging it.
        let progress: Arc<Vec<AtomicU64>> =
            Arc::new((0..THREADS).map(|_| AtomicU64::new(0)).collect());
        let recorders: Vec<_> = (0..THREADS)
            .map(|thread| {
                let metrics = Arc::clone(&metrics);
                let done = Arc::clone(&done);
                let progress = Arc::clone(&progress);
                std::thread::spawn(move || {
                    let route = if thread % 2 == 0 { "/a" } else { "/b" };
                    let series = metrics.series_for("GET", route, 200);
                    let mut recorded = 0u64;
                    while !done.load(Ordering::Relaxed) {
                        for duration in durations {
                            series.record_time_to_headers(duration);
                            series.record_duration(duration);
                            recorded += 1;
                        }
                        progress[thread].fetch_add(1, Ordering::Relaxed);
                    }
                    recorded
                })
            })
            .collect();
        let batches =
            || -> Vec<u64> { progress.iter().map(|p| p.load(Ordering::Relaxed)).collect() };
        let wait_until = |ready: &dyn Fn(&[u64]) -> bool, scrape: bool| -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if ready(&batches()) {
                    return true;
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
                if scrape {
                    assert_scrape(&metrics.render_prometheus());
                } else {
                    std::thread::yield_now();
                }
            }
        };
        let (started, progressed, baseline) = {
            let _stop = StopOnDrop(Arc::clone(&done));
            let started = wait_until(&|b| b.iter().all(|&n| n > 0), false);
            let baseline = batches();
            if started {
                for _ in 0..SCRAPES {
                    assert_scrape(&metrics.render_prometheus());
                }
            }
            let progressed =
                started && wait_until(&|b| b.iter().zip(&baseline).all(|(n, base)| n > base), true);
            assert_scrape(&metrics.render_prometheus());
            (started, progressed, baseline)
        };
        let recorded: u64 = recorders.into_iter().map(|r| r.join().unwrap()).sum();
        assert!(started, "a recorder never completed a batch");
        assert!(
            progressed,
            "a recorder made no progress while scraping: {baseline:?}"
        );
        let counts = assert_consistent_histograms(&metrics.render_prometheus());
        assert_eq!(counts.len(), 4, "{counts:?}");
        let total: u64 = counts.iter().map(|(_, count)| count).sum();
        // Two primed observations per route, one in each family.
        assert_eq!(total, 2 * recorded + 4, "{counts:?}");
    }

    /// Each observation lands in exactly one bucket, so the count is the
    /// bucket total, and one past every bound only reaches `+Inf`.
    #[test]
    fn count_is_the_bucket_total_including_the_overflow_bucket() {
        let histogram = Histogram::default();
        histogram.record(Duration::from_secs(10));
        histogram.record(Duration::from_nanos(10_000_000_001));
        histogram.record(Duration::MAX);
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.buckets[DURATION_BUCKETS.len() - 1], 1);
        assert_eq!(snapshot.buckets[DURATION_BUCKETS.len()], 2);
        assert_eq!(histogram.count(), 3);
    }
}
