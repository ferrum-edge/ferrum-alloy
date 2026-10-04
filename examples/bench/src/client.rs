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
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
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
    async fn ready(&mut self) -> Result<(), Failure> {
        match self {
            Self::H1(sender) => sender.ready().await?,
            Self::H2(sender) => sender.ready().await?,
        }
        Ok(())
    }

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

/// Preparation is outside both warm-up and measurement. A successful
/// workload exchange proves more than a bound listener or H2's `ready()`,
/// which only checks whether its dispatcher is closed.
async fn prepare(
    target: &Target,
    mut sender: Sender,
    totals: &mut Totals,
) -> Result<Sender, Failure> {
    exchange(&mut sender, target).await?;
    if target.workload == Workload::Cancel && !target.transport.http2() {
        drop(sender);
        sender = dial(target).await?;
        totals.connects += 1;
    }
    sender.ready().await?;
    Ok(sender)
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Preparing,
    Warmup,
    Draining,
    Measuring { start: Instant, end: Instant },
}

impl Phase {
    fn contains(self, begin: Instant, finish: Instant) -> bool {
        match self {
            Self::Measuring { start, end } => begin >= start && finish <= end,
            Self::Preparing | Self::Warmup | Self::Draining => false,
        }
    }
}

/// Restore a usable connection before sending another request. At the
/// warm-up boundary this also keeps HTTP/1 cancellation reconnects outside
/// the measurement window.
async fn connected(
    target: &Target,
    sender: Option<Sender>,
    totals: &mut Totals,
) -> Result<Sender, Failure> {
    match sender {
        Some(sender) if !sender.is_closed() => Ok(sender),
        _ => {
            let sender = dial(target).await?;
            totals.connects += 1;
            Ok(sender)
        }
    }
}

/// One closed-loop worker. It re-dials when its connection closes, and on
/// HTTP/1.1 after every cancelled request, since a connection whose
/// response was abandoned cannot be reused.
async fn worker(
    target: Arc<Target>,
    sender: Sender,
    mut phase: watch::Receiver<Phase>,
    ready: mpsc::UnboundedSender<Result<(), String>>,
) -> Result<Totals, Failure> {
    let mut totals = Totals::default();
    let prepared = prepare(&target, sender, &mut totals).await;
    ready.send(prepared.as_ref().map(|_| ()).map_err(ToString::to_string))?;
    let mut sender = Some(prepared?);
    let reuse = !(target.workload == Workload::Cancel && !target.transport.http2());
    loop {
        let current_phase = *phase.borrow_and_update();
        match current_phase {
            Phase::Preparing => {
                phase.changed().await?;
                continue;
            }
            Phase::Draining => {
                // The previous exchange has finished and its response body
                // has been dropped. Park every worker here before the
                // coordinator takes the baseline or starts the clock.
                let connection = async {
                    let mut connection = connected(&target, sender.take(), &mut totals).await?;
                    connection.ready().await?;
                    Ok::<_, Failure>(connection)
                }
                .await;
                ready.send(connection.as_ref().map(|_| ()).map_err(ToString::to_string))?;
                sender = Some(connection?);
                phase.changed().await?;
                continue;
            }
            Phase::Measuring { end, .. } if Instant::now() >= end => break,
            Phase::Warmup | Phase::Measuring { .. } => {}
        }
        let mut current = match connected(&target, sender.take(), &mut totals).await {
            Ok(current) => current,
            Err(error) => {
                let now = Instant::now();
                if phase.borrow().contains(now, now) {
                    totals.error(&error);
                }
                continue;
            }
        };
        let begin = Instant::now();
        let result = exchange(&mut current, &target).await;
        let end = Instant::now();
        if phase.borrow().contains(begin, end) {
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
    Ok(totals)
}

/// Stop warm-up load and rendezvous between exchanges. A watch update alone
/// cannot release measured load: every worker may still be awaiting a warm-up
/// response's first frame for the entire requested measurement duration.
async fn measurement_window<S>(
    load: Load,
    phase: &watch::Sender<Phase>,
    readiness: &mut mpsc::UnboundedReceiver<Result<(), String>>,
    probe: impl Fn() -> S,
) -> Result<(S, S), Failure> {
    if !load.warmup.is_zero() {
        phase.send_replace(Phase::Warmup);
        tokio::time::sleep(load.warmup).await;
    }
    phase.send_replace(Phase::Draining);
    for _ in 0..load.concurrency {
        readiness
            .recv()
            .await
            .ok_or("worker exited before reaching the measurement boundary")?
            .map_err(|error| format!("worker measurement boundary: {error}"))?;
    }
    // Baseline probes and unfinished warm-up exchanges cannot consume the
    // requested window. All workers start their next exchange after release.
    let start = probe();
    let window_start = Instant::now();
    let window_end = window_start + load.duration;
    phase.send_replace(Phase::Measuring {
        start: window_start,
        end: window_end,
    });
    tokio::time::sleep_until(window_end.into()).await;
    Ok((start, probe()))
}

/// Opens the connections and waits for every worker to complete a workload
/// exchange, then runs the requested warm-up and measurement window.
/// Calls `probe` at the window's start and end.
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
                senders.push(Sender::H2(shared.clone()));
            }
        }
        senders.push(sender);
    }
    let connects = u64::try_from(load.connections(target.transport)).unwrap_or(u64::MAX);

    let (phase, receiver) = watch::channel(Phase::Preparing);
    let (ready, mut readiness) = mpsc::unbounded_channel();
    // Dropping the set aborts the other workers if any startup fails.
    let mut workers = JoinSet::new();
    for sender in senders {
        workers.spawn(worker(
            Arc::clone(&target),
            sender,
            receiver.clone(),
            ready.clone(),
        ));
    }
    drop(ready);
    drop(receiver);
    for _ in 0..load.concurrency {
        readiness
            .recv()
            .await
            .ok_or("worker exited before completing a startup request")?
            .map_err(|error| format!("worker startup: {error}"))?;
    }

    let (start, end) = measurement_window(load, &phase, &mut readiness, probe).await?;

    let mut totals = Totals {
        connects,
        ..Totals::default()
    };
    while let Some(worker) = workers.join_next().await {
        totals.merge(worker??);
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
    #![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use hyper::Response;
    use hyper::body::Frame;
    use hyper::service::service_fn;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;
    use crate::dims::FRAME_BYTES;

    /// A first frame controlled by the test, followed by a body that never
    /// ends. Dropping it acknowledges that the client cancelled the stream.
    struct GatedBody {
        release: Option<oneshot::Receiver<()>>,
        cancelled: Option<oneshot::Sender<()>>,
        data: Bytes,
        sent: bool,
    }

    impl hyper::body::Body for GatedBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            if self.sent {
                return Poll::Pending;
            }
            if let Some(release) = &mut self.release
                && Pin::new(release).poll(cx).is_pending()
            {
                return Poll::Pending;
            }
            self.sent = true;
            Poll::Ready(Some(Ok(Frame::data(self.data.clone()))))
        }
    }

    impl Drop for GatedBody {
        fn drop(&mut self) {
            if let Some(cancelled) = self.cancelled.take() {
                let _ = cancelled.send(());
            }
        }
    }

    #[tokio::test]
    async fn measurement_waits_for_every_workers_first_cancelled_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target {
            addr: listener.local_addr().unwrap(),
            transport: Transport::H2c,
            workload: Workload::Cancel,
            tls: None,
        };
        let mut load = load(4, 2);
        load.duration = Duration::from_millis(200);
        let (gates, mut startup) = mpsc::unbounded_channel();
        let requests = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let gates = gates.clone();
                let requests = Arc::clone(&requests);
                let service = service_fn(move |_| {
                    let (release, cancelled) = if requests.fetch_add(1, Ordering::SeqCst) < 4 {
                        let (release, receiver) = oneshot::channel();
                        let (cancelled, acknowledgement) = oneshot::channel();
                        gates.send((release, acknowledgement)).unwrap();
                        (Some(receiver), Some(cancelled))
                    } else {
                        (None, None)
                    };
                    std::future::ready(Ok::<_, Infallible>(Response::new(GatedBody {
                        release,
                        cancelled,
                        data: Bytes::from_static(&[b'x'; FRAME_BYTES]),
                        sent: false,
                    })))
                });
                connections.spawn(async move {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let probes = Arc::new(AtomicUsize::new(0));
        let samples = Arc::clone(&probes);
        let measured = tokio::spawn(async move {
            drive(target, load, || samples.fetch_add(1, Ordering::SeqCst)).await
        });
        let mut blocked = Vec::new();
        for _ in 0..load.concurrency {
            blocked.push(startup.recv().await.unwrap());
        }
        // All workers reached response headers, but none read their first
        // frame. The old deadline-before-spawn code already took its baseline.
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        let (last, cancelled) = blocked.pop().unwrap();
        for (release, cancelled) in blocked {
            release.send(()).unwrap();
            cancelled.await.unwrap();
        }
        // One unready worker must keep the entire measurement behind the gate.
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert!(!measured.is_finished());
        last.send(()).unwrap();
        cancelled.await.unwrap();
        let measured = measured.await.unwrap().unwrap();
        assert_eq!((measured.start, measured.end), (0, 1));
        assert_eq!(measured.window, Duration::from_millis(200));
        assert!(!measured.totals.latencies_us.is_empty());
        assert_eq!(measured.totals.errors, 0);
        assert!(measured.totals.error_samples.is_empty());
        assert_eq!(measured.totals.connects, 2);
        // Preparation bytes must not be counted as measured cancellations.
        let requests = measured.totals.latencies_us.len() as u64;
        assert_eq!(measured.totals.body_bytes, requests * FRAME_BYTES as u64);
        server.abort();
    }

    #[tokio::test]
    async fn measurement_drains_every_warmup_cancellation_before_starting_the_clock() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Arc::new(Target {
            addr: listener.local_addr().unwrap(),
            transport: Transport::H2c,
            workload: Workload::Cancel,
            tls: None,
        });
        let mut load = load(4, 2);
        load.warmup = Duration::from_millis(100);
        load.duration = Duration::from_millis(200);
        let (gates, mut warmup) = mpsc::unbounded_channel();
        let (arrivals, mut measured_requests) = mpsc::unbounded_channel();
        let requests = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let gates = gates.clone();
                let arrivals = arrivals.clone();
                let requests = Arc::clone(&requests);
                let service = service_fn(move |_| {
                    let index = requests.fetch_add(1, Ordering::SeqCst);
                    let (release, cancelled, bytes) = if index < 4 {
                        (None, None, 3 * FRAME_BYTES)
                    } else if index < 8 {
                        let (release, receiver) = oneshot::channel();
                        let (cancelled, acknowledgement) = oneshot::channel();
                        gates.send((release, acknowledgement)).unwrap();
                        (Some(receiver), Some(cancelled), 2 * FRAME_BYTES)
                    } else {
                        let _ = arrivals.send(Instant::now());
                        (None, None, FRAME_BYTES)
                    };
                    std::future::ready(Ok::<_, Infallible>(Response::new(GatedBody {
                        release,
                        cancelled,
                        data: Bytes::from(vec![b'x'; bytes]),
                        sent: false,
                    })))
                });
                connections.spawn(async move {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let (phase, receiver) = watch::channel(Phase::Preparing);
        let mut observed = receiver.clone();
        let (ready, mut readiness) = mpsc::unbounded_channel();
        let mut workers = JoinSet::new();
        for _ in 0..load.connections(target.transport) {
            let Sender::H2(sender) = dial(&target).await.unwrap() else {
                unreachable!("the fixture uses HTTP/2");
            };
            for _ in 0..load.streams {
                workers.spawn(worker(
                    Arc::clone(&target),
                    Sender::H2(sender.clone()),
                    receiver.clone(),
                    ready.clone(),
                ));
            }
        }
        drop(ready);
        drop(receiver);
        for _ in 0..load.concurrency {
            readiness.recv().await.unwrap().unwrap();
        }
        // Prime all four warm-up streams behind first-frame gates before
        // running the real coordinator's 100 ms warm-up timer. This fixture
        // depends on workload progress, rather than a scheduling delay.
        phase.send_replace(Phase::Warmup);
        let mut blocked = Vec::new();
        for _ in 0..load.concurrency {
            blocked.push(warmup.recv().await.unwrap());
        }
        observed.borrow_and_update();
        let probes = Arc::new(AtomicUsize::new(0));
        let samples = Arc::clone(&probes);
        let warmup_start = Instant::now();
        let measured = tokio::spawn(async move {
            let probe = || samples.fetch_add(1, Ordering::SeqCst);
            let samples = measurement_window(load, &phase, &mut readiness, probe)
                .await
                .unwrap();
            let mut totals = Totals {
                connects: 2,
                ..Totals::default()
            };
            while let Some(worker) = workers.join_next().await {
                totals.merge(worker.unwrap().unwrap());
            }
            (samples, totals)
        });
        let boundary = loop {
            observed.changed().await.unwrap();
            let current = *observed.borrow_and_update();
            if !matches!(current, Phase::Warmup) {
                break current;
            }
        };
        // An immediate Warmup -> Measuring transition fails here even though
        // all connections and startup exchanges succeeded.
        assert!(matches!(boundary, Phase::Draining));
        assert!(warmup_start.elapsed() >= load.warmup);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        let (last, cancelled) = blocked.pop().unwrap();
        for (release, cancelled) in blocked {
            release.send(()).unwrap();
            cancelled.await.unwrap();
        }
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert!(!measured.is_finished());
        assert!(matches!(*observed.borrow(), Phase::Draining));
        last.send(()).unwrap();
        cancelled.await.unwrap();
        observed.changed().await.unwrap();
        let window = *observed.borrow_and_update();
        let Phase::Measuring { start, end } = window else {
            unreachable!("the drained workers must be released into measurement");
        };
        assert_eq!(end.duration_since(start), Duration::from_millis(200));
        let arrival = measured_requests.recv().await.unwrap();
        assert!(arrival > start && arrival < end);
        let ((start, end), totals) = measured.await.unwrap();
        assert_eq!((start, end), (0, 1));
        assert_eq!(load.warmup, Duration::from_millis(100));
        assert!(!totals.latencies_us.is_empty());
        assert!(totals.latencies_us.iter().all(|latency| *latency > 0));
        assert_eq!(totals.errors, 0);
        assert!(totals.error_samples.is_empty());
        assert_eq!(totals.connects, 2);
        // Preparation and warm-up frames had different sizes. Counting even
        // one of them as a measured success breaks this exact byte invariant.
        let requests = totals.latencies_us.len() as u64;
        assert_eq!(totals.body_bytes, requests * FRAME_BYTES as u64);
        server.abort();
    }

    #[tokio::test]
    async fn failed_startup_cancellation_returns_the_cause_without_probing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target {
            addr: listener.local_addr().unwrap(),
            transport: Transport::H2c,
            workload: Workload::Cancel,
            tls: None,
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(|_| {
                std::future::ready(Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new())))
            });
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        let probes = AtomicUsize::new(0);
        let result = drive(target, load(2, 2), || probes.fetch_add(1, Ordering::SeqCst)).await;
        let error = result.err().unwrap().to_string();
        assert_eq!(
            error,
            "worker startup: the response ended before the client cancelled it"
        );
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        server.abort();
    }

    fn load(concurrency: usize, streams: usize) -> Load {
        Load {
            concurrency,
            streams,
            warmup: Duration::ZERO,
            duration: Duration::from_secs(1),
        }
    }

    #[test]
    fn measurement_excludes_preparation_warmup_and_boundary_crossing_requests() {
        let start = Instant::now();
        let end = start + Duration::from_millis(200);
        let phase = Phase::Measuring { start, end };
        let tick = Duration::from_nanos(1);
        assert!(!Phase::Preparing.contains(start, end));
        assert!(!Phase::Warmup.contains(start, end));
        assert!(!Phase::Draining.contains(start, end));
        assert!(!phase.contains(start - tick, start + tick));
        assert!(!phase.contains(end - tick, end + tick));
        assert!(phase.contains(start, end));
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
