//! The dimensions of the benchmark matrix: server stack, workload, transport.

use crate::Failure;

/// One value of a matrix dimension, named on the command line and in results.
pub(crate) trait Dimension: Copy + PartialEq + 'static {
    /// What the dimension is called in error messages.
    const KIND: &'static str;
    /// Every value, in matrix order.
    const ALL: &'static [Self];

    /// The command-line and result name.
    fn name(self) -> &'static str;

    fn parse(value: &str) -> Result<Self, Failure> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.name() == value)
            .ok_or_else(|| {
                let expected: Vec<_> = Self::ALL.iter().map(|v| v.name()).collect();
                let expected = expected.join(", ");
                let kind = Self::KIND;
                format!("unknown {kind} {value:?}; expected one of {expected}").into()
            })
    }
}

/// Parses `all` or a comma-separated list, keeping the order given.
pub(crate) fn parse_list<D: Dimension>(value: &str) -> Result<Vec<D>, Failure> {
    if value == "all" {
        return Ok(D::ALL.to_vec());
    }
    let mut list = Vec::new();
    let items = value.split(',').map(str::trim);
    for item in items.filter(|item| !item.is_empty()) {
        let parsed = D::parse(item)?;
        if !list.contains(&parsed) {
            list.push(parsed);
        }
    }
    if list.is_empty() {
        return Err(format!("empty {} list", D::KIND).into());
    }
    Ok(list)
}

/// The server stack under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scenario {
    /// hyper-util's HTTP/1.1 + HTTP/2 server with the same router, no Alloy.
    Plain,
    /// `AlloyApp` defaults, telemetry layer active, no subscriber output.
    Alloy,
    /// As `Alloy`, plus JSON access logs written to a sink.
    AlloyLogs,
    /// OpenTelemetry bridge, every request sampled, exporter discards batches.
    OtelSampled,
    /// OpenTelemetry bridge with sampling ratio 0.
    OtelUnsampled,
    /// Sampled, exporting over OTLP/HTTP to a closed port.
    OtelUnreachable,
    /// Sampled, exporting over OTLP/HTTP to a healthy collector.
    OtelCollector,
}

impl Dimension for Scenario {
    const KIND: &'static str = "scenario";
    const ALL: &'static [Self] = &[
        Self::Plain,
        Self::Alloy,
        Self::AlloyLogs,
        Self::OtelSampled,
        Self::OtelUnsampled,
        Self::OtelUnreachable,
        Self::OtelCollector,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Alloy => "alloy",
            Self::AlloyLogs => "alloy-logs",
            Self::OtelSampled => "otel-sampled",
            Self::OtelUnsampled => "otel-unsampled",
            Self::OtelUnreachable => "otel-unreachable",
            Self::OtelCollector => "otel-collector",
        }
    }
}

impl Scenario {
    /// Whether the scenario runs an OpenTelemetry pipeline.
    pub(crate) fn otel(self) -> bool {
        matches!(
            self,
            Self::OtelSampled | Self::OtelUnsampled | Self::OtelUnreachable | Self::OtelCollector
        )
    }
}

/// Response bodies served by the benchmark router.
pub(crate) const LARGE_BYTES: usize = 64 * 1024;
/// Frame size of the streamed responses.
pub(crate) const FRAME_BYTES: usize = 1024;
/// Frames in a `stream` response.
pub(crate) const STREAM_FRAMES: usize = 64;
/// Frames in the response a `cancel` client abandons: far more than the
/// loopback socket buffers and the HTTP/2 flow-control window hold, so the
/// server is still writing when the client goes away.
pub(crate) const CANCEL_FRAMES: usize = 4096;

/// What each request asks for, and how the client reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Workload {
    /// `GET /small`: a 60-byte JSON object.
    Small,
    /// `GET /large`: 64 KiB with a known length.
    Large,
    /// `GET /stream`: 64 frames of 1 KiB, length unknown (chunked on HTTP/1.1).
    Stream,
    /// `GET /stream-long`, abandoned by the client after the first data frame.
    Cancel,
}

impl Dimension for Workload {
    const KIND: &'static str = "workload";
    const ALL: &'static [Self] = &[Self::Small, Self::Large, Self::Stream, Self::Cancel];

    fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Large => "large",
            Self::Stream => "stream",
            Self::Cancel => "cancel",
        }
    }
}

impl Workload {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Small => "/small",
            Self::Large => "/large",
            Self::Stream => "/stream",
            Self::Cancel => "/stream-long",
        }
    }
}

/// Wire protocol and transport security between client and server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// HTTP/1.1 over plain TCP, keep-alive.
    H1,
    /// HTTP/2 with prior knowledge over plain TCP.
    H2c,
    /// HTTP/1.1 over TLS (ALPN `http/1.1`).
    H1Tls,
    /// HTTP/2 over TLS (ALPN `h2`).
    H2Tls,
    /// HTTP/1.1 over TLS with a verified client certificate.
    H1Mtls,
    /// HTTP/2 over TLS with a verified client certificate.
    H2Mtls,
}

impl Dimension for Transport {
    const KIND: &'static str = "transport";
    const ALL: &'static [Self] = &[
        Self::H1,
        Self::H2c,
        Self::H1Tls,
        Self::H2Tls,
        Self::H1Mtls,
        Self::H2Mtls,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::H1 => "h1",
            Self::H2c => "h2c",
            Self::H1Tls => "h1-tls",
            Self::H2Tls => "h2-tls",
            Self::H1Mtls => "h1-mtls",
            Self::H2Mtls => "h2-mtls",
        }
    }
}

impl Transport {
    pub(crate) fn http2(self) -> bool {
        matches!(self, Self::H2c | Self::H2Tls | Self::H2Mtls)
    }

    pub(crate) fn tls(self) -> bool {
        !matches!(self, Self::H1 | Self::H2c)
    }

    pub(crate) fn mtls(self) -> bool {
        matches!(self, Self::H1Mtls | Self::H2Mtls)
    }

    /// The single ALPN protocol the client offers.
    pub(crate) fn alpn(self) -> &'static [u8] {
        if self.http2() { b"h2" } else { b"http/1.1" }
    }

    pub(crate) fn protocol(self) -> &'static str {
        if self.http2() { "h2" } else { "http/1.1" }
    }
}

/// One point of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    pub(crate) scenario: Scenario,
    pub(crate) workload: Workload,
    pub(crate) transport: Transport,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn names_round_trip() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::parse(scenario.name()).unwrap(), *scenario);
        }
        for workload in Workload::ALL {
            assert_eq!(Workload::parse(workload.name()).unwrap(), *workload);
        }
        for transport in Transport::ALL {
            assert_eq!(Transport::parse(transport.name()).unwrap(), *transport);
        }
    }

    #[test]
    fn lists_parse_all_dedupe_and_reject_unknown() {
        assert_eq!(parse_list::<Transport>("all").unwrap(), Transport::ALL);
        assert_eq!(
            parse_list::<Workload>("large, small,large").unwrap(),
            [Workload::Large, Workload::Small]
        );
        let error = parse_list::<Scenario>("plain,nope").unwrap_err();
        let error = error.to_string();
        assert!(error.contains("unknown scenario \"nope\""), "{error}");
        assert!(parse_list::<Scenario>(",").is_err());
    }

    #[test]
    fn transport_properties() {
        assert!(!Transport::H1.tls() && !Transport::H1.http2());
        assert!(Transport::H2c.http2() && !Transport::H2c.tls());
        let mtls = Transport::H2Mtls;
        assert!(mtls.http2() && mtls.tls() && mtls.mtls());
        assert!(Transport::H1Tls.tls() && !Transport::H1Tls.mtls());
    }
}
