//! The load generator: closed-loop workers over explicit hyper connections,
//! so the number of connections and HTTP/2 streams is exactly what was asked
//! for.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Request;
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::Failure;
use crate::dims::{Transport, Workload};

/// Where and how to send load.
pub(crate) struct Target {
    pub(crate) addr: SocketAddr,
    pub(crate) transport: Transport,
    pub(crate) workload: Workload,
    pub(crate) tls: Option<Arc<ClientConfig>>,
}

/// Offered load and measurement window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Load {
    /// Requests in flight at any time.
    pub(crate) concurrency: usize,
    /// Concurrent streams per HTTP/2 connection (ignored for HTTP/1.1).
    pub(crate) streams: usize,
    pub(crate) warmup: Duration,
    pub(crate) duration: Duration,
}

impl Load {
    /// Connections opened up front: one per worker for HTTP/1.1, one per
    /// `streams` workers for HTTP/2.
    pub(crate) fn connections(&self, transport: Transport) -> usize {
        if transport.http2() {
            self.concurrency / self.streams.max(1)
        } else {
            self.concurrency
        }
    }

    pub(crate) fn streams_per_connection(&self, transport: Transport) -> usize {
        if transport.http2() { self.streams } else { 1 }
    }

    pub(crate) fn validate(&self, transport: Transport) -> Result<(), Failure> {
        if self.concurrency == 0 || self.streams == 0 {
            return Err("--concurrency and --streams must be at least 1".into());
        }
        if transport.http2() && !self.concurrency.is_multiple_of(self.streams) {
            let protocol = transport.protocol();
            let message = format!("--concurrency must be a multiple of --streams for {protocol}");
            return Err(message.into());
        }
        if self.duration.is_zero() {
            return Err("--seconds must be greater than 0".into());
        }
        Ok(())
    }
}

/// How many error messages a run keeps, so that a failing run says why.
pub(crate) const ERROR_SAMPLES: usize = 5;

/// What the workers saw inside the measurement window.
#[derive(Debug, Default)]
pub(crate) struct Totals {
    /// Latency of every request that started and finished inside the window.
    pub(crate) latencies_us: Vec<u32>,
    pub(crate) errors: u64,
    /// The first [`ERROR_SAMPLES`] error messages.
    pub(crate) error_samples: Vec<String>,
    /// Connections opened during the whole run, including warm-up.
    pub(crate) connects: u64,
    pub(crate) body_bytes: u64,
}

impl Totals {
    fn error(&mut self, error: &dyn std::fmt::Display) {
        self.errors += 1;
        if self.error_samples.len() < ERROR_SAMPLES {
            self.error_samples.push(error.to_string());
        }
    }

    fn merge(&mut self, other: Self) {
        self.latencies_us.extend(other.latencies_us);
        self.errors += other.errors;
        let room = ERROR_SAMPLES.saturating_sub(self.error_samples.len());
        let samples = other.error_samples.into_iter().take(room);
        self.error_samples.extend(samples);
        self.connects += other.connects;
        self.body_bytes += other.body_bytes;
    }
}

/// A run's totals and the probes taken at the window's start and end.
pub(crate) struct Measured<S> {
    pub(crate) totals: Totals,
    pub(crate) window: Duration,
    pub(crate) start: S,
    pub(crate) end: S,
}

enum Sender {
    H1(http1::SendRequest<Empty<Bytes>>),
    H2(http2::SendRequest<Empty<Bytes>>),
}

impl Sender {
    fn is_closed(&self) -> bool {
        match self {
            Self::H1(sender) => sender.is_closed(),
            Self::H2(sender) => sender.is_closed(),
        }
    }
}

async fn handshake<I>(io: I, h2: bool) -> Result<Sender, Failure>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    if h2 {
        let (sender, connection) = http2::handshake(TokioExecutor::new(), io).await?;
        tokio::spawn(connection);
        Ok(Sender::H2(sender))
    } else {
        let (sender, connection) = http1::handshake(io).await?;
        tokio::spawn(connection);
        Ok(Sender::H1(sender))
    }
}

async fn dial(target: &Target) -> Result<Sender, Failure> {
    let stream = TcpStream::connect(target.addr).await?;
    stream.set_nodelay(true)?;
    let h2 = target.transport.http2();
    if target.workload == Workload::Cancel && !h2 {
        // An abandoned HTTP/1.1 connection is reset, as an aborting client's
        // would be, rather than left in TIME_WAIT to exhaust ephemeral ports.
        stream.set_zero_linger()?;
    }
    match &target.tls {
        None => handshake(TokioIo::new(stream), h2).await,
        Some(config) => {
            let name = ServerName::try_from("localhost")?;
            let stream = TlsConnector::from(Arc::clone(config))
                .connect(name, stream)
                .await?;
            handshake(TokioIo::new(stream), h2).await
        }
    }
}

/// Sends one request and reads the body: all of it, or for `cancel` only
/// until the first data frame, then drops the response.
async fn exchange(sender: &mut Sender, target: &Target) -> Result<u64, Failure> {
    let path = target.workload.path();
    let response = match sender {
        Sender::H1(sender) => {
            let request = Request::get(path)
                .header(http::header::HOST, target.addr.to_string())
                .body(Empty::new())?;
            sender.ready().await?;
            sender.send_request(request).await?
        }
        Sender::H2(sender) => {
            let secure = target.tls.is_some();
            let scheme = if secure { "https" } else { "http" };
            let uri = format!("{scheme}://{}{path}", target.addr);
            let request = Request::get(uri).body(Empty::new())?;
            sender.ready().await?;
            sender.send_request(request).await?
        }
    };
    if !response.status().is_success() {
        return Err(format!("status {}", response.status()).into());
    }
    let cancel = target.workload == Workload::Cancel;
    let mut body = response.into_body();
    let mut bytes = 0_u64;
    while let Some(frame) = body.frame().await {
        if let Some(data) = frame?.data_ref() {
            bytes += data.len() as u64;
            if cancel && !data.is_empty() {
                return Ok(bytes);
            }
        }
    }
    if cancel {
        return Err("the response ended before the client cancelled it".into());
    }
    Ok(bytes)
}

/// One closed-loop worker. It re-dials when its connection closes, and on
/// HTTP/1.1 after every cancelled request, since a connection whose
/// response was abandoned cannot be reused.
async fn worker(
    target: Arc<Target>,
    mut sender: Option<Sender>,
    window_start: Instant,
    window_end: Instant,
) -> Totals {
    let mut totals = Totals::default();
    let reuse = !(target.workload == Workload::Cancel && !target.transport.http2());
    while Instant::now() < window_end {
        let mut current = match sender.take() {
            Some(current) if !current.is_closed() => current,
            _ => match dial(&target).await {
                Ok(dialed) => {
                    totals.connects += 1;
                    dialed
                }
                Err(error) => {
                    if Instant::now() >= window_start {
                        totals.error(&error);
                    }
                    continue;
                }
            },
        };
        let begin = Instant::now();
        let result = exchange(&mut current, &target).await;
        let end = Instant::now();
        if begin >= window_start && end <= window_end {
            match result {
                Ok(bytes) => {
                    let micros = end.duration_since(begin).as_micros();
                    let micros = u32::try_from(micros).unwrap_or(u32::MAX);
                    totals.latencies_us.push(micros);
                    totals.body_bytes += bytes;
                }
                Err(error) => totals.error(&error),
            }
        }
        if reuse {
            sender = Some(current);
        }
    }
    totals
}

/// Opens the connections, runs the workers through warm-up and the
/// measurement window, and calls `probe` at the window's start and end.
/// Only requests that start and finish inside the window are counted.
pub(crate) async fn drive<S>(
    target: Target,
    load: Load,
    probe: impl Fn() -> S,
) -> Result<Measured<S>, Failure> {
    load.validate(target.transport)?;
    let target = Arc::new(target);
    let streams = load.streams_per_connection(target.transport);
    let mut senders = Vec::new();
    for _ in 0..load.connections(target.transport) {
        // Fail fast: a run whose connections cannot be opened measures nothing.
        let sender = dial(&target).await?;
        if let Sender::H2(shared) = &sender {
            for _ in 1..streams {
                senders.push(Some(Sender::H2(shared.clone())));
            }
        }
        senders.push(Some(sender));
    }
    let connects = u64::try_from(load.connections(target.transport)).unwrap_or(u64::MAX);

    let window_start = Instant::now() + load.warmup;
    let window_end = window_start + load.duration;
    let workers: Vec<_> = senders
        .into_iter()
        .map(|sender| {
            let target = Arc::clone(&target);
            tokio::spawn(worker(target, sender, window_start, window_end))
        })
        .collect();

    tokio::time::sleep_until(window_start.into()).await;
    let start = probe();
    tokio::time::sleep_until(window_end.into()).await;
    let end = probe();

    let mut totals = Totals {
        connects,
        ..Totals::default()
    };
    for worker in workers {
        totals.merge(worker.await?);
    }
    Ok(Measured {
        totals,
        window: load.duration,
        start,
        end,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    fn load(concurrency: usize, streams: usize) -> Load {
        Load {
            concurrency,
            streams,
            warmup: Duration::ZERO,
            duration: Duration::from_secs(1),
        }
    }

    #[test]
    fn connections_follow_the_protocol() {
        let load = load(32, 8);
        assert_eq!(load.connections(Transport::H1), 32);
        assert_eq!(load.streams_per_connection(Transport::H1Tls), 1);
        assert_eq!(load.connections(Transport::H2c), 4);
        assert_eq!(load.streams_per_connection(Transport::H2Mtls), 8);
    }

    #[test]
    fn error_samples_keep_the_first_few_messages() {
        let mut first = Totals::default();
        let mut second = Totals::default();
        for index in 0..ERROR_SAMPLES {
            first.error(&format!("first {index}"));
            second.error(&format!("second {index}"));
        }
        first.error(&"dropped");
        assert_eq!(first.errors, ERROR_SAMPLES as u64 + 1);
        assert_eq!(first.error_samples.len(), ERROR_SAMPLES);
        first.error_samples.truncate(2);
        first.merge(second);
        assert_eq!(first.errors, 2 * ERROR_SAMPLES as u64 + 1);
        assert_eq!(first.error_samples.len(), ERROR_SAMPLES);
        assert_eq!(first.error_samples[1], "first 1");
        assert_eq!(first.error_samples[2], "second 0");
    }

    #[test]
    fn validation_rejects_uneven_http2_streams_and_empty_windows() {
        assert!(load(32, 8).validate(Transport::H2c).is_ok());
        assert!(load(30, 8).validate(Transport::H1).is_ok());
        assert!(load(30, 8).validate(Transport::H2Tls).is_err());
        assert!(load(0, 1).validate(Transport::H1).is_err());
        let mut empty = load(1, 1);
        empty.duration = Duration::ZERO;
        assert!(empty.validate(Transport::H1).is_err());
    }
}
