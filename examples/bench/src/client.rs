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
use tokio::task::{JoinError, JoinSet};
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
    fn error(&mut self, error: &(dyn std::error::Error + 'static)) {
        self.errors += 1;
        if self.error_samples.len() < ERROR_SAMPLES {
            let mut message = error.to_string();
            let mut source = error.source();
            while let Some(cause) = source {
                message.push_str(": ");
                message.push_str(&cause.to_string());
                source = cause.source();
            }
            self.error_samples.push(message);
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
        #[cfg(not(test))]
        let executor = TokioExecutor::new();
        #[cfg(test)]
        let executor = tests::DiagnosticExecutor::current();
        let (sender, connection) = http2::handshake(executor, io).await?;
        #[cfg(test)]
        let connection = tests::diagnostic_driver(connection);
        tokio::spawn(connection);
        Ok(Sender::H2(sender))
    } else {
        let (sender, connection) = http1::handshake(io).await?;
        #[cfg(test)]
        let connection = tests::diagnostic_driver(connection);
        tokio::spawn(connection);
        Ok(Sender::H1(sender))
    }
}

async fn dial(target: &Target) -> Result<Sender, Failure> {
    #[cfg(test)]
    tests::diagnostic_dial();
    let stream = TcpStream::connect(target.addr).await?;
    #[cfg(test)]
    tests::diagnostic_socket(stream.local_addr().ok());
    stream.set_nodelay(true)?;
    let h2 = target.transport.http2();
    if target.workload == Workload::Cancel && !h2 {
        // An abandoned HTTP/1.1 connection is reset, as an aborting client's
        // would be, rather than left in TIME_WAIT to exhaust ephemeral ports.
        stream.set_zero_linger()?;
    }
    match &target.tls {
        None => {
            #[cfg(test)]
            tests::diagnostic_worker_stage("http-handshake");
            #[cfg(test)]
            {
                tests::diagnostic_handshake(stream, h2).await
            }
            #[cfg(not(test))]
            {
                handshake(TokioIo::new(stream), h2).await
            }
        }
        Some(config) => {
            let name = ServerName::try_from("localhost")?;
            #[cfg(test)]
            tests::diagnostic_worker_stage("tls-handshake");
            let stream = TlsConnector::from(Arc::clone(config))
                .connect(name, stream)
                .await?;
            #[cfg(test)]
            tests::diagnostic_worker_stage("http-handshake");
            #[cfg(test)]
            {
                tests::diagnostic_handshake(stream, h2).await
            }
            #[cfg(not(test))]
            {
                handshake(TokioIo::new(stream), h2).await
            }
        }
    }
}

/// Sends one request and reads the body: all of it, or for `cancel` only
/// until the first data frame, then drops the response.
async fn exchange(sender: &mut Sender, target: &Target) -> Result<u64, Failure> {
    #[cfg(test)]
    tests::diagnostic_exchange_started(sender);
    let path = target.workload.path();
    let response = match sender {
        Sender::H1(sender) => {
            let request = Request::get(path)
                .header(http::header::HOST, target.addr.to_string())
                .body(Empty::new())?;
            sender.ready().await?;
            #[cfg(test)]
            tests::diagnostic_worker_stage("response-headers");
            sender.send_request(request).await?
        }
        Sender::H2(sender) => {
            let secure = target.tls.is_some();
            let scheme = if secure { "https" } else { "http" };
            let uri = format!("{scheme}://{}{path}", target.addr);
            let request = Request::get(uri).body(Empty::new())?;
            sender.ready().await?;
            #[cfg(test)]
            tests::diagnostic_worker_stage("response-headers");
            sender.send_request(request).await?
        }
    };
    if !response.status().is_success() {
        return Err(format!("status {}", response.status()).into());
    }
    let cancel = target.workload == Workload::Cancel;
    let mut body = response.into_body();
    let mut bytes = 0_u64;
    #[cfg(test)]
    tests::diagnostic_worker_stage("first-data-frame");
    while let Some(frame) = body.frame().await {
        if let Some(data) = frame?.data_ref() {
            bytes += data.len() as u64;
            #[cfg(test)]
            tests::diagnostic_body_bytes(data.len() as u64);
            if cancel && !data.is_empty() {
                #[cfg(test)]
                tests::diagnostic_exchange_completed(sender, bytes);
                return Ok(bytes);
            }
        }
    }
    if cancel {
        #[cfg(test)]
        tests::diagnostic_static_error("response ended before cancellation");
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
    #[cfg(test)]
    tests::diagnostic_sender_stage(&sender, "preparation-sender-ready");
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
    phase: watch::Receiver<Phase>,
    ready: mpsc::UnboundedSender<Result<(), String>>,
) -> Result<Totals, Failure> {
    worker_inner(
        target,
        sender,
        phase,
        ready,
        #[cfg(test)]
        None,
    )
    .await
}

async fn worker_inner(
    target: Arc<Target>,
    sender: Sender,
    mut phase: watch::Receiver<Phase>,
    ready: mpsc::UnboundedSender<Result<(), String>>,
    #[cfg(test)] per_phase: Option<usize>,
) -> Result<Totals, Failure> {
    let mut totals = Totals::default();
    let prepared = prepare(&target, sender, &mut totals).await;
    #[cfg(test)]
    if let Err(error) = &prepared {
        tests::diagnostic_error(error.as_ref());
    }
    ready.send(prepared.as_ref().map(|_| ()).map_err(ToString::to_string))?;
    let mut sender = Some(prepared?);
    let reuse = !(target.workload == Workload::Cancel && !target.transport.http2());
    #[cfg(test)]
    let (mut warmup_exchanges, mut measured_exchanges) = (0, 0);
    loop {
        let current_phase = *phase.borrow_and_update();
        #[cfg(test)]
        tests::diagnostic_worker_phase(current_phase);
        match current_phase {
            Phase::Preparing => {
                #[cfg(test)]
                tests::diagnostic_worker_stage("preparation-parked");
                phase.changed().await?;
                continue;
            }
            Phase::Draining => {
                // The previous exchange has finished and its response body
                // has been dropped. Park every worker here before the
                // coordinator takes the baseline or starts the clock.
                let connection = async {
                    let mut connection = connected(&target, sender.take(), &mut totals).await?;
                    #[cfg(test)]
                    tests::diagnostic_sender_stage(&connection, "drain-sender-ready");
                    connection.ready().await?;
                    Ok::<_, Failure>(connection)
                }
                .await;
                #[cfg(test)]
                if let Err(error) = &connection {
                    tests::diagnostic_error(error.as_ref());
                }
                ready.send(connection.as_ref().map(|_| ()).map_err(ToString::to_string))?;
                sender = Some(connection?);
                #[cfg(test)]
                tests::diagnostic_worker_stage("drain-parked");
                phase.changed().await?;
                continue;
            }
            Phase::Measuring { end, .. } if Instant::now() >= end => break,
            Phase::Warmup | Phase::Measuring { .. } => {}
        }
        #[cfg(test)]
        if let Some(limit) = per_phase {
            match current_phase {
                Phase::Warmup if warmup_exchanges >= limit => {
                    tests::diagnostic_worker_stage("warmup-quota-phase-change");
                    phase.changed().await?;
                    continue;
                }
                Phase::Measuring { end, .. } if measured_exchanges >= limit => {
                    tests::diagnostic_worker_stage("measurement-quota-deadline");
                    tokio::time::sleep_until(end.into()).await;
                    break;
                }
                _ => {}
            }
        }
        let mut current = match connected(&target, sender.take(), &mut totals).await {
            Ok(current) => current,
            Err(error) => {
                #[cfg(test)]
                tests::diagnostic_error(error.as_ref());
                let now = Instant::now();
                if phase.borrow().contains(now, now) {
                    totals.error(error.as_ref());
                }
                #[cfg(test)]
                tests::diagnostic_totals(&totals, true);
                continue;
            }
        };
        let begin = Instant::now();
        let result = exchange(&mut current, &target).await;
        let end = Instant::now();
        #[cfg(test)]
        tests::diagnostic_exchange_window(*phase.borrow(), begin, end, &result);
        #[cfg(test)]
        let failed = result.is_err();
        #[cfg(test)]
        if let Err(error) = &result {
            tests::diagnostic_error(error.as_ref());
        }
        #[cfg(test)]
        match current_phase {
            Phase::Warmup => warmup_exchanges += 1,
            Phase::Measuring { .. } => measured_exchanges += 1,
            _ => {}
        }
        if phase.borrow().contains(begin, end) {
            match result {
                Ok(bytes) => {
                    let micros = end.duration_since(begin).as_micros();
                    let micros = u32::try_from(micros).unwrap_or(u32::MAX);
                    totals.latencies_us.push(micros);
                    totals.body_bytes += bytes;
                }
                Err(error) => totals.error(error.as_ref()),
            }
        }
        #[cfg(test)]
        tests::diagnostic_totals(&totals, failed);
        if reuse {
            sender = Some(current);
        }
    }
    #[cfg(test)]
    if per_phase.is_some() && target.transport.http2() {
        // A quiet tail of the fixed health window must not hide a driver that
        // died on late DATA. Prove reuse after the deadline on the retained
        // sender, without re-dialling or including this probe in the totals.
        tests::diagnostic_reuse();
        let sender = sender.as_mut().ok_or("health probe lost its H2 sender")?;
        exchange(sender, &target).await?;
    }
    Ok(totals)
}

fn premature_worker_exit(
    worker: Option<Result<Result<Totals, Failure>, JoinError>>,
    stage: &str,
) -> Failure {
    let cause = match worker {
        Some(Err(error)) => error.to_string(),
        Some(Ok(Err(error))) => error.to_string(),
        Some(Ok(Ok(_))) => "worker exited before becoming ready".to_owned(),
        None => "all workers exited before becoming ready".to_owned(),
    };
    format!("worker {stage}: {cause}").into()
}

/// A parked peer retains its readiness sender, so channel closure alone
/// cannot reveal a worker that panicked before acknowledging admission.
async fn await_readiness(
    count: usize,
    stage: &str,
    readiness: &mut mpsc::UnboundedReceiver<Result<(), String>>,
    workers: &mut JoinSet<Result<Totals, Failure>>,
) -> Result<(), Failure> {
    #[cfg(test)]
    tests::diagnostic_readiness_start(stage);
    for _ in 0..count {
        tokio::select! {
            biased;
            worker = workers.join_next() => {
                return Err(premature_worker_exit(worker, stage));
            }
            ready = readiness.recv() => {
                match ready {
                    Some(ready) => ready.map_err(|error| format!("worker {stage}: {error}"))?,
                    None => {
                        return Err(premature_worker_exit(workers.join_next().await, stage));
                    }
                }
            }
        }
        #[cfg(test)]
        tests::diagnostic_readiness_received();
    }
    Ok(())
}

/// Stop warm-up load and rendezvous between exchanges. A watch update alone
/// cannot release measured load: every worker may still be awaiting a warm-up
/// response's first frame for the entire requested measurement duration.
async fn measurement_window<S>(
    load: Load,
    phase: &watch::Sender<Phase>,
    readiness: &mut mpsc::UnboundedReceiver<Result<(), String>>,
    workers: &mut JoinSet<Result<Totals, Failure>>,
    probe: impl Fn() -> S,
) -> Result<(S, S), Failure> {
    if !load.warmup.is_zero() {
        #[cfg(test)]
        tests::diagnostic_coordinator_stage("warmup-timer");
        phase.send_replace(Phase::Warmup);
        tokio::select! {
            biased;
            worker = workers.join_next() => {
                return Err(premature_worker_exit(worker, "warm-up"));
            }
            () = tokio::time::sleep(load.warmup) => {}
        }
    }
    phase.send_replace(Phase::Draining);
    await_readiness(load.concurrency, "measurement boundary", readiness, workers).await?;
    // Baseline probes and unfinished warm-up exchanges cannot consume the
    // requested window. All workers start their next exchange after release.
    #[cfg(test)]
    tests::diagnostic_coordinator_stage("baseline-probe");
    let start = probe();
    let window_start = Instant::now();
    let window_end = window_start + load.duration;
    phase.send_replace(Phase::Measuring {
        start: window_start,
        end: window_end,
    });
    #[cfg(test)]
    tests::diagnostic_coordinator_stage("measurement-deadline");
    tokio::time::sleep_until(window_end.into()).await;
    #[cfg(test)]
    tests::diagnostic_coordinator_stage("end-probe");
    Ok((start, probe()))
}

async fn measure_workers<S>(
    load: Load,
    phase: &watch::Sender<Phase>,
    readiness: &mut mpsc::UnboundedReceiver<Result<(), String>>,
    workers: &mut JoinSet<Result<Totals, Failure>>,
    connects: u64,
    probe: impl Fn() -> S,
) -> Result<Measured<S>, Failure> {
    let result = async {
        await_readiness(load.concurrency, "startup", readiness, workers).await?;
        let (start, end) = measurement_window(load, phase, readiness, workers, probe).await?;
        let mut totals = Totals {
            connects,
            ..Totals::default()
        };
        #[cfg(test)]
        tests::diagnostic_coordinator_stage("worker-joins-and-reuse");
        while let Some(worker) = workers.join_next().await {
            #[cfg(test)]
            tests::diagnostic_worker_joined();
            totals.merge(worker??);
        }
        Ok(Measured {
            totals,
            window: load.duration,
            start,
            end,
        })
    }
    .await;
    if result.is_err() {
        // Await cancellation so parked peers have released their senders and
        // connections before the failed run returns.
        #[cfg(test)]
        tests::diagnostic_coordinator_stage("failed-worker-shutdown");
        workers.shutdown().await;
    }
    result
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
    // Dropping the set also aborts workers if the coordinator is cancelled.
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
    measure_workers(load, &phase, &mut readiness, &mut workers, connects, probe).await
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

    use std::convert::Infallible;
    use std::fmt::Write;
    use std::future::Future;
    use std::io::Write as _;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use hyper::Response;
    use hyper::body::Frame;
    use hyper::rt::Executor;
    use hyper::service::service_fn;
    use hyper_util::server::conn::auto::Builder;
    use tokio::net::TcpListener;
    use tokio::runtime::Runtime;
    use tokio::sync::oneshot;
    use tokio_rustls::TlsAcceptor;
    use tracing::instrument::WithSubscriber;
    use tracing::{Dispatch, Event, Metadata, Subscriber};
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
    use tracing_subscriber::{Layer, registry};

    use super::*;
    use crate::dims::FRAME_BYTES;
    use crate::pki::Pki;

    // Task-local observation keeps the real dial/exchange/coordinator path.
    // Cancellation health and controlled boundary fixtures enter these
    // scopes; the production binary has no diagnostic state. No lock crosses an await.
    tokio::task_local! {
        static HEALTH_COORDINATOR: Arc<HealthDiagnostics>;
        static HEALTH_WORKER: (Arc<HealthDiagnostics>, usize);
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct SuccessfulExchanges {
        count: u64,
        bytes: u64,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct ExchangeProgress {
        started: u64,
        completed: u64,
        bytes: u64,
        included: SuccessfulExchanges,
        excluded: SuccessfulExchanges,
    }

    impl ExchangeProgress {
        fn assert_accounted(self) {
            assert_eq!(self.started, self.completed);
            assert_eq!(self.completed, self.included.count + self.excluded.count);
            assert_eq!(self.bytes, self.included.bytes + self.excluded.bytes);
            for progress in [self.included, self.excluded] {
                assert!(progress.bytes >= progress.count);
                if progress.count == 0 {
                    assert_eq!(progress.bytes, 0);
                }
            }
        }
    }

    #[derive(Clone)]
    struct ConnectionDiagnostic {
        owner: usize,
        generation: u64,
        local_addr: Option<SocketAddr>,
        driver: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct WorkerDiagnostic {
        phase: &'static str,
        exchange_phase: usize,
        stage: &'static str,
        since: Instant,
        outcome: &'static str,
        connect_attempts: u64,
        connection: Option<ConnectionDiagnostic>,
        sender_closed: Option<bool>,
        // Preparation, warm-up, measurement, and retained-sender reuse.
        exchanges: [ExchangeProgress; 4],
        measured: usize,
        errors: u64,
        body_bytes: u64,
        last_error_stage: Option<&'static str>,
        error_events: u64,
        error_samples: Vec<ErrorIdentity>,
        wire_events: u64,
        wire_samples: Vec<WireSample>,
    }

    #[derive(Clone)]
    struct CoordinatorDiagnostic {
        stage: &'static str,
        since: Instant,
        readiness: usize,
        joined: usize,
        last_error_stage: Option<&'static str>,
    }

    #[derive(Clone, Copy)]
    struct ConnectionCapture {
        owner: usize,
        generation: u64,
        local_addr: Option<SocketAddr>,
        driver: usize,
    }

    struct WorkerCapture {
        state: WorkerDiagnostic,
        connection: Option<ConnectionCapture>,
    }

    struct HealthCapture {
        sample_start: Instant,
        sample_end: Instant,
        before_health_drop: bool,
        coordinator: CoordinatorDiagnostic,
        workers: [WorkerCapture; 4],
        observer: crate::health::ObserverCapture,
    }

    pub(crate) struct HealthDiagnostics {
        instance: String,
        origin: &'static str,
        pub(crate) observer: crate::health::Observer,
        started: Instant,
        load: Load,
        coordinator: Mutex<CoordinatorDiagnostic>,
        workers: [Mutex<WorkerDiagnostic>; 4],
        worker_changed: [tokio::sync::Notify; 4],
        printed: AtomicBool,
    }

    impl HealthDiagnostics {
        pub(crate) fn new(load: Load) -> Self {
            // The observer has exactly four slots, never a CLI-sized vector.
            assert_eq!(load.concurrency, 4);
            assert_eq!(load.streams, 2);
            let started = Instant::now();
            Self {
                instance: crate::run::new_run_id().unwrap(),
                origin: "controlled-observer",
                observer: crate::health::Observer::default(),
                started,
                load,
                coordinator: Mutex::new(CoordinatorDiagnostic {
                    stage: "load-validation",
                    since: started,
                    readiness: 0,
                    joined: 0,
                    last_error_stage: None,
                }),
                workers: std::array::from_fn(|_| {
                    Mutex::new(WorkerDiagnostic {
                        phase: "preparation",
                        exchange_phase: 0,
                        stage: "not-spawned",
                        since: started,
                        outcome: "not-spawned",
                        connect_attempts: 0,
                        connection: None,
                        sender_closed: None,
                        exchanges: [ExchangeProgress::default(); 4],
                        measured: 0,
                        errors: 0,
                        body_bytes: 0,
                        last_error_stage: None,
                        error_events: 0,
                        error_samples: Vec::new(),
                        wire_events: 0,
                        wire_samples: Vec::new(),
                    })
                }),
                printed: AtomicBool::new(false),
                worker_changed: std::array::from_fn(|_| tokio::sync::Notify::new()),
            }
        }

        pub(crate) fn with_origin(mut self, origin: &'static str) -> Self {
            self.origin = origin;
            self
        }

        pub(crate) fn coordinator_stage(&self, stage: &'static str) {
            let mut state = self.coordinator.lock().unwrap_or_else(|e| e.into_inner());
            if stage == "failed-worker-shutdown" {
                state.last_error_stage = Some(state.stage);
            }
            state.stage = stage;
            state.since = Instant::now();
        }

        fn worker(&self, index: usize, update: impl FnOnce(&mut WorkerDiagnostic)) {
            let mut state = self.workers[index]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            update(&mut state);
        }

        async fn wait_stage(&self, index: usize, stage: &'static str) {
            loop {
                let changed = self.worker_changed[index].notified();
                let mut reached = false;
                self.worker(index, |state| reached = state.stage == stage);
                if reached {
                    return;
                }
                changed.await;
            }
        }

        fn assert_accounting(&self) -> (u64, u64) {
            let mut included = SuccessfulExchanges::default();
            for index in 0..4 {
                self.worker(index, |state| {
                    assert_eq!(state.outcome, "completed-ok");
                    for (phase, progress) in state.exchanges.iter().enumerate() {
                        progress.assert_accounted();
                        if phase != 2 {
                            assert_eq!(progress.included.count, 0);
                            assert_eq!(progress.included.bytes, 0);
                        }
                    }
                    let measurement = state.exchanges[2];
                    assert_eq!(state.measured as u64, measurement.included.count);
                    assert_eq!(state.body_bytes, measurement.included.bytes);
                    assert_eq!(state.errors, 0);
                    assert_eq!(state.error_events, 0);
                    assert!(state.error_samples.is_empty());
                    assert_eq!(state.last_error_stage, None);
                    included.count += measurement.included.count;
                    included.bytes += measurement.included.bytes;
                });
            }
            (included.count, included.bytes)
        }

        pub(crate) fn assert_warmed_progress(&self, transport: Transport) -> (u64, u64) {
            let coordinator = self.coordinator.lock().unwrap();
            assert_eq!(coordinator.stage, "worker-joins-and-reuse");
            assert_eq!(coordinator.readiness, 4);
            assert_eq!(coordinator.joined, 4);
            drop(coordinator);
            let mut sockets = [None, None];
            let mut resets = [0, 0];
            for index in 0..4 {
                self.worker(index, |state| {
                    assert_eq!(state.outcome, "completed-ok");
                    let [preparation, warmup, measurement, reuse] = state.exchanges;
                    assert_eq!(preparation.started, 1);
                    assert!(warmup.started > 0);
                    assert!(warmup.started <= HEALTH_EXCHANGES_PER_PHASE as u64);
                    assert!(measurement.started <= HEALTH_EXCHANGES_PER_PHASE as u64);
                    // A closed-loop worker can have at most one successful
                    // exchange finish beyond the fixed measurement deadline.
                    assert!(measurement.excluded.count <= 1);
                    assert_eq!(reuse.started, u64::from(transport.http2()));
                    for progress in state.exchanges {
                        assert!(progress.bytes <= progress.completed * FRAME_BYTES as u64);
                    }
                    if transport.http2() {
                        let connection = state.connection.as_ref().unwrap();
                        assert_eq!(connection.owner, (index / 2) * 2);
                        assert_eq!(connection.generation, 1);
                        let expected_dials = u64::from(index.is_multiple_of(2));
                        assert_eq!(state.connect_attempts, expected_dials);
                        assert_eq!(state.sender_closed, Some(false));
                        let socket = connection.local_addr.unwrap();
                        let retained = &mut sockets[index / 2];
                        if let Some(original) = retained {
                            assert_eq!(*original, socket);
                        } else {
                            *retained = Some(socket);
                        }
                        resets[index / 2] += state.exchanges.iter().map(|p| p.started).sum::<u64>();
                    }
                });
            }
            if transport.http2() {
                assert_ne!(sockets[0], sockets[1]);
                // Includes both workers' preparation, warm-up, measurement
                // and retained-sender probes, even if no reset state expires.
                for count in resets {
                    assert!(count <= 36);
                    assert!(count < 50);
                }
            }
            // Reconcile every successful phase byte, including any late
            // measurement completion, with the actual returned worker totals.
            let included = self.assert_accounting();
            assert!(included.0 > 0);
            included
        }

        fn capture(&self) -> HealthCapture {
            let sample_start = Instant::now();
            let coordinator = self
                .coordinator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let workers = std::array::from_fn(|index| {
                let mut state = self.workers[index]
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                // Remove all live atomic/Arc references from the frozen state.
                let connection = state.connection.take().map(|connection| ConnectionCapture {
                    owner: connection.owner,
                    generation: connection.generation,
                    local_addr: connection.local_addr,
                    driver: connection.driver.load(Ordering::Relaxed),
                });
                WorkerCapture { state, connection }
            });
            let observer = self.observer.capture();
            HealthCapture {
                sample_start,
                sample_end: Instant::now(),
                before_health_drop: false,
                coordinator,
                workers,
                observer,
            }
        }

        pub(crate) fn failure(&self, cell: crate::dims::Cell, reason: &'static str) {
            if self.printed.load(Ordering::Relaxed) {
                return;
            }
            self.failure_capture(cell, reason, self.capture());
        }

        fn render(
            &self,
            cell: crate::dims::Cell,
            reason: &'static str,
            capture: &HealthCapture,
        ) -> String {
            use crate::dims::Dimension;

            let now = capture.sample_end;
            let state = &capture.coordinator;
            let wire_budget = if cell.transport.http2() {
                DIAGNOSTIC_WIRE_BYTES
            } else {
                DIAGNOSTIC_WIRE_EMPTY_BYTES
            };
            let mut message =
                SnapshotText::new(DIAGNOSTIC_SNAPSHOT_BYTES - wire_budget - DIAGNOSTIC_LOSS_BYTES);
            let feature = match std::env::var("ALLOY_BENCH_DIAGNOSTIC_FEATURE").as_deref() {
                Ok("all-features") => "all-features",
                Ok("default-features") => "default-features",
                _ => "unspecified-features",
            };
            let _ = writeln!(
                message,
                "instance={} origin={} feature={feature}",
                self.instance, self.origin,
            );
            let _ = writeln!(
                message,
                "sample_start_us={} sample_end_us={} interval_us={} atomic=false \
                 ages_to=sample_end before_health_future_drop={}",
                capture
                    .sample_start
                    .duration_since(self.started)
                    .as_micros(),
                now.duration_since(self.started).as_micros(),
                now.duration_since(capture.sample_start).as_micros(),
                capture.before_health_drop,
            );
            let _ = writeln!(
                message,
                "cancellation-health failure: reason={reason} scenario={} workload={} \
                 transport={} elapsed_ms={} concurrency={} streams={} warmup_ms={} \
                 window_ms={} per_phase={} timeout_ms=15000 coordinator={} \
                 coordinator_stage_ms={} readiness={}/4 joined={}/4 \
                 coordinator_last_error_stage={:?} \
                 wire_observer=client-plaintext-and-pinned-h2-poll wire_cause_may_be_unknown",
                cell.scenario.name(),
                cell.workload.name(),
                cell.transport.name(),
                now.duration_since(self.started).as_millis(),
                self.load.concurrency,
                self.load.streams,
                self.load.warmup.as_millis(),
                self.load.duration.as_millis(),
                HEALTH_EXCHANGES_PER_PHASE,
                state.stage,
                now.duration_since(state.since).as_millis(),
                state.readiness,
                state.joined,
                state.last_error_stage,
            );
            let _ = writeln!(
                message,
                "exchanges_hex phases=preparation,warmup,measurement,reuse \
                 fields=started/completed/bytes;included-count/bytes;excluded-count/bytes",
            );
            for (index, worker) in capture.workers.iter().enumerate() {
                let state = &worker.state;
                let connection = worker.connection.map(|connection| {
                    let driver = match connection.driver {
                        0 => "not-polled",
                        1 => "running",
                        2 => "public-dispatch-completed-ok-wire-health-unknown",
                        3 => "public-dispatch-completed-error",
                        _ => "future-dropped",
                    };
                    (
                        connection.owner,
                        connection.generation,
                        connection.local_addr,
                        driver,
                    )
                });
                let _ = write!(
                    message,
                    "worker={index} phase={} stage={} stage_ms={} outcome={} \
                     connect_attempts={} connection(owner,generation,local,driver)={connection:?} \
                     sender_closed_last_observed={:?} \
                     measured={} errors={} body_bytes={} last_error_stage={:?} \
                     error_events={} wire_events={} exchanges_hex=",
                    state.phase,
                    state.stage,
                    now.saturating_duration_since(state.since).as_millis(),
                    state.outcome,
                    state.connect_attempts,
                    state.sender_closed,
                    state.measured,
                    state.errors,
                    state.body_bytes,
                    state.last_error_stage,
                    state.error_events,
                    state.wire_events,
                );
                for progress in state.exchanges {
                    let _ = write!(
                        message,
                        " [{:x}/{:x}/{:x};{:x}/{:x};{:x}/{:x}]",
                        progress.started,
                        progress.completed,
                        progress.bytes,
                        progress.included.count,
                        progress.included.bytes,
                        progress.excluded.count,
                        progress.excluded.bytes,
                    );
                }
                let _ = writeln!(message);
            }
            capture.observer.write_required(&mut message, now);
            message.checkpoint();
            for (index, worker) in capture.workers.iter().enumerate() {
                let state = &worker.state;
                let _ = writeln!(
                    message,
                    "worker={index} error_samples={:?} wire_samples={:?}",
                    state.error_samples, state.wire_samples,
                );
            }
            capture.observer.write_history(&mut message, now);
            let mut wire = SnapshotText::new(wire_budget);
            capture.observer.write_wire_core(&mut wire, now);
            wire.checkpoint();
            capture.observer.write_wire_detail(&mut wire, now);
            message.finish(wire)
        }

        fn failure_capture(
            &self,
            cell: crate::dims::Cell,
            reason: &'static str,
            capture: HealthCapture,
        ) {
            use crate::dims::Dimension;

            if self.printed.swap(true, Ordering::Relaxed) {
                return;
            }
            let message = self.render(cell, reason, &capture);
            let feature = match std::env::var("ALLOY_BENCH_DIAGNOSTIC_FEATURE").as_deref() {
                Ok("all-features") => "all-features",
                Ok("default-features") => "default-features",
                _ => "unspecified-features",
            };
            let _ = std::io::stderr().lock().write_all(message.as_bytes());
            // Test-only, opt-in artifact output. One fixed-size snapshot per
            // cell; I/O failure must never replace the Result or unwind.
            if let Some(directory) = std::env::var_os("ALLOY_BENCH_DIAGNOSTIC_DIR") {
                let name = format!(
                    "{feature}-{}-{}-{}-{}.txt",
                    self.origin,
                    cell.scenario.name(),
                    cell.transport.name(),
                    self.instance,
                );
                let path = std::path::PathBuf::from(directory).join(name);
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    let _ = file.write_all(message.as_bytes());
                }
            }
        }
    }

    // Bounds apply while formatting, before a String can grow. Only safe,
    // known error representations are formatted: h2's opaque source Display
    // can include peer GOAWAY debug bytes, and io::Error can wrap arbitrary
    // application text. Neither is dumped, even when it would fit the bound.
    const DIAGNOSTIC_TEXT_BYTES: usize = 256;
    const DIAGNOSTIC_CHAIN_DEPTH: usize = 4;

    const DIAGNOSTIC_SNAPSHOT_BYTES: usize = 64 * 1024;
    const DIAGNOSTIC_WIRE_BYTES: usize = 48 * 1024;
    const DIAGNOSTIC_WIRE_EMPTY_BYTES: usize = 512;
    const DIAGNOSTIC_LOSS_BYTES: usize = 256;

    fn write_bounded(text: &mut String, limit: usize, input: &str) -> std::fmt::Result {
        for character in input.chars() {
            if text.len() + character.len_utf8() > limit {
                return Err(std::fmt::Error);
            }
            text.push(character);
        }
        Ok(())
    }

    struct SnapshotText(String, usize, usize, usize);

    impl SnapshotText {
        fn new(limit: usize) -> Self {
            Self(String::new(), limit, 0, 0)
        }

        fn checkpoint(&mut self) {
            self.3 = self.2;
        }

        fn finish(mut self, wire: Self) -> String {
            self.0.push('\n');
            self.0.push_str(&wire.0);
            let _ = writeln!(
                self.0,
                "\nsnapshot_loss detail_bytes={} wire_bytes={} budget={} wire_reserve={} \
                 compact_ages_cap_us={} required_detail={} required_wire={}",
                self.2,
                wire.2,
                DIAGNOSTIC_SNAPSHOT_BYTES,
                wire.1,
                u64::MAX,
                self.3,
                wire.3,
            );
            self.0
        }
    }

    impl std::fmt::Write for SnapshotText {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let before = self.0.len();
            let _ = write_bounded(&mut self.0, self.1, text);
            self.2 = self.2.saturating_add(text.len() - (self.0.len() - before));
            // Continue formatting after capacity, so all lost bytes are counted.
            Ok(())
        }
    }

    #[test]
    fn snapshot_reserves_wire_state_and_reports_all_formatting_loss() {
        let mut detail = SnapshotText::new(
            DIAGNOSTIC_SNAPSHOT_BYTES - DIAGNOSTIC_WIRE_BYTES - DIAGNOSTIC_LOSS_BYTES,
        );
        let _ = write!(detail, "{}", "é".repeat(DIAGNOSTIC_SNAPSHOT_BYTES));
        let mut wire = SnapshotText::new(DIAGNOSTIC_WIRE_BYTES);
        let observer = crate::health::Observer::default();
        observer.write_wire(&mut wire, Instant::now());
        let _ = write!(wire, "{}", "0".repeat(DIAGNOSTIC_SNAPSHOT_BYTES));
        let detail_loss = detail.2;
        let wire_loss = wire.2;
        let snapshot = detail.finish(wire);
        assert!(snapshot.len() <= DIAGNOSTIC_SNAPSHOT_BYTES);
        assert!(detail_loss > 0 && wire_loss > 0);
        assert!(snapshot.contains("boundary=client-plaintext-I/O"));
        assert!(snapshot.contains(&format!(
            "snapshot_loss detail_bytes={detail_loss} wire_bytes={wire_loss}"
        )));
    }

    #[tokio::test]
    async fn saturated_history_keeps_server_rows_workers_and_pending_tasks() {
        let diagnostics = HealthDiagnostics::new(load(4, 2));
        let other = HealthDiagnostics::new(load(4, 2));
        let socket = Some(
            "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535"
                .parse()
                .unwrap(),
        );
        for _ in 0..72 {
            let request = diagnostics.observer.request(socket).unwrap();
            request.frames.store(u64::MAX, Ordering::Relaxed);
            *request.response.lock().unwrap() = Some(Instant::now());
        }
        let mut pending = Vec::new();
        // Fill every task slot: old completions must not displace pending tasks
        // or any server row. No live transport or causal claim is involved.
        for ordinal in 0..160 {
            let observation = diagnostics
                .observer
                .task(
                    "h2-response-callback-child-empty-get",
                    2,
                    1,
                    socket,
                    ordinal,
                )
                .unwrap();
            if ordinal < 148 {
                let mut task = Box::pin(crate::health::Observed::new(
                    std::future::ready(()),
                    Some(observation),
                ));
                std::future::poll_fn(|cx| task.as_mut().poll(cx)).await;
            } else {
                let mut task = Box::pin(crate::health::Observed::new(
                    std::future::pending::<()>(),
                    Some(observation),
                ));
                std::future::poll_fn(|cx| {
                    assert!(task.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                pending.push(task);
            }
        }
        for index in 0..4 {
            diagnostics.worker(index, |state| {
                state.stage = "response-headers";
                state.outcome = "running";
                state.exchanges = [ExchangeProgress {
                    started: u64::MAX,
                    completed: u64::MAX,
                    bytes: u64::MAX,
                    included: SuccessfulExchanges {
                        count: u64::MAX,
                        bytes: u64::MAX,
                    },
                    excluded: SuccessfulExchanges {
                        count: u64::MAX,
                        bytes: u64::MAX,
                    },
                }; 4];
                for _ in 0..5 {
                    state.error_samples.push(ErrorIdentity {
                        stage: "response-headers",
                        chain: vec!["é".repeat(128); DIAGNOSTIC_CHAIN_DEPTH],
                        source_end: "bounded controlled sample",
                    });
                }
            });
        }
        let cell = crate::dims::Cell {
            scenario: crate::dims::Scenario::Plain,
            workload: Workload::Cancel,
            transport: Transport::H2c,
        };
        let capture = diagnostics.capture();
        let rendered = diagnostics.render(cell, "controlled-saturation", &capture);
        assert!(rendered.len() <= DIAGNOSTIC_SNAPSHOT_BYTES);
        assert_eq!(rendered.matches("server socket_ref=").count(), 72);
        assert_eq!(rendered.matches("stage=response-headers").count(), 4);
        // Required compact task records exclude metadata and optional task history.
        let pending_tasks: Vec<_> = rendered
            .lines()
            .filter(|line| {
                line.starts_with("task=")
                    && line.contains(" polls_hex(polls,inner,pending,ready,wakes)=")
            })
            .collect();
        assert_eq!(pending_tasks.len(), 12);
        for (id, task) in (148..160).zip(pending_tasks) {
            assert!(task.starts_with(&format!("task={id} ")));
            assert!(task.contains(" polls_hex(polls,inner,pending,ready,wakes)=1/1/1/0/0 "));
            assert!(task.contains(" drop=false "));
        }
        assert!(rendered.contains("required_detail=0 required_wire=0"));
        assert!(!rendered.contains("detail_bytes=0 "));
        assert!(rendered.contains("socket_ref=0 socket=Some([ffff:"));
        let other_text = other.render(cell, "controlled-isolation", &other.capture());
        assert!(!other_text.contains(&diagnostics.instance));
        assert!(!other_text.contains("server socket_ref="));
        drop(pending);
        assert_eq!(
            rendered,
            diagnostics.render(cell, "controlled-saturation", &capture)
        );
    }

    #[tokio::test]
    async fn capture_precedes_health_drop_and_joinset_abort_effects() {
        struct CoordinatorDrop(Arc<HealthDiagnostics>);
        impl Drop for CoordinatorDrop {
            fn drop(&mut self) {
                self.0.coordinator_stage("after-future-drop");
            }
        }
        struct ChildDrop(Option<oneshot::Sender<()>>);
        impl Drop for ChildDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let diagnostics = Arc::new(HealthDiagnostics::new(load(4, 2)));
        diagnostics.coordinator_stage("before-future-drop");
        let child_diagnostics = Arc::clone(&diagnostics);
        let coordinator_diagnostics = Arc::clone(&diagnostics);
        let (destroyed, destruction) = oneshot::channel();
        let mut health = Box::pin(async move {
            let _guard = CoordinatorDrop(coordinator_diagnostics);
            let mut children = JoinSet::new();
            children.spawn(diagnostic_worker(child_diagnostics, 0, async move {
                let _guard = ChildDrop(Some(destroyed));
                diagnostic_dial();
                diagnostic_socket(Some("127.0.0.1:12345".parse().unwrap()));
                diagnostic_worker_stage("response-headers");
                diagnostic_driver(std::future::pending::<Result<(), hyper::Error>>()).await?;
                Ok(Totals::default())
            }));
            std::future::pending::<()>().await;
            drop(children);
        });
        std::future::poll_fn(|cx| {
            assert!(health.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::timeout(
            Duration::from_secs(10),
            diagnostics.wait_stage(0, "response-headers"),
        )
        .await
        .unwrap();
        let capture = capture_before_drop(health, &diagnostics);
        assert!(capture.sample_start <= capture.sample_end);
        assert!(capture.before_health_drop);
        assert_eq!(capture.coordinator.stage, "before-future-drop");
        assert_eq!(capture.workers[0].state.outcome, "running");
        assert_eq!(capture.workers[0].connection.unwrap().driver, 1);
        assert!(capture.workers[0].state.connection.is_none());
        let cell = crate::dims::Cell {
            scenario: crate::dims::Scenario::Plain,
            workload: Workload::Cancel,
            transport: Transport::H2c,
        };
        let frozen = diagnostics.render(cell, "controlled-before-drop", &capture);
        tokio::time::timeout(Duration::from_secs(10), destruction)
            .await
            .unwrap()
            .unwrap();
        diagnostics.worker(0, |state| {
            assert_eq!(state.outcome, "future-dropped");
            assert_eq!(
                state
                    .connection
                    .as_ref()
                    .unwrap()
                    .driver
                    .load(Ordering::Relaxed),
                4
            );
        });
        assert_eq!(
            diagnostics.coordinator.lock().unwrap().stage,
            "after-future-drop"
        );
        assert_eq!(
            frozen,
            diagnostics.render(cell, "controlled-before-drop", &capture)
        );
        assert!(frozen.contains("atomic=false"));
        assert!(frozen.contains("before_health_future_drop=true"));
        assert!(!frozen.contains("future-dropped"));
        let after = diagnostics.render(cell, "controlled-after-drop", &diagnostics.capture());
        assert!(after.contains("future-dropped"));
        assert!(after.contains("drop=true"));
    }

    #[test]
    fn snapshot_footer_and_utf8_remain_bounded_at_maximum_loss_counts() {
        let mut detail = SnapshotText::new(
            DIAGNOSTIC_SNAPSHOT_BYTES - DIAGNOSTIC_WIRE_BYTES - DIAGNOSTIC_LOSS_BYTES,
        );
        let mut wire = SnapshotText::new(DIAGNOSTIC_WIRE_BYTES);
        for text in [&mut detail, &mut wire] {
            let _ = write!(text, "{}", "🦀".repeat(DIAGNOSTIC_SNAPSHOT_BYTES));
            text.2 = usize::MAX;
            text.checkpoint();
        }
        let text = detail.finish(wire);
        assert!(text.len() <= DIAGNOSTIC_SNAPSHOT_BYTES);
        assert!(text.contains(&format!("required_wire={}", usize::MAX)));
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn diagnostic_sources_preserve_health_gates_and_test_only_boundary() {
        let client = include_str!("client.rs");
        let timeout = client
            .split("\n    pub(crate) async fn cancellation_health_timeout<S>(")
            .nth(1)
            .unwrap()
            .split("async fn cancellation_health_inner<S>(")
            .next()
            .unwrap();
        assert!(timeout.contains("timeout(Duration::from_secs(15), health.as_mut())"));
        assert!(timeout.contains("let mut health = Box::pin(health);"));
        let helper = timeout.split("fn capture_before_drop<F>(").nth(1).unwrap();
        let sampling = helper.find("diagnostics.capture()").unwrap();
        let teardown = helper.find("drop(health)").unwrap();
        assert!(sampling < teardown);
        let budget = client
            .split("\n    pub(crate) const HEALTH_EXCHANGES_PER_PHASE:")
            .nth(1)
            .unwrap();
        assert!(budget.starts_with(" usize = 8;"));
        let run = include_str!("run.rs");
        let matrix = run
            .split("fn assert_health_matrix(scenario: Scenario)")
            .nth(1)
            .unwrap()
            .split("#[test]")
            .next()
            .unwrap();
        let production = client
            .split("#[cfg(test)]\npub(crate) mod tests")
            .next()
            .unwrap();
        assert!(!production.contains(".reset_stream_duration("));
        assert!(!production.contains(".max_local_error_reset_streams("));
        assert!(!production.contains(".initial_connection_window_size("));
        assert!(!production.contains(".initial_stream_window_size("));
        assert!(run.contains("concurrency: 4,\n                streams: 2,"));
        assert!(matrix.contains("options(5.0)"));
        assert!(matrix.contains("options.load.warmup = Duration::from_secs(1)"));
        assert!(matrix.contains("for transport in Transport::ALL"));
        assert!(matrix.contains("for workload in Workload::ALL"));
        assert!(matrix.contains("assert_eq!(result[\"requests\"], requests"));
        assert!(matrix.contains("assert_eq!(result[\"seconds\"], 5.0"));
        assert!(matrix.contains("assert_eq!(result[\"warmup_seconds\"], 1.0"));
        assert!(matrix.contains("assert_body_accounting(&result, *workload, *transport)"));
        assert!(run.contains("assert_eq!(result[\"errors\"], 0"));
        assert!(include_str!("main.rs").contains("#[cfg(test)]\nmod health;"));
        let health = include_str!("health.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for bound in [
            "const WIRE_CONNECTIONS: usize = 2;",
            "const WIRE_STREAMS: usize = 36;",
            "const WIRE_EVENTS: usize = 64;",
            "const TASK_SLOTS: usize = 160;",
            "const REQUEST_SLOTS: usize = 72;",
        ] {
            assert!(health.contains(bound));
        }
    }

    #[derive(Default)]
    struct BoundedText(String);

    impl std::fmt::Write for BoundedText {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            write_bounded(&mut self.0, DIAGNOSTIC_TEXT_BYTES, text)
        }
    }

    #[derive(Clone, Debug)]
    struct ErrorIdentity {
        stage: &'static str,
        chain: Vec<String>,
        source_end: &'static str,
    }

    impl ErrorIdentity {
        fn new(stage: &'static str, error: &(dyn std::error::Error + 'static)) -> Self {
            let mut chain = Vec::new();
            let mut source = Some(error);
            for _ in 0..DIAGNOSTIC_CHAIN_DEPTH {
                let Some(error) = source else {
                    break;
                };
                let mut text = BoundedText::default();
                if let Some(error) = error.downcast_ref::<hyper::Error>() {
                    // Hyper Display is its static description, not its source.
                    let _ = write!(text, "hyper::Error: {error}");
                } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
                    let _ = write!(
                        text,
                        "io::Error kind={:?} os={:?}; inner text unknown (omitted)",
                        error.kind(),
                        error.raw_os_error(),
                    );
                } else if error.is::<rustls::Error>() {
                    let _ = write!(text, "rustls::Error; detail unknown (omitted)");
                } else {
                    let _ = write!(text, "unknown opaque error; text omitted");
                }
                chain.push(text.0);
                source = error.source();
            }
            Self {
                stage,
                chain,
                source_end: if source.is_some() {
                    "depth-limit; further sources unknown"
                } else {
                    "no further source exposed; inner cause may be unknown"
                },
            }
        }
    }

    pub(super) fn diagnostic_error(error: &(dyn std::error::Error + 'static)) {
        observe_worker(|_, state| {
            let stage = state.stage;
            record_error(state, stage, error);
        });
    }

    pub(super) fn diagnostic_static_error(message: &'static str) {
        observe_worker(|_, state| {
            state.last_error_stage = Some(state.stage);
            state.error_events = state.error_events.saturating_add(1);
            if state.error_samples.len() < ERROR_SAMPLES {
                let mut text = BoundedText::default();
                let _ = write!(text, "worker: {message}");
                state.error_samples.push(ErrorIdentity {
                    stage: state.stage,
                    chain: vec![text.0],
                    source_end: "static worker error; no source exposed",
                });
            }
        });
    }

    fn record_error(
        state: &mut WorkerDiagnostic,
        stage: &'static str,
        error: &(dyn std::error::Error + 'static),
    ) {
        state.last_error_stage = Some(stage);
        state.error_events = state.error_events.saturating_add(1);
        if state.error_samples.len() < ERROR_SAMPLES {
            state.error_samples.push(ErrorIdentity::new(stage, error));
        }
    }

    #[derive(Clone, Debug)]
    struct WireSample {
        generation: u64,
        local_addr: Option<SocketAddr>,
        event: &'static str,
        error: String,
    }

    // These two callsites in pinned h2 0.4.19 describe the actual connection
    // poll's error path. Restrict before recording fields: no frame/header
    // spans, messages, payload debug fields, or general log streams. Hyper's
    // own unstable tracing is disabled and is not enabled for this experiment.
    fn wire_metadata(metadata: &Metadata<'_>) -> bool {
        metadata.is_event()
            && metadata.target() == "h2::proto::connection"
            && *metadata.level() == tracing::Level::DEBUG
            && matches!(metadata.line(), Some(491 | 521))
            && metadata.fields().len() == 2
            && metadata.fields().field("error").is_some()
    }

    struct WireLayer {
        diagnostics: Arc<HealthDiagnostics>,
        connection: ConnectionDiagnostic,
    }

    struct IoKindVisitor(BoundedText);

    impl tracing::field::Visit for IoKindVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "error" {
                let _ = write!(self.0, "{value:?}");
            }
        }
    }

    impl<S: Subscriber> Layer<S> for WireLayer {
        fn on_event(&self, event: &Event<'_>, _context: LayerContext<'_, S>) {
            let io = event.metadata().line() == Some(491);
            self.diagnostics.worker(self.connection.owner, |state| {
                state.wire_events = state.wire_events.saturating_add(1);
                if state.wire_samples.len() >= ERROR_SAMPLES {
                    return;
                }
                let error = if io {
                    // At this exact callsite the field is std::io::ErrorKind,
                    // not io::Error or any peer-controlled debug data.
                    let mut visitor = IoKindVisitor(BoundedText::default());
                    event.record(&mut visitor);
                    let IoKindVisitor(text) = visitor;
                    text.0
                } else {
                    // The protocol field can contain GOAWAY debug bytes.
                    // Do not even ask it to format. Occurrence is evidence;
                    // a typed protocol reason is unavailable to this observer.
                    "unknown; protocol detail omitted (may contain peer bytes)".into()
                };
                state.wire_samples.push(WireSample {
                    generation: self.connection.generation,
                    local_addr: self.connection.local_addr,
                    event: if io {
                        "h2-io-error"
                    } else {
                        "h2-connection-error"
                    },
                    error,
                });
            });
        }
    }

    fn diagnostic_dispatch() -> Option<Dispatch> {
        HEALTH_WORKER
            .try_with(|(diagnostics, index)| {
                let mut connection = None;
                diagnostics.worker(*index, |state| connection = state.connection.clone());
                connection.map(|connection| {
                    Dispatch::new(
                        registry().with(
                            WireLayer {
                                diagnostics: Arc::clone(diagnostics),
                                connection,
                            }
                            .with_filter(filter_fn(wire_metadata)),
                        ),
                    )
                })
            })
            .ok()
            .flatten()
    }

    #[derive(Clone)]
    pub(super) struct DiagnosticExecutor {
        dispatch: Option<Dispatch>,
        observer: Option<(Arc<HealthDiagnostics>, ConnectionDiagnostic)>,
        sequence: Arc<AtomicUsize>,
    }

    impl DiagnosticExecutor {
        pub(super) fn current() -> Self {
            let observer = HEALTH_WORKER
                .try_with(|(diagnostics, index)| {
                    let mut connection = None;
                    diagnostics.worker(*index, |state| connection = state.connection.clone());
                    connection.map(|connection| (Arc::clone(diagnostics), connection))
                })
                .ok()
                .flatten();
            Self {
                dispatch: diagnostic_dispatch(),
                observer,
                sequence: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl<F> Executor<F> for DiagnosticExecutor
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        fn execute(&self, future: F) {
            let mut observation = None;
            let mut gate = None;
            if let Some((diagnostics, connection)) = &self.observer {
                let ordinal = self.sequence.fetch_add(1, Ordering::Relaxed);
                // Pinned Hyper 1.11.1 spawns Task first during handshake.
                // Our body is Empty and requests are ordinary GETs: no Pipe
                // or upgrade child can intervene; subsequent H2ClientFuture
                // children are SendWhen response callbacks. The same enum
                // type represents all variants, so type_name alone is NOT
                // evidence of a wire task. Unexpected types stay unknown.
                let h2 = std::any::type_name::<F>()
                    .starts_with("hyper::proto::h2::client::H2ClientFuture<");
                let kind = match (h2, ordinal) {
                    (true, 0) => "h2-wire-child-pinned-first-spawn",
                    (true, _) => "h2-response-callback-child-empty-get",
                    _ => "executor-child-unknown",
                };
                observation = diagnostics.observer.task(
                    kind,
                    connection.owner,
                    connection.generation,
                    connection.local_addr,
                    ordinal,
                );
                if h2 && ordinal == 0 {
                    gate = diagnostics
                        .observer
                        .wire_gate
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                }
            }
            let future = crate::health::Observed::new(future, observation).gated(gate);
            // Task-locals and thread defaults do not follow tokio::spawn.
            // Carry this connection's dispatch on every executor child poll,
            // including children spawned from children, on any runtime thread.
            match &self.dispatch {
                Some(dispatch) => {
                    TokioExecutor::new().execute(future.with_subscriber(dispatch.clone()));
                }
                None => TokioExecutor::new().execute(future),
            }
        }
    }

    fn observe_worker(update: impl FnOnce(usize, &mut WorkerDiagnostic)) {
        let _ = HEALTH_WORKER.try_with(|(diagnostics, index)| {
            diagnostics.worker(*index, |state| update(*index, state));
        });
    }

    pub(super) fn diagnostic_worker_stage(stage: &'static str) {
        let _ = HEALTH_WORKER.try_with(|(diagnostics, index)| {
            diagnostics.worker(*index, |state| {
                state.stage = stage;
                state.since = Instant::now();
            });
            diagnostics.worker_changed[*index].notify_one();
        });
    }

    pub(super) fn diagnostic_sender_stage(sender: &Sender, stage: &'static str) {
        diagnostic_worker_stage(stage);
        observe_worker(|_, state| state.sender_closed = Some(sender.is_closed()));
    }

    pub(super) fn diagnostic_dial() {
        diagnostic_worker_stage("tcp-connect");
        observe_worker(|_, state| {
            state.connect_attempts = state.connect_attempts.saturating_add(1);
            state.connection = None;
            state.sender_closed = None;
        });
    }

    pub(super) fn diagnostic_socket(local_addr: Option<SocketAddr>) {
        observe_worker(|index, state| {
            state.connection = Some(ConnectionDiagnostic {
                owner: index,
                generation: state.connect_attempts,
                local_addr,
                driver: Arc::new(AtomicUsize::new(0)),
            });
        });
    }

    pub(super) async fn diagnostic_handshake<I>(io: I, h2: bool) -> Result<Sender, Failure>
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let wire = h2
            .then(|| {
                HEALTH_WORKER
                    .try_with(|(diagnostics, index)| {
                        let mut connection = None;
                        diagnostics.worker(*index, |state| connection = state.connection.clone());
                        connection.and_then(|connection| {
                            diagnostics.observer.wire(
                                &diagnostics.instance,
                                connection.owner,
                                connection.generation,
                                connection.local_addr,
                            )
                        })
                    })
                    .ok()
                    .flatten()
            })
            .flatten();
        match wire {
            Some(wire) => handshake(TokioIo::new(crate::health::WireIo::new(io, wire)), h2).await,
            None => handshake(TokioIo::new(io), h2).await,
        }
    }

    pub(super) fn diagnostic_exchange_started(sender: &Sender) {
        diagnostic_sender_stage(sender, "exchange-sender-ready");
        observe_worker(|_, state| {
            let progress = &mut state.exchanges[state.exchange_phase];
            progress.started = progress.started.saturating_add(1);
        });
    }

    pub(super) fn diagnostic_body_bytes(bytes: u64) {
        observe_worker(|_, state| {
            let progress = &mut state.exchanges[state.exchange_phase];
            progress.bytes = progress.bytes.saturating_add(bytes);
        });
    }

    pub(super) fn diagnostic_exchange_completed(sender: &Sender, bytes: u64) {
        diagnostic_sender_stage(sender, "exchange-completed");
        observe_worker(|_, state| {
            let progress = &mut state.exchanges[state.exchange_phase];
            progress.completed = progress.completed.saturating_add(1);
            if matches!(state.exchange_phase, 0 | 3) {
                progress.excluded.count += 1;
                progress.excluded.bytes += bytes;
            }
        });
    }

    pub(super) fn diagnostic_exchange_window(
        phase: Phase,
        begin: Instant,
        finish: Instant,
        result: &Result<u64, Failure>,
    ) {
        let Ok(bytes) = result else {
            return;
        };
        // Compare raw window bounds and timestamps independently of
        // Phase::contains and the worker's accounting decision. The saved
        // exchange phase also keeps drained warm-up DATA outside the totals.
        observe_worker(|_, state| {
            let included = match phase {
                Phase::Measuring { start, end } if state.exchange_phase == 2 => {
                    begin >= start && finish <= end
                }
                _ => false,
            };
            let progress = &mut state.exchanges[state.exchange_phase];
            let destination = if included {
                &mut progress.included
            } else {
                &mut progress.excluded
            };
            destination.count += 1;
            destination.bytes += *bytes;
        });
    }

    pub(super) fn diagnostic_worker_phase(phase: Phase) {
        observe_worker(|_, state| {
            let (name, index) = match phase {
                Phase::Preparing => ("preparation", 0),
                Phase::Warmup => ("warmup", 1),
                Phase::Draining => ("draining", 1),
                Phase::Measuring { .. } => ("measurement", 2),
            };
            state.phase = name;
            state.exchange_phase = index;
        });
    }

    pub(super) fn diagnostic_reuse() {
        diagnostic_worker_stage("retained-sender-reuse");
        observe_worker(|_, state| {
            state.phase = "reuse";
            state.exchange_phase = 3;
        });
    }

    pub(super) fn diagnostic_totals(totals: &Totals, failed: bool) {
        observe_worker(|_, state| {
            state.measured = totals.latencies_us.len();
            state.errors = totals.errors;
            state.body_bytes = totals.body_bytes;
            if failed {
                state.last_error_stage = Some(state.stage);
            }
        });
    }

    pub(super) fn diagnostic_coordinator_stage(stage: &'static str) {
        let _ = HEALTH_COORDINATOR.try_with(|diagnostics| diagnostics.coordinator_stage(stage));
    }

    pub(super) fn diagnostic_readiness_start(stage: &str) {
        let name = if stage == "startup" {
            "startup-readiness"
        } else {
            "drain-readiness"
        };
        diagnostic_coordinator_stage(name);
        let _ = HEALTH_COORDINATOR.try_with(|diagnostics| {
            let mut state = diagnostics
                .coordinator
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.readiness = 0;
        });
    }

    pub(super) fn diagnostic_readiness_received() {
        let _ = HEALTH_COORDINATOR.try_with(|diagnostics| {
            let mut state = diagnostics
                .coordinator
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.readiness += 1;
        });
    }

    pub(super) fn diagnostic_worker_joined() {
        let _ = HEALTH_COORDINATOR.try_with(|diagnostics| {
            let mut state = diagnostics
                .coordinator
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.joined += 1;
        });
    }

    struct DriverDiagnostic(Option<Arc<AtomicUsize>>);

    impl Drop for DriverDiagnostic {
        fn drop(&mut self) {
            if let Some(state) = &self.0 {
                state.store(4, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn diagnostic_driver<F>(
        connection: F,
    ) -> impl Future<Output = Result<(), hyper::Error>>
    where
        F: Future<Output = Result<(), hyper::Error>>,
    {
        let mut state = None;
        observe_worker(|_, worker| {
            state = worker.connection.as_ref().map(|c| Arc::clone(&c.driver));
        });
        let mut guard = DriverDiagnostic(state);
        let dispatch = diagnostic_dispatch();
        let observer = HEALTH_WORKER
            .try_with(|(diagnostics, index)| (Arc::clone(diagnostics), *index))
            .ok();
        let mut polls = None;
        if let Some((diagnostics, index)) = &observer {
            diagnostics.worker(*index, |state| {
                if let Some(connection) = &state.connection {
                    polls = diagnostics.observer.task(
                        "public-dispatcher",
                        connection.owner,
                        connection.generation,
                        connection.local_addr,
                        0,
                    );
                }
            });
        }
        let connection = crate::health::Observed::new(connection, polls);
        async move {
            if let Some(state) = &guard.0 {
                state.store(1, Ordering::Relaxed);
            }
            let result = match dispatch {
                Some(dispatch) => connection.with_subscriber(dispatch).await,
                None => connection.await,
            };
            if let Err(error) = &result
                && let Some((diagnostics, index)) = observer
            {
                diagnostics.worker(index, |state| {
                    record_error(state, "public-driver-result", error);
                });
            }
            if let Some(state) = guard.0.take() {
                state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Relaxed);
            }
            result
        }
    }

    struct WorkerDiagnosticGuard {
        diagnostics: Arc<HealthDiagnostics>,
        index: usize,
        finished: bool,
    }

    impl WorkerDiagnosticGuard {
        fn complete(&mut self) {
            self.finished = true;
        }
    }

    impl Drop for WorkerDiagnosticGuard {
        fn drop(&mut self) {
            if !self.finished {
                self.diagnostics.worker(self.index, |state| {
                    state.outcome = "future-dropped";
                });
            }
        }
    }

    fn diagnostic_worker<F>(
        diagnostics: Arc<HealthDiagnostics>,
        index: usize,
        future: F,
    ) -> impl Future<Output = Result<Totals, Failure>>
    where
        F: Future<Output = Result<Totals, Failure>>,
    {
        let polls = diagnostics.observer.task("worker", index, 0, None, 0);
        let future = crate::health::Observed::new(future, polls);
        HEALTH_WORKER.scope((Arc::clone(&diagnostics), index), async move {
            // Own the whole guard inside the scope across the worker's await.
            let mut guard = WorkerDiagnosticGuard {
                diagnostics,
                index,
                finished: false,
            };
            observe_worker(|_, state| state.outcome = "running");
            let result = future.await;
            if let Ok(totals) = &result {
                // Snapshot the returned totals after retained-sender reuse,
                // rather than relying on the last measurement-loop snapshot.
                diagnostic_totals(totals, false);
            }
            if let Err(error) = &result {
                diagnostic_error(error.as_ref());
            }
            observe_worker(|_, state| {
                state.outcome = if result.is_ok() {
                    "completed-ok"
                } else {
                    "completed-error"
                };
                if result.is_err() {
                    state.last_error_stage = Some(state.stage);
                }
            });
            guard.complete();
            result
        })
    }

    #[tokio::test]
    async fn health_diagnostics_preserve_successful_worker_completion_after_drop() {
        let diagnostics = Arc::new(HealthDiagnostics::new(load(4, 2)));
        let worker = diagnostic_worker(Arc::clone(&diagnostics), 0, async {
            // An uncounted warm-up error must survive a later successful return.
            diagnostic_worker_phase(Phase::Warmup);
            diagnostic_worker_stage("response-headers");
            diagnostic_error(&std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            diagnostic_totals(&Totals::default(), true);
            let totals = Totals {
                latencies_us: vec![7],
                body_bytes: 512,
                ..Totals::default()
            };
            diagnostic_totals(&totals, false);
            diagnostic_reuse();
            Ok(totals)
        });
        let mut worker = Box::pin(worker);
        // A ready inner future must finish in one poll. Retain the completed
        // wrapper until both its completion and destruction can be checked.
        let totals = std::future::poll_fn(|cx| match worker.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => panic!("ready worker did not complete in one poll"),
        })
        .await
        .unwrap();
        assert_eq!(totals.latencies_us, vec![7]);
        assert_eq!(totals.body_bytes, 512);
        assert_eq!(totals.errors, 0);
        let assert_completed = || {
            diagnostics.worker(0, |state| {
                assert_eq!(state.outcome, "completed-ok");
                assert_eq!(state.stage, "retained-sender-reuse");
                assert_eq!(state.measured, 1);
                assert_eq!(state.body_bytes, 512);
                assert_eq!(state.last_error_stage, Some("response-headers"));
                assert_eq!(state.error_events, 1);
                assert!(state.error_samples[0].chain[0].contains("ConnectionReset"));
            });
        };
        assert_completed();
        drop(worker);
        assert_completed();
    }

    #[tokio::test]
    async fn health_diagnostics_preserve_worker_error_completion_after_drop() {
        let diagnostics = Arc::new(HealthDiagnostics::new(load(4, 2)));
        let worker = diagnostic_worker(Arc::clone(&diagnostics), 0, async {
            diagnostic_worker_stage("first-data-frame");
            diagnostic_body_bytes(512);
            Err(std::io::Error::other("worker completion regression").into())
        });
        let mut worker = Box::pin(worker);
        let error = std::future::poll_fn(|cx| match worker.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => panic!("ready worker error did not complete in one poll"),
        })
        .await
        .unwrap_err();
        let error = error.downcast::<std::io::Error>().unwrap();
        assert_eq!(error.to_string(), "worker completion regression");
        let assert_completed = || {
            diagnostics.worker(0, |state| {
                assert_eq!(state.outcome, "completed-error");
                assert_eq!(state.stage, "first-data-frame");
                assert_eq!(state.last_error_stage, Some("first-data-frame"));
                assert_eq!(state.exchanges[0].bytes, 512);
                assert_eq!(state.error_events, 1);
                let sample = &state.error_samples[0];
                assert_eq!(sample.stage, "first-data-frame");
                assert!(sample.chain[0].contains("io::Error kind=Other"));
                assert!(sample.source_end.contains("unknown"));
                assert!(!sample.chain[0].contains("worker completion regression"));
            });
        };
        assert_completed();
        drop(worker);
        assert_completed();
    }

    #[tokio::test]
    async fn health_diagnostics_survive_dropped_worker_and_driver_futures() {
        let diagnostics = Arc::new(HealthDiagnostics::new(load(4, 2)));
        let worker = diagnostic_worker(Arc::clone(&diagnostics), 0, async {
            diagnostic_dial();
            diagnostic_socket(Some("127.0.0.1:12345".parse().unwrap()));
            diagnostic_worker_phase(Phase::Warmup);
            observe_worker(|_, state| state.exchanges[1].started = 1);
            diagnostic_body_bytes(512);
            diagnostic_worker_stage("first-data-frame");
            diagnostic_error(&std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            let driver = diagnostic_driver(std::future::pending::<Result<(), hyper::Error>>());
            driver.await?;
            Ok(Totals::default())
        });
        let mut worker = Box::pin(worker);
        // Poll once to establish the pending await, then drop exactly the
        // future a timeout would cancel. No timer, network or retry is needed.
        std::future::poll_fn(|cx| {
            assert!(worker.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(worker);
        diagnostics.worker(0, |state| {
            assert_eq!(state.outcome, "future-dropped");
            assert_eq!(state.phase, "warmup");
            assert_eq!(state.stage, "first-data-frame");
            assert_eq!(state.exchanges[1].started, 1);
            assert_eq!(state.exchanges[1].completed, 0);
            assert_eq!(state.exchanges[1].bytes, 512);
            assert_eq!(state.error_events, 1);
            assert_eq!(state.error_samples[0].stage, "first-data-frame");
            assert!(state.error_samples[0].chain[0].contains("ConnectionReset"));
            let connection = state.connection.as_ref().unwrap();
            assert_eq!(connection.owner, 0);
            assert_eq!(connection.generation, 1);
            assert_eq!(connection.driver.load(Ordering::Relaxed), 4);
        });
        // Observation after scope exit cannot contaminate a retained slot.
        diagnostic_worker_stage("outside-scope");
        diagnostics.worker(0, |state| {
            assert_eq!(state.stage, "first-data-frame");
        });
    }

    #[test]
    fn error_observation_bounds_formatting_and_opaque_source_chains() {
        struct RepeatedText(AtomicUsize);

        impl std::fmt::Display for RepeatedText {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for _ in 0..10_000 {
                    self.0.fetch_add(1, Ordering::Relaxed);
                    formatter.write_str("é")?;
                }
                Ok(())
            }
        }

        let text = RepeatedText(AtomicUsize::new(0));
        let mut bounded = BoundedText::default();
        assert!(write!(bounded, "{text}").is_err());
        assert_eq!(bounded.0.len(), DIAGNOSTIC_TEXT_BYTES);
        assert_eq!(
            text.0.load(Ordering::Relaxed),
            DIAGNOSTIC_TEXT_BYTES / 2 + 1,
        );

        #[derive(Debug)]
        struct Opaque;

        impl std::fmt::Display for Opaque {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("opaque error text must never be formatted");
            }
        }

        impl std::error::Error for Opaque {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self)
            }
        }

        let diagnostics = HealthDiagnostics::new(load(4, 2));
        diagnostics.worker(0, |state| {
            for _ in 0..100 {
                record_error(state, "preparation", &Opaque);
            }
            assert_eq!(state.error_events, 100);
            assert_eq!(state.error_samples.len(), ERROR_SAMPLES);
            for sample in &state.error_samples {
                assert_eq!(sample.chain.len(), DIAGNOSTIC_CHAIN_DEPTH);
                assert!(
                    sample
                        .chain
                        .iter()
                        .all(|s| s.len() <= DIAGNOSTIC_TEXT_BYTES)
                );
                assert!(sample.source_end.starts_with("depth-limit"));
            }
        });
    }

    #[test]
    fn health_observer_retains_real_initial_dial_and_preparation_failures() {
        for preparation in [false, true] {
            let runtime = CancellationRuntime::new();
            let diagnostics = Arc::new(HealthDiagnostics::new(load(4, 2)));
            let result = runtime.block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let target = Target {
                    addr: listener.local_addr().unwrap(),
                    transport: Transport::H2c,
                    workload: Workload::Cancel,
                    tls: None,
                };
                if preparation {
                    tokio::spawn(async move {
                        let mut servers = JoinSet::new();
                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            servers.spawn(async move {
                                let service = service_fn(|_| {
                                    std::future::ready(Ok::<_, Infallible>(Response::new(Empty::<
                                        Bytes,
                                    >::new(
                                    ))))
                                });
                                let builder =
                                    hyper::server::conn::http2::Builder::new(TokioExecutor::new());
                                let _ = builder
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            });
                        }
                    });
                } else {
                    // A real refused initial TCP connection, before workers
                    // exist; the same observation hook retains its identity.
                    drop(listener);
                }
                tokio::time::timeout(
                    Duration::from_secs(10),
                    cancellation_health(target, load(4, 2), || (), Arc::clone(&diagnostics)),
                )
                .await
            });
            runtime.shutdown();
            assert!(result.unwrap().is_err());
            let mut retained = 0;
            for worker in 0..4 {
                diagnostics.worker(worker, |state| {
                    for sample in &state.error_samples {
                        let ended = sample.chain[0].contains("response ended before cancellation");
                        if preparation && ended {
                            assert_eq!(sample.stage, "first-data-frame");
                            retained += 1;
                        } else if !preparation && sample.chain[0].contains("ConnectionRefused") {
                            assert_eq!(sample.stage, "tcp-connect");
                            assert_eq!(state.outcome, "initial-dial-error");
                            retained += 1;
                        }
                    }
                });
            }
            assert!(retained > 0);
        }
    }

    #[test]
    fn scoped_observer_sees_real_h2_child_wire_errors_without_peer_bytes() {
        async fn wire_failure(diagnostics: Arc<HealthDiagnostics>, protocol: bool) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = Target {
                addr: listener.local_addr().unwrap(),
                transport: Transport::H2c,
                workload: Workload::Cancel,
                tls: None,
            };
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut preface = [0; 24];
                wire_read(&stream, &mut preface).await;
                assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
                h2_write(&stream, 4, 0, 0, &[]).await;
                h2_until(&stream, 1).await;
                if protocol {
                    // DATA on stream zero is a real protocol error. No frame
                    // or peer bytes may be retained by the event collector.
                    h2_write(&stream, 0, 0, 0, b"peer-private-marker").await;
                    let (_, id, payload) = h2_until(&stream, 7).await;
                    assert_eq!(id, 0);
                    let last = u32::from_be_bytes(payload[..4].try_into().unwrap()) & 0x7fff_ffff;
                    let reason = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                    assert_eq!(reason, 1); // PROTOCOL_ERROR, not a guessed delay cause.
                    Some((last, reason))
                } else {
                    // Truncated PING (9-byte header + 1 of 8 payload bytes)
                    // followed by EOF is a real framed-reader I/O error.
                    wire_write(&stream, &[0, 0, 8, 6, 0, 0, 0, 0, 0, 0]).await;
                    None
                }
            });
            let result = diagnostic_worker(Arc::clone(&diagnostics), 0, async {
                let mut sender = dial(&target).await?;
                exchange(&mut sender, &target).await?;
                Ok(Totals::default())
            })
            .await;
            assert!(result.is_err());
            let goaway = server.await.unwrap();
            let wires = diagnostics.observer.wires();
            assert_eq!(wires.len(), 1);
            let frames = wires[0].snapshot();
            if let Some((last, reason)) = goaway {
                assert!(frames.has_goaway(crate::health::Direction::Tx, last, reason));
                assert_eq!(frames.directions[1].invalid_streams, 1);
            } else {
                assert!(frames.directions[1].eof_partial());
                assert_eq!(frames.directions[1].invalid_lengths, 0);
            }
            let mut frame_text = String::new();
            diagnostics
                .observer
                .write_wire(&mut frame_text, Instant::now());
            assert!(!frame_text.contains("peer-private-marker"));
            diagnostics.worker(0, |state| {
                assert!(state.wire_events > 0);
                assert!(state.wire_samples.len() <= ERROR_SAMPLES);
                let sample = &state.wire_samples[0];
                assert_eq!(sample.generation, 1);
                let connection = state.connection.as_ref().unwrap();
                assert_eq!(sample.local_addr, connection.local_addr);
                if protocol {
                    assert_eq!(sample.event, "h2-connection-error");
                    assert!(sample.error.starts_with("unknown"));
                } else {
                    assert_eq!(sample.event, "h2-io-error");
                    // h2 0.4.19's length-delimited reader needs 17 bytes here.
                    // With only 10 buffered, tokio-util 0.7.19's default
                    // Decoder::decode_eof returns ErrorKind::Other; h2
                    // preserves that kind at its I/O-error callsite.
                    assert_eq!(sample.error, "Other");
                }
                let wire_text = format!("{:?}", state.wire_samples);
                let error_text = format!("{:?}", state.error_samples);
                assert!(!wire_text.contains("peer-private-marker"));
                assert!(!error_text.contains("peer-private-marker"));
                assert!(state.error_events > 0);
            });
        }

        let runtime = CancellationRuntime::new();
        let left = Arc::new(HealthDiagnostics::new(load(4, 2)));
        let right = Arc::new(HealthDiagnostics::new(load(4, 2)));
        let result = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                // Separate subscribers coexist without a global default. The
                // real wire futures are separately spawned executor children.
                tokio::join!(
                    wire_failure(Arc::clone(&left), true),
                    wire_failure(Arc::clone(&right), false),
                );
            })
            .await
        });
        runtime.shutdown();
        result.unwrap();
        let mut address = None;
        left.worker(0, |state| {
            address = state.connection.as_ref().unwrap().local_addr
        });
        right.worker(0, |state| {
            assert_ne!(address, state.connection.as_ref().unwrap().local_addr);
        });
        for diagnostics in [&left, &right] {
            let wire = &diagnostics.observer.wires()[0];
            assert_eq!(wire.owner, 0);
            assert_eq!(wire.generation, 1);
            diagnostics.worker(0, |state| {
                assert_eq!(wire.socket, state.connection.as_ref().unwrap().local_addr);
            });
            let mut text = String::new();
            diagnostics.observer.write_wire(&mut text, Instant::now());
            assert!(text.contains(&diagnostics.instance));
            let other = if Arc::ptr_eq(diagnostics, &left) {
                &right
            } else {
                &left
            };
            assert!(!text.contains(&other.instance));
            for index in 1..4 {
                diagnostics.worker(index, |state| assert_eq!(state.wire_events, 0));
            }
        }
    }

    /// Functional cancellation health is finite work, not an unbounded reset
    /// flood. Pinned h2 retains 50 local resets per connection for one second.
    /// With two workers per connection, preparation, both phase budgets and
    /// the final reuse probe require at most 2 * (1 + 8 + 8 + 1) = 36 retained
    /// resets, even if none expire.
    pub(crate) const HEALTH_EXCHANGES_PER_PHASE: usize = 8;

    pub(crate) async fn cancellation_health<S>(
        target: Target,
        load: Load,
        probe: impl Fn() -> S,
        diagnostics: Arc<HealthDiagnostics>,
    ) -> Result<Measured<S>, Failure> {
        HEALTH_COORDINATOR
            .scope(Arc::clone(&diagnostics), async move {
                cancellation_health_inner(target, load, probe, diagnostics).await
            })
            .await
    }

    pub(crate) async fn cancellation_health_timeout<S>(
        health: impl Future<Output = Result<Measured<S>, Failure>>,
        diagnostics: &HealthDiagnostics,
        cell: crate::dims::Cell,
    ) -> Result<Measured<S>, Failure> {
        // Shared by the real matrices and the controlled reuse regression.
        // Keep the original strict failure bound and original Result.
        let mut health = Box::pin(health);
        let result = tokio::time::timeout(Duration::from_secs(15), health.as_mut()).await;
        match result {
            Ok(Ok(measured)) => Ok(measured),
            Ok(Err(error)) => {
                let capture = capture_before_drop(health, diagnostics);
                diagnostics.failure_capture(cell, "driver-error", capture);
                Err(error)
            }
            Err(error) => {
                let capture = capture_before_drop(health, diagnostics);
                diagnostics.failure_capture(cell, "timeout", capture);
                Err(error.into())
            }
        }
    }

    fn capture_before_drop<F>(
        health: Pin<Box<F>>,
        diagnostics: &HealthDiagnostics,
    ) -> HealthCapture {
        let mut capture = diagnostics.capture();
        capture.before_health_drop = true;
        // Dropping the coordinator can abort JoinSet workers and release
        // senders. No subsequent rendering reads their evolving observations.
        drop(health);
        capture
    }

    async fn cancellation_health_inner<S>(
        target: Target,
        load: Load,
        probe: impl Fn() -> S,
        diagnostics: Arc<HealthDiagnostics>,
    ) -> Result<Measured<S>, Failure> {
        load.validate(target.transport)?;
        assert_eq!(target.workload, Workload::Cancel);
        assert_eq!(load.concurrency, 4);
        assert_eq!(load.streams, 2);
        let target = Arc::new(target);
        let (phase, receiver) = watch::channel(Phase::Preparing);
        let (ready, mut readiness) = mpsc::unbounded_channel();
        let mut workers = JoinSet::new();
        let mut worker_index = 0;
        diagnostic_coordinator_stage("initial-dial-and-spawn");
        for _ in 0..load.connections(target.transport) {
            let first = worker_index;
            diagnostics.worker(first, |state| state.outcome = "dialing");
            let sender = HEALTH_WORKER
                .scope((Arc::clone(&diagnostics), first), async {
                    let result = dial(&target).await;
                    if let Err(error) = &result {
                        diagnostic_error(error.as_ref());
                        observe_worker(|_, state| state.outcome = "initial-dial-error");
                    }
                    result
                })
                .await?;
            let mut connection = None;
            diagnostics.worker(first, |state| connection = state.connection.clone());
            let mut senders = Vec::new();
            if let Sender::H2(shared) = &sender {
                for _ in 1..load.streams {
                    senders.push(Sender::H2(shared.clone()));
                }
            }
            senders.push(sender);
            for sender in senders {
                let index = worker_index;
                worker_index += 1;
                if index != first {
                    diagnostics.worker(index, |state| state.connection = connection.clone());
                }
                diagnostics.worker(index, |state| {
                    state.outcome = "spawned-not-polled";
                    state.sender_closed = Some(sender.is_closed());
                });
                workers.spawn(diagnostic_worker(
                    Arc::clone(&diagnostics),
                    index,
                    worker_inner(
                        Arc::clone(&target),
                        sender,
                        receiver.clone(),
                        ready.clone(),
                        Some(HEALTH_EXCHANGES_PER_PHASE),
                    ),
                ));
            }
        }
        drop(ready);
        drop(receiver);
        let connects = load.connections(target.transport) as u64;
        measure_workers(load, &phase, &mut readiness, &mut workers, connects, probe).await
    }

    /// Own every async task for one transport, including the connection
    /// drivers and H2 children spawned by the unchanged TokioExecutor.
    /// A current-thread runtime leaves no concurrently polling task when
    /// block_on returns or unwinds; shutdown drops all remaining task futures.
    /// This fixture does not spawn blocking work.
    struct CancellationRuntime {
        runtime: Option<Runtime>,
    }

    impl CancellationRuntime {
        fn new() -> Self {
            Self {
                runtime: Some(
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap(),
                ),
            }
        }

        fn block_on<F: Future>(&self, future: F) -> F::Output {
            self.runtime.as_ref().unwrap().block_on(future)
        }

        fn shutdown(mut self) {
            let runtime = self.runtime.take().unwrap();
            let metrics = runtime.metrics();
            runtime.shutdown_timeout(Duration::from_secs(1));
            // Check after shutdown, not after merely scheduling aborts. This
            // includes tasks whose JoinHandles the production driver discards.
            assert_eq!(metrics.num_alive_tasks(), 0);
        }
    }

    impl Drop for CancellationRuntime {
        fn drop(&mut self) {
            if let Some(runtime) = self.runtime.take() {
                // Unwind cannot asynchronously join the worker/server sets.
                // Their drops request abort; runtime shutdown also drops every
                // remaining async driver and executor child before returning.
                runtime.shutdown_timeout(Duration::from_secs(1));
            }
        }
    }

    #[test]
    fn cancellation_runtime_ends_drivers_and_executor_children_on_every_exit() {
        #[derive(Clone, Copy, Debug)]
        enum Exit {
            Normal,
            Timeout,
            Unwind,
        }

        struct TaskDrop(Arc<AtomicUsize>);

        impl Drop for TaskDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        for exit in [Exit::Normal, Exit::Timeout, Exit::Unwind] {
            let runtime = CancellationRuntime::new();
            let metrics = runtime.runtime.as_ref().unwrap().metrics();
            let dropped = Arc::new(AtomicUsize::new(0));
            let driver_drop = TaskDrop(Arc::clone(&dropped));
            let child_drop = TaskDrop(Arc::clone(&dropped));
            let (driver,) = runtime.block_on(async {
                let (started, child_started) = oneshot::channel();
                let driver = tokio::spawn(async move {
                    let _driver_drop = driver_drop;
                    TokioExecutor::new().execute(async move {
                        let _child_drop = child_drop;
                        started.send(()).unwrap();
                        std::future::pending::<()>().await;
                    });
                    std::future::pending::<()>().await;
                });
                let ready = tokio::time::timeout(Duration::from_secs(10), child_started).await;
                ready.unwrap().unwrap();
                (driver,)
            });
            assert_eq!(metrics.num_alive_tasks(), 2);
            assert!(!driver.is_finished());
            match exit {
                Exit::Normal => runtime.shutdown(),
                Exit::Timeout => {
                    let result = runtime.block_on(async {
                        let pending = std::future::pending::<()>();
                        tokio::time::timeout(Duration::ZERO, pending).await
                    });
                    drop(runtime);
                    assert!(result.is_err());
                }
                Exit::Unwind => {
                    let result = std::panic::catch_unwind(move || {
                        runtime.block_on(async { panic!("fixture unwind") });
                    });
                    assert!(result.is_err());
                }
            }
            // Both pending futures were destroyed and the retained driver
            // handle is finished, rather than merely marked for cancellation.
            assert!(driver.is_finished(), "{exit:?}");
            assert_eq!(dropped.load(Ordering::SeqCst), 2, "{exit:?}");
            assert_eq!(metrics.num_alive_tasks(), 0, "{exit:?}");
        }
    }

    async fn wire_read(stream: &TcpStream, bytes: &mut [u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            stream.readable().await.unwrap();
            match stream.try_read(&mut bytes[offset..]) {
                Ok(0) => panic!("wire peer closed before sending the expected bytes"),
                Ok(read) => offset += read,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("wire read: {error}"),
            }
        }
    }

    async fn wire_write(stream: &TcpStream, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            stream.writable().await.unwrap();
            match stream.try_write(&bytes[offset..]) {
                Ok(0) => panic!("wire peer stopped accepting bytes"),
                Ok(written) => offset += written,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("wire write: {error}"),
            }
        }
    }

    /// Send only half of one declared 1 KiB HTTP/1 chunk. The second half is
    /// withheld until exchange returns, so success cannot depend on coalescing
    /// reads or reaching the chunk boundary / end of the response.
    pub(crate) fn partial_first_cancellation() -> (u64, u64) {
        let runtime = CancellationRuntime::new();
        let result = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let target = Target {
                    addr: listener.local_addr().unwrap(),
                    transport: Transport::H1,
                    workload: Workload::Cancel,
                    tls: None,
                };
                let (finished, cancelled) = oneshot::channel();
                let mut servers = JoinSet::new();
                servers.spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    stream.set_nodelay(true).unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        wire_read(&stream, &mut byte).await;
                        request.push(byte[0]);
                        assert!(request.len() <= 8192);
                    }
                    wire_write(
                        &stream,
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n400\r\n",
                    )
                    .await;
                    wire_write(&stream, &[b'x'; FRAME_BYTES / 2]).await;
                    cancelled.await.unwrap();
                });
                let mut sender = dial(&target).await.unwrap();
                let bytes = exchange(&mut sender, &target).await.unwrap();
                assert!(bytes > 0 && bytes <= (FRAME_BYTES / 2) as u64);
                drop(sender);
                let mut totals = Totals {
                    connects: 1,
                    ..Totals::default()
                };
                // Exercise the real HTTP/1 reconnect before releasing the
                // peer, rather than inventing a connect count for reporting.
                let replacement = connected(&target, None, &mut totals).await.unwrap();
                drop(replacement);
                finished.send(()).unwrap();
                servers.join_next().await.unwrap().unwrap();
                assert!(servers.is_empty());
                (bytes, totals.connects)
            })
            .await
        });
        runtime.shutdown();
        result.unwrap()
    }

    async fn h2_frame(stream: &TcpStream) -> (u8, u8, u32, Vec<u8>) {
        let mut header = [0; 9];
        wire_read(stream, &mut header).await;
        let len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
        assert!(len <= 16_384);
        let id = u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff;
        let mut payload = vec![0; len];
        wire_read(stream, &mut payload).await;
        (header[3], header[4], id, payload)
    }

    async fn h2_write(stream: &TcpStream, kind: u8, flags: u8, id: u32, payload: &[u8]) {
        let len = u32::try_from(payload.len()).unwrap().to_be_bytes();
        let mut header = [0; 9];
        header[..3].copy_from_slice(&len[1..]);
        header[3] = kind;
        header[4] = flags;
        header[5..].copy_from_slice(&id.to_be_bytes());
        wire_write(stream, &header).await;
        wire_write(stream, payload).await;
    }

    async fn h2_until(stream: &TcpStream, expected: u8) -> (u8, u32, Vec<u8>) {
        loop {
            let (kind, flags, id, payload) = h2_frame(stream).await;
            if kind == expected {
                return (flags, id, payload);
            }
            match kind {
                4 if flags == 0 => h2_write(stream, 4, 1, 0, &[]).await,
                4 | 8 => {}
                _ => panic!("expected H2 frame {expected}, received {kind}"),
            }
        }
    }

    async fn h2_cancelled_response(stream: &TcpStream) -> u32 {
        let (flags, id, _) = h2_until(stream, 1).await;
        assert_eq!(flags & 5, 5); // END_STREAM | END_HEADERS on the empty GET.
        h2_write(stream, 1, 4, id, &[0x88]).await; // HPACK static :status 200.
        h2_write(stream, 0, 0, id, &[b'x'; FRAME_BYTES]).await;
        let (_, reset_id, reason) = h2_until(stream, 3).await;
        assert_eq!(reset_id, id);
        assert_eq!(reason, 8_u32.to_be_bytes()); // CANCEL.
        id
    }

    #[test]
    fn late_data_requires_retained_reset_state_for_http2_reuse() {
        // Exercise the pinned default of 50 retained resets. Extend only the
        // test retention time so scheduling cannot make resets expire; lower
        // the error-reset limit to expose the first forgotten-stream error.
        // No client or server production setting is changed by this fixture.
        for resets in [50, 51] {
            let runtime = CancellationRuntime::new();
            let result = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let target = Target {
                        addr: listener.local_addr().unwrap(),
                        transport: Transport::H2c,
                        workload: Workload::Cancel,
                        tls: None,
                    };
                    let (acknowledged, mut acknowledgements) = mpsc::unbounded_channel();
                    let mut servers = JoinSet::new();
                    servers.spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        stream.set_nodelay(true).unwrap();
                        let mut preface = [0; 24];
                        wire_read(&stream, &mut preface).await;
                        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
                        h2_write(&stream, 4, 0, 0, &[]).await;
                        let mut last = 0;
                        for _ in 0..resets {
                            last = h2_cancelled_response(&stream).await;
                            acknowledged.send(()).unwrap();
                        }
                        // DATA already in flight when CANCEL was sent must
                        // be ignored while the reset is retained.
                        h2_write(&stream, 0, 0, last, &[b'x'; FRAME_BYTES]).await;
                        if resets == 50 {
                            h2_cancelled_response(&stream).await;
                        } else {
                            let (_, id, payload) = h2_until(&stream, 7).await;
                            assert_eq!(id, 0);
                            assert_eq!(&payload[4..8], &11_u32.to_be_bytes());
                            assert_eq!(&payload[8..], b"too_many_internal_resets");
                        }
                    });
                    let stream = TcpStream::connect(target.addr).await.unwrap();
                    stream.set_nodelay(true).unwrap();
                    let mut builder = http2::Builder::new(TokioExecutor::new());
                    builder
                        .reset_stream_duration(Duration::from_secs(60))
                        .max_local_error_reset_streams(0);
                    let (sender, connection) =
                        builder.handshake(TokioIo::new(stream)).await.unwrap();
                    let mut drivers = JoinSet::new();
                    drivers.spawn(connection);
                    let mut sender = Sender::H2(sender);
                    for _ in 0..resets {
                        assert_eq!(exchange(&mut sender, &target).await.unwrap(), 1024);
                        acknowledgements.recv().await.unwrap();
                    }
                    if resets == 50 {
                        assert_eq!(exchange(&mut sender, &target).await.unwrap(), 1024);
                    }
                    servers.join_next().await.unwrap().unwrap();
                    drop(sender);
                    drivers.shutdown().await;
                    assert!(servers.is_empty() && drivers.is_empty());
                })
                .await
            });
            runtime.shutdown();
            result.unwrap();
        }
    }

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

    struct Cancellation {
        connection: usize,
        release: oneshot::Sender<()>,
        cancelled: oneshot::Receiver<()>,
    }

    async fn cancellations(
        arrivals: &mut mpsc::UnboundedReceiver<Cancellation>,
        count: usize,
    ) -> Vec<Cancellation> {
        let mut requests = Vec::new();
        for _ in 0..count {
            requests.push(arrivals.recv().await.unwrap());
        }
        requests
    }

    async fn release_cancellations(requests: Vec<Cancellation>) -> Vec<usize> {
        let mut connections = Vec::new();
        let mut cancelled = Vec::new();
        for request in requests {
            connections.push(request.connection);
            request.release.send(()).unwrap();
            cancelled.push(request.cancelled);
        }
        for acknowledgement in cancelled {
            acknowledgement.await.unwrap();
        }
        connections.sort_unstable();
        connections
    }

    struct CountedGateBody {
        inner: GatedBody,
        drops: Arc<AtomicUsize>,
        changed: Arc<tokio::sync::Notify>,
    }

    impl hyper::body::Body for CountedGateBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            hyper::body::Body::poll_frame(Pin::new(&mut self.inner), cx)
        }
    }

    impl Drop for CountedGateBody {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_one();
        }
    }

    struct LiveFixtureTask(Arc<AtomicUsize>);

    impl LiveFixtureTask {
        fn new(live: Arc<AtomicUsize>) -> Self {
            live.fetch_add(1, Ordering::SeqCst);
            Self(live)
        }
    }

    impl Drop for LiveFixtureTask {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn bounded_poll_observer_distinguishes_wire_headers_and_data_gates() {
        #[derive(Clone, Copy, Debug)]
        enum Held {
            Wire,
            Headers,
            Data,
        }

        for held in [Held::Wire, Held::Headers, Held::Data] {
            for release in [false, true] {
                let runtime = CancellationRuntime::new();
                let diagnostics = Arc::new(
                    HealthDiagnostics::new(load(4, 2)).with_origin("controlled-poll-gates"),
                );
                let unrelated = HealthDiagnostics::new(load(4, 2));
                let gate = Arc::new(crate::health::Gate::default());
                if matches!(held, Held::Wire) {
                    *diagnostics.observer.wire_gate.lock().unwrap() = Some(Arc::clone(&gate));
                }
                let result = runtime.block_on(async {
                    tokio::time::timeout(Duration::from_secs(10), async {
                        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let target = Target {
                            addr: listener.local_addr().unwrap(),
                            transport: Transport::H2c,
                            workload: Workload::Cancel,
                            tls: None,
                        };
                        let server_observer = Arc::clone(&diagnostics);
                        let server_gate = Arc::clone(&gate);
                        let mut servers = JoinSet::new();
                        servers.spawn(async move {
                            let (stream, peer) = listener.accept().await.unwrap();
                            stream.set_nodelay(true).unwrap();
                            let service = service_fn(move |_| {
                                let observation = server_observer.observer.request(Some(peer));
                                let headers =
                                    matches!(held, Held::Headers).then(|| Arc::clone(&server_gate));
                                let data =
                                    matches!(held, Held::Data).then(|| Arc::clone(&server_gate));
                                async move {
                                    let response = crate::health::response_with_gates(
                                        async {
                                            Response::new(axum::body::Body::new(GatedBody {
                                                release: None,
                                                cancelled: None,
                                                data: Bytes::from_static(&[b'x'; FRAME_BYTES]),
                                                sent: false,
                                            }))
                                        },
                                        observation,
                                        headers,
                                        data,
                                    )
                                    .await;
                                    Ok::<_, Infallible>(response)
                                }
                            });
                            let builder =
                                hyper::server::conn::http2::Builder::new(TokioExecutor::new());
                            let _ = builder
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                        let mut workers = JoinSet::new();
                        workers.spawn(diagnostic_worker(Arc::clone(&diagnostics), 0, async move {
                            let mut sender = dial(&target).await?;
                            let first = exchange(&mut sender, &target).await?;
                            diagnostic_reuse();
                            let reuse = exchange(&mut sender, &target).await?;
                            Ok(Totals {
                                body_bytes: first + reuse,
                                ..Totals::default()
                            })
                        }));
                        gate.reached().await;
                        let worker = diagnostics.observer.tasks("worker");
                        assert_eq!(worker.len(), 1);
                        worker[0].pending().await;
                        let driver = diagnostics.observer.tasks("public-dispatcher");
                        assert_eq!(driver.len(), 1);
                        driver[0].pending().await;
                        let wire = diagnostics
                            .observer
                            .tasks("h2-wire-child-pinned-first-spawn");
                        assert_eq!(wire.len(), 1);
                        wire[0].pending().await;
                        let callbacks = diagnostics
                            .observer
                            .tasks("h2-response-callback-child-empty-get");
                        assert_eq!(callbacks.len(), 1);
                        if matches!(held, Held::Data) {
                            callbacks[0].ready().await;
                        } else {
                            callbacks[0].pending().await;
                        }
                        let requests = diagnostics.observer.requests();
                        let expected = if matches!(held, Held::Data) {
                            "first-data-frame"
                        } else {
                            "response-headers"
                        };
                        diagnostics.wait_stage(0, expected).await;
                        let wires = diagnostics.observer.wires();
                        assert_eq!(wires.len(), 1);
                        let frames = wires[0].snapshot();
                        let tx = &frames.directions[0];
                        let rx = &frames.directions[1];
                        if matches!(held, Held::Wire) {
                            assert_eq!(wire[0].snapshot().inner_polls, 0);
                            assert!(requests.is_empty());
                            assert_eq!(tx.headers_complete, 0);
                            assert_eq!(rx.blocks_complete, 0);
                        } else {
                            assert_eq!(tx.headers_complete, 1);
                            assert_eq!(rx.data_complete, 0);
                            assert!(wire[0].snapshot().inner_polls > 0);
                            assert_eq!(requests.len(), 1);
                            let request = &requests[0];
                            assert_eq!(request.frames.load(Ordering::Relaxed), 0);
                            if matches!(held, Held::Headers) {
                                assert_eq!(rx.blocks_complete, 0);
                                assert!(request.response.lock().unwrap().is_none());
                                assert_eq!(request.handler.snapshot().inner_polls, 0);
                                assert_eq!(request.body.snapshot().polls, 0);
                            } else {
                                assert_eq!(rx.blocks_complete, 1);
                                assert!(request.response.lock().unwrap().is_some());
                                assert_eq!(request.handler.snapshot().ready, 1);
                                assert!(request.body.snapshot().pending > 0);
                                assert_eq!(request.body.snapshot().inner_polls, 0);
                                assert_eq!(callbacks[0].snapshot().ready, 1);
                            }
                        }
                        diagnostics.worker(0, |state| {
                            assert_eq!(state.stage, expected);
                            assert_eq!(state.connect_attempts, 1);
                        });
                        assert!(unrelated.observer.tasks("worker").is_empty());
                        assert!(unrelated.observer.requests().is_empty());
                        assert!(unrelated.observer.wires().is_empty());
                        if release {
                            let gated = match held {
                                Held::Wire => Arc::clone(&wire[0]),
                                Held::Headers => Arc::clone(&requests[0].handler),
                                Held::Data => Arc::clone(&worker[0]),
                            };
                            let before_wakes = gated.snapshot().wakes;
                            gate.release();
                            let totals = workers.join_next().await.unwrap().unwrap().unwrap();
                            assert_eq!(totals.body_bytes, (2 * FRAME_BYTES) as u64);
                            diagnostics.worker(0, |state| {
                                assert_eq!(state.connect_attempts, 1);
                                assert_eq!(state.exchanges[0].completed, 1);
                                assert_eq!(state.exchanges[3].completed, 1);
                                assert_eq!(state.outcome, "completed-ok");
                            });
                            let requests = diagnostics.observer.requests();
                            assert_eq!(requests.len(), 2);
                            let mut socket = None;
                            diagnostics.worker(0, |state| {
                                socket = state.connection.as_ref().unwrap().local_addr;
                            });
                            for (index, request) in requests.iter().enumerate() {
                                assert_eq!(request.socket, socket);
                                assert_eq!(request.ordinal, index + 1);
                            }
                            assert!(
                                requests
                                    .iter()
                                    .all(|request| { request.frames.load(Ordering::Relaxed) == 1 })
                            );
                            assert!(gated.snapshot().wakes > before_wakes);
                            let frames = wires[0].snapshot();
                            assert_eq!(frames.directions[0].headers_complete, 2);
                            assert_eq!(frames.directions[1].blocks_complete, 2);
                            assert_eq!(frames.directions[1].data_complete, 2);
                        }
                        // Abort and join the tasks we own. Runtime shutdown
                        // below also destroys the separately spawned children.
                        workers.shutdown().await;
                        servers.shutdown().await;
                    })
                    .await
                });
                runtime.shutdown();
                result.unwrap();
                for kind in [
                    "worker",
                    "public-dispatcher",
                    "h2-wire-child-pinned-first-spawn",
                    "h2-response-callback-child-empty-get",
                ] {
                    let tasks = diagnostics.observer.tasks(kind);
                    assert!(!tasks.is_empty(), "{held:?} {kind}");
                    assert!(tasks.iter().all(|task| task.snapshot().dropped));
                }
                for request in diagnostics.observer.requests() {
                    assert!(request.handler.snapshot().dropped);
                    if request.response.lock().unwrap().is_some() {
                        assert!(request.body.snapshot().dropped);
                    }
                }
            }
        }
    }

    fn retained_sender_probe_gate_case(release_gate: bool) {
        use crate::dims::{Cell, Scenario};

        let runtime = CancellationRuntime::new();
        let metrics = runtime.runtime.as_ref().unwrap().metrics();
        let load = Load {
            warmup: Duration::from_secs(1),
            duration: Duration::from_secs(5),
            ..load(4, 2)
        };
        let origin = if release_gate {
            "controlled-retained-release"
        } else {
            "controlled-retained-blocked"
        };
        let diagnostics = Arc::new(HealthDiagnostics::new(load).with_origin(origin));
        let accepted = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let reuse_at = Arc::new([AtomicUsize::new(usize::MAX), AtomicUsize::new(usize::MAX)]);
        let identities = Arc::new(Mutex::new([None, None]));
        let drops = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(tokio::sync::Notify::new());
        let probes = AtomicUsize::new(0);
        let result = runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = Target {
                addr: listener.local_addr().unwrap(),
                transport: Transport::H2c,
                workload: Workload::Cancel,
                tls: None,
            };
            let (gates, mut arrivals) = mpsc::unbounded_channel();
            let server_accepted = Arc::clone(&accepted);
            let server_live = Arc::clone(&live);
            let server_requests = Arc::clone(&requests);
            let server_reuse_at = Arc::clone(&reuse_at);
            let server_identities = Arc::clone(&identities);
            let server_drops = Arc::clone(&drops);
            let server_changed = Arc::clone(&changed);
            tokio::spawn(async move {
                let _owner = LiveFixtureTask::new(Arc::clone(&server_live));
                let mut connections = JoinSet::new();
                loop {
                    let (stream, peer) = listener.accept().await.unwrap();
                    stream.set_nodelay(true).unwrap();
                    let connection = server_accepted.fetch_add(1, Ordering::SeqCst);
                    assert!(connection < 2, "retained probe must not reconnect");
                    server_identities.lock().unwrap()[connection] = Some(peer);
                    let gates = gates.clone();
                    let requests = Arc::clone(&server_requests);
                    let reuse_at = Arc::clone(&server_reuse_at);
                    let drops = Arc::clone(&server_drops);
                    let changed = Arc::clone(&server_changed);
                    let service = service_fn(move |_| {
                        let index = requests[connection].fetch_add(1, Ordering::SeqCst);
                        let threshold = reuse_at[connection].load(Ordering::SeqCst);
                        let (release, cancelled) = if index >= threshold {
                            assert!(index < threshold + 2);
                            let (release, receiver) = oneshot::channel();
                            let (cancelled, acknowledgement) = oneshot::channel();
                            gates
                                .send(Cancellation {
                                    connection,
                                    release,
                                    cancelled: acknowledgement,
                                })
                                .unwrap();
                            (Some(receiver), Some(cancelled))
                        } else {
                            (None, None)
                        };
                        std::future::ready(Ok::<_, Infallible>(Response::new(CountedGateBody {
                            inner: GatedBody {
                                release,
                                cancelled,
                                data: Bytes::from_static(&[b'x'; FRAME_BYTES]),
                                sent: false,
                            },
                            drops: Arc::clone(&drops),
                            changed: Arc::clone(&changed),
                        })))
                    });
                    let live = Arc::clone(&server_live);
                    connections.spawn(async move {
                        let _owner = LiveFixtureTask::new(live);
                        let builder = Builder::new(TokioExecutor::new());
                        let _ = builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
            let probe = || {
                let index = probes.fetch_add(1, Ordering::SeqCst);
                if index == 0 {
                    // The real drain rendezvous finished. Derive each reuse
                    // ordinal from completed preparation/warm-up work, not
                    // elapsed time or an assumed warm-up request count.
                    for connection in 0..2 {
                        let mut before = 0;
                        for worker in (2 * connection)..(2 * connection + 2) {
                            diagnostics.worker(worker, |state| {
                                assert_eq!(state.phase, "draining");
                                assert_eq!(state.exchanges[0].completed, 1);
                                assert_eq!(
                                    state.exchanges[1].started,
                                    state.exchanges[1].completed,
                                );
                                assert!(state.exchanges[1].completed <= 8);
                                before += state.exchanges[0].completed as usize;
                                before += state.exchanges[1].completed as usize;
                            });
                        }
                        reuse_at[connection].store(before + 16, Ordering::SeqCst);
                    }
                } else {
                    assert_eq!(index, 1);
                    for worker in 0..4 {
                        diagnostics.worker(worker, |state| {
                            assert_eq!(state.measured, 8);
                            assert_eq!(state.exchanges[2].completed, 8);
                            assert_eq!(state.errors, 0);
                        });
                    }
                }
                index
            };
            let controlled = async {
                let health = cancellation_health(target, load, probe, Arc::clone(&diagnostics));
                tokio::pin!(health);
                let mut probes = tokio::select! {
                    result = &mut health => {
                        return match result {
                            Err(error) => Err(error),
                            Ok(_) => panic!("health returned before gated reuse"),
                        };
                    }
                    probes = cancellations(&mut arrivals, 4) => probes,
                };
                let mut connections: Vec<_> = probes.iter().map(|p| p.connection).collect();
                connections.sort_unstable();
                assert_eq!(connections, [0, 0, 1, 1]);
                assert_eq!(accepted.load(Ordering::SeqCst), 2);
                assert_eq!(live.load(Ordering::SeqCst), 3);
                let mut measured = 0;
                for worker in 0..4 {
                    diagnostics.worker(worker, |state| {
                        assert_eq!(state.phase, "reuse");
                        assert_eq!(state.measured, 8);
                        assert_eq!(state.exchanges[2].completed, 8);
                        assert_eq!(state.body_bytes, (8 * FRAME_BYTES) as u64);
                        assert_eq!(state.errors, 0);
                        measured += state.measured;
                        let connection = state.connection.as_ref().unwrap();
                        assert_eq!(connection.owner, (worker / 2) * 2);
                        assert_eq!(connection.generation, 1);
                        let peer = identities.lock().unwrap()[worker / 2];
                        assert_eq!(connection.local_addr, peer);
                    });
                }
                assert_eq!(measured, 32);
                let before =
                    reuse_at[0].load(Ordering::SeqCst) + reuse_at[1].load(Ordering::SeqCst);
                // Real server body destruction acknowledges every earlier
                // cancellation before the probe's first DATA is released.
                while drops.load(Ordering::SeqCst) < before {
                    changed.notified().await;
                }
                assert_eq!(drops.load(Ordering::SeqCst), before);
                let held = if release_gate {
                    None
                } else {
                    Some(probes.remove(0))
                };
                release_cancellations(probes).await;
                let result = health.await;
                // Own the whole retained gate through the await. Timeout drops
                // it, but the current-thread owner destroys tasks before any
                // newly unblocked server body can run after block_on returns.
                drop(held);
                result
            };
            cancellation_health_timeout(
                controlled,
                &diagnostics,
                Cell {
                    scenario: Scenario::Plain,
                    workload: Workload::Cancel,
                    transport: Transport::H2c,
                },
            )
            .await
        });
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        for connection in 0..2 {
            let expected = reuse_at[connection].load(Ordering::SeqCst) + 2;
            assert_eq!(requests[connection].load(Ordering::SeqCst), expected);
            assert!(expected <= 36);
        }
        if release_gate {
            let measured = result.unwrap();
            let totals = &measured.totals;
            assert_eq!(totals.latencies_us.len(), 32);
            assert!(totals.latencies_us.iter().all(|latency| *latency > 0));
            assert_eq!(totals.body_bytes, (32 * FRAME_BYTES) as u64);
            assert_eq!(totals.errors, 0);
            assert!(totals.error_samples.is_empty());
            assert_eq!(totals.connects, 2);
            assert_eq!(measured.window, Duration::from_secs(5));
            assert_eq!((measured.start, measured.end), (0, 1));
        } else {
            let error = result.err().unwrap();
            assert!(error.is::<tokio::time::error::Elapsed>());
            let state = diagnostics.coordinator.lock().unwrap();
            assert_eq!(state.stage, "worker-joins-and-reuse");
            assert_eq!(state.joined, 3);
            drop(state);
            let mut pending = 0;
            for worker in 0..4 {
                diagnostics.worker(worker, |state| {
                    assert_eq!(state.phase, "reuse");
                    assert_eq!(state.measured, 8);
                    assert_eq!(state.exchanges[2].completed, 8);
                    if state.exchanges[3].completed == 0 {
                        pending += 1;
                        assert_eq!(state.stage, "first-data-frame");
                        assert_eq!(state.exchanges[3].started, 1);
                        assert_eq!(state.exchanges[3].bytes, 0);
                    }
                });
            }
            assert_eq!(pending, 1);
        }
        // Destruction is checked after shutdown, separately from the earlier
        // timeout snapshot. This includes discarded drivers and H2 children.
        runtime.shutdown();
        assert_eq!(metrics.num_alive_tasks(), 0);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        let mut destroyed = 0;
        for worker in 0..4 {
            diagnostics.worker(worker, |state| {
                assert_eq!(state.measured, 8);
                assert_eq!(state.phase, "reuse");
                let driver = &state.connection.as_ref().unwrap().driver;
                assert!(matches!(driver.load(Ordering::Relaxed), 2..=4));
                if state.outcome == "future-dropped" {
                    destroyed += 1;
                    assert_eq!(state.stage, "first-data-frame");
                } else {
                    assert_eq!(state.outcome, "completed-ok");
                    assert_eq!(state.exchanges[3].completed, 1);
                }
            });
        }
        assert_eq!(destroyed, usize::from(!release_gate));
    }

    #[test]
    fn retained_sender_probe_timeout_preserves_reuse_evidence_and_destroys_tasks() {
        retained_sender_probe_gate_case(false);
    }

    #[test]
    fn released_retained_sender_probe_reuses_both_connections() {
        retained_sender_probe_gate_case(true);
    }

    /// Exercise real workers and transport identities without assuming that a
    /// shared runner completes an exchange within a 200 ms benchmark window.
    /// This coordinator exists only in tests; production remains fixed-window.
    pub(crate) fn cancellation_rounds<S>(
        transport: Transport,
        probe: impl Fn() -> S,
    ) -> Measured<S> {
        let runtime = CancellationRuntime::new();
        let rounds = cancellation_rounds_inner(transport, probe, false);
        let result = runtime.block_on(rounds);
        // Keep the owner through every causal assertion. Normal paths join
        // workers and server sets first; timeout also shuts down all async
        // tasks before the error is unwrapped or the next transport can start.
        runtime.shutdown();
        result.unwrap()
    }

    async fn cancellation_rounds_inner<S>(
        transport: Transport,
        probe: impl Fn() -> S,
        warmed: bool,
    ) -> Result<Measured<S>, tokio::time::error::Elapsed> {
        let timeout = Duration::from_secs(10);
        tokio::time::timeout(timeout, async {
            let pki = transport.tls().then(|| Pki::generate().unwrap());
            let tls = pki.as_ref().map(|pki| {
                let config = pki.server(transport.mtls()).unwrap();
                TlsAcceptor::from(config.rustls)
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = Arc::new(Target {
                addr: listener.local_addr().unwrap(),
                transport,
                workload: Workload::Cancel,
                tls: pki.as_ref().map(|pki| pki.client(transport).unwrap()),
            });
            let load = load(4, 2);
            let diagnostics = Arc::new(HealthDiagnostics::new(load));
            let (gates, mut arrivals) = mpsc::unbounded_channel();
            let accepted = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&accepted);
            let (stop, mut stopped) = oneshot::channel();
            // Successful teardown aborts and joins these sets. Timeout or
            // unwind drops them to request abort; the enclosing runtime owner
            // also destroys unjoined connections and H2 executor children.
            let mut servers = JoinSet::new();
            servers.spawn(async move {
                let requests = Arc::new(AtomicUsize::new(0));
                let mut connections = JoinSet::new();
                loop {
                    let (stream, _) = tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => accepted.unwrap(),
                    };
                    stream.set_nodelay(true).unwrap();
                    let connection = observed.fetch_add(1, Ordering::SeqCst);
                    let gates = gates.clone();
                    let requests = Arc::clone(&requests);
                    let service = service_fn(move |_| {
                        let index = requests.fetch_add(1, Ordering::SeqCst);
                        let bytes = match index / load.concurrency {
                            0 => 3 * FRAME_BYTES,
                            1 if warmed => 2 * FRAME_BYTES,
                            1 => FRAME_BYTES,
                            2 if warmed => FRAME_BYTES / 2,
                            3 if warmed => FRAME_BYTES,
                            4 if warmed => 4 * FRAME_BYTES,
                            _ => 2 * FRAME_BYTES,
                        };
                        let (release, receiver) = oneshot::channel();
                        let (cancelled, acknowledgement) = oneshot::channel();
                        gates
                            .send(Cancellation {
                                connection,
                                release,
                                cancelled: acknowledgement,
                            })
                            .unwrap();
                        std::future::ready(Ok::<_, Infallible>(Response::new(GatedBody {
                            release: Some(receiver),
                            cancelled: Some(cancelled),
                            data: Bytes::from(vec![b'x'; bytes]),
                            sent: false,
                        })))
                    });
                    let tls = tls.clone();
                    connections.spawn(async move {
                        let builder = Builder::new(TokioExecutor::new());
                        match tls {
                            None => {
                                let _ = builder
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            }
                            Some(acceptor) => {
                                let stream = acceptor.accept(stream).await.unwrap();
                                let session = stream.get_ref().1;
                                assert_eq!(session.alpn_protocol(), Some(transport.alpn()));
                                if transport.mtls() {
                                    assert!(!session.peer_certificates().unwrap().is_empty());
                                }
                                let _ = builder
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            }
                        }
                    });
                }
                connections.shutdown().await;
            });

            let (phase, receiver) = watch::channel(Phase::Preparing);
            let (ready, mut readiness) = mpsc::unbounded_channel();
            let mut workers = JoinSet::new();
            let mut worker_index = 0;
            for _ in 0..load.connections(transport) {
                let first = worker_index;
                let sender = HEALTH_WORKER
                    .scope((Arc::clone(&diagnostics), first), dial(&target))
                    .await
                    .unwrap();
                let mut connection = None;
                diagnostics.worker(first, |state| connection = state.connection.clone());
                let mut senders = Vec::new();
                if let Sender::H2(shared) = &sender {
                    for _ in 1..load.streams {
                        senders.push(Sender::H2(shared.clone()));
                    }
                }
                senders.push(sender);
                for sender in senders {
                    let index = worker_index;
                    worker_index += 1;
                    if index != first {
                        diagnostics.worker(index, |state| state.connection = connection.clone());
                    }
                    workers.spawn(diagnostic_worker(
                        Arc::clone(&diagnostics),
                        index,
                        worker_inner(
                            Arc::clone(&target),
                            sender,
                            receiver.clone(),
                            ready.clone(),
                            warmed.then_some(HEALTH_EXCHANGES_PER_PHASE),
                        ),
                    ));
                }
            }
            drop(ready);
            drop(receiver);
            let preparation = cancellations(&mut arrivals, load.concurrency).await;
            let preparation = release_cancellations(preparation).await;
            await_readiness(load.concurrency, "startup", &mut readiness, &mut workers)
                .await
                .unwrap();
            let warmup = if warmed {
                // Hold the first DATA until Draining is published. Every
                // worker must finish this distinct warm-up frame before the
                // real readiness rendezvous admits the measurement window.
                phase.send_replace(Phase::Warmup);
                let warmup = cancellations(&mut arrivals, load.concurrency).await;
                phase.send_replace(Phase::Draining);
                Some(release_cancellations(warmup).await)
            } else {
                phase.send_replace(Phase::Draining);
                None
            };
            await_readiness(
                load.concurrency,
                "measurement boundary",
                &mut readiness,
                &mut workers,
            )
            .await
            .unwrap();
            let start = probe();
            let window_start = Instant::now();
            phase.send_replace(Phase::Measuring {
                start: window_start,
                end: window_start + timeout,
            });
            let completed = cancellations(&mut arrivals, load.concurrency).await;
            let completed = release_cancellations(completed).await;
            // Each worker's next request proves it finished and accounted for
            // the previous cancellation, including any HTTP/1 reconnect.
            let crossing = cancellations(&mut arrivals, load.concurrency).await;
            let window_end = Instant::now();
            phase.send_replace(Phase::Measuring {
                start: window_start,
                end: window_end,
            });
            let end = probe();
            // These exchanges began inside the window but can receive their
            // first frame only after its deadline. They must not be counted.
            let crossing = release_cancellations(crossing).await;
            let reuse = if warmed {
                let reuse = cancellations(&mut arrivals, load.concurrency).await;
                Some(release_cancellations(reuse).await)
            } else {
                None
            };
            let mut totals = Totals {
                connects: load.connections(transport) as u64,
                ..Totals::default()
            };
            while let Some(worker) = workers.join_next().await {
                totals.merge(worker.unwrap().unwrap());
            }
            assert_eq!(totals.latencies_us.len(), load.concurrency);
            assert!(totals.latencies_us.iter().all(|latency| *latency > 0));
            let frame_bytes = if warmed { FRAME_BYTES / 2 } else { FRAME_BYTES };
            assert_eq!(totals.body_bytes, (load.concurrency * frame_bytes) as u64);
            assert_eq!(totals.errors, 0);
            assert!(totals.error_samples.is_empty());
            let (requests, bytes) = diagnostics.assert_accounting();
            assert_eq!(totals.latencies_us.len() as u64, requests);
            assert_eq!(totals.body_bytes, bytes);
            assert_eq!(totals.connects, accepted.load(Ordering::SeqCst) as u64);
            if transport.http2() {
                let wires = diagnostics.observer.wires();
                assert_eq!(wires.len(), 2);
                for (index, wire) in wires.iter().enumerate() {
                    assert_eq!(wire.owner, index * 2);
                    assert_eq!(wire.generation, 1);
                    assert!(wire.snapshot().directions[1].data_complete > 0);
                }
                assert_eq!(preparation, [0, 0, 1, 1]);
                assert_eq!(completed, preparation);
                assert_eq!(crossing, preparation);
                assert_eq!(totals.connects, load.connections(transport) as u64);
                if warmed {
                    assert_eq!(warmup.unwrap(), preparation);
                    assert_eq!(reuse.unwrap(), preparation);
                    let mut sockets = [None, None];
                    for index in 0..4 {
                        diagnostics.worker(index, |state| {
                            let [preparation, warmup, measurement, reuse] = state.exchanges;
                            for progress in [preparation, warmup, reuse] {
                                assert_eq!(progress.started, 1);
                                assert_eq!(progress.excluded.count, 1);
                            }
                            assert_eq!(preparation.excluded.bytes, (3 * FRAME_BYTES) as u64);
                            assert_eq!(warmup.excluded.bytes, (2 * FRAME_BYTES) as u64);
                            assert_eq!(measurement.started, 2);
                            assert_eq!(measurement.included.count, 1);
                            assert_eq!(measurement.included.bytes, (FRAME_BYTES / 2) as u64);
                            assert_eq!(measurement.excluded.count, 1);
                            assert_eq!(measurement.excluded.bytes, FRAME_BYTES as u64);
                            assert_eq!(reuse.excluded.bytes, (4 * FRAME_BYTES) as u64);
                            let connection = state.connection.as_ref().unwrap();
                            assert_eq!(connection.owner, (index / 2) * 2);
                            assert_eq!(connection.generation, 1);
                            assert_eq!(state.connect_attempts, u64::from(index.is_multiple_of(2)));
                            assert_eq!(state.sender_closed, Some(false));
                            let socket = connection.local_addr.unwrap();
                            let retained = &mut sockets[index / 2];
                            if let Some(original) = retained {
                                assert_eq!(*original, socket);
                            } else {
                                *retained = Some(socket);
                            }
                            let resets = state.exchanges.iter().map(|p| p.started).sum::<u64>();
                            assert_eq!(resets, 5);
                        });
                    }
                    assert_ne!(sockets[0], sockets[1]);
                    // Exactly ten cancellations per original connection,
                    // including preparation, warm-up and the retained probe.
                    assert_eq!(accepted.load(Ordering::SeqCst), 2);
                }
            } else {
                assert!(diagnostics.observer.wires().is_empty());
                let distinct: std::collections::BTreeSet<_> = preparation
                    .into_iter()
                    .chain(completed)
                    .chain(crossing)
                    .collect();
                assert_eq!(distinct.len(), 3 * load.concurrency);
                assert_eq!(totals.connects, (3 * load.concurrency) as u64);
            }
            stop.send(()).unwrap();
            servers.join_next().await.unwrap().unwrap();
            assert!(workers.is_empty() && servers.is_empty());
            assert!(readiness.is_closed());
            Measured {
                totals,
                window: window_end.duration_since(window_start),
                start,
                end,
            }
        })
        .await
    }

    #[test]
    fn warmed_cancellation_excludes_drained_late_and_reuse_frames() {
        let runtime = CancellationRuntime::new();
        let rounds = cancellation_rounds_inner(Transport::H2c, || (), true);
        let result = runtime.block_on(rounds);
        runtime.shutdown();
        let measured = result.unwrap();
        assert_eq!(measured.totals.latencies_us.len(), 4);
        assert_eq!(measured.totals.body_bytes, (4 * (FRAME_BYTES / 2)) as u64);
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
            let samples = measurement_window(load, &phase, &mut readiness, &mut workers, probe)
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

    /// The lost worker cannot exit until every peer has acknowledged the
    /// chosen stage and parked while retaining its readiness sender.
    async fn interrupted_admission(stage: Phase, reported_error: Option<&'static str>) -> String {
        let mut load = load(4, 2);
        load.duration = Duration::from_millis(200);
        let (phase, receiver) = watch::channel(Phase::Preparing);
        let observed = receiver.clone();
        let (ready, mut readiness) = mpsc::unbounded_channel();
        let (parked, mut arrivals) = mpsc::unbounded_channel();
        let (release, loss) = oneshot::channel();
        let mut workers = JoinSet::<Result<Totals, Failure>>::new();
        for _ in 1..load.concurrency {
            let mut phase = receiver.clone();
            let ready = ready.clone();
            let parked = parked.clone();
            workers.spawn(async move {
                ready.send(Ok(())).unwrap();
                if matches!(stage, Phase::Preparing) {
                    parked.send(()).unwrap();
                }
                loop {
                    phase.changed().await.unwrap();
                    let current = *phase.borrow_and_update();
                    if matches!(current, Phase::Draining) {
                        ready.send(Ok(())).unwrap();
                        if matches!(stage, Phase::Draining) {
                            parked.send(()).unwrap();
                        }
                    }
                }
            });
        }
        let mut lost_phase = receiver.clone();
        let lost_ready = ready.clone();
        workers.spawn(async move {
            if matches!(stage, Phase::Draining) {
                lost_ready.send(Ok(())).unwrap();
                loop {
                    lost_phase.changed().await.unwrap();
                    if matches!(*lost_phase.borrow_and_update(), Phase::Draining) {
                        break;
                    }
                }
            }
            parked.send(()).unwrap();
            loss.await.unwrap();
            match reported_error {
                Some(error) => {
                    lost_ready.send(Err(error.to_owned())).unwrap();
                    // Keep the task alive so this case must propagate the
                    // reported error through readiness, rather than a join.
                    std::future::pending::<Result<Totals, Failure>>().await
                }
                None => panic!("injected worker panic"),
            }
        });
        drop(ready);
        drop(receiver);
        let probes = Arc::new(AtomicUsize::new(0));
        let samples = Arc::clone(&probes);
        let measured = tokio::spawn(async move {
            let result = measure_workers(load, &phase, &mut readiness, &mut workers, 2, || {
                samples.fetch_add(1, Ordering::SeqCst)
            })
            .await;
            // Returning the error includes joining every aborted peer, not
            // merely dropping the set and requesting their cancellation.
            assert!(workers.is_empty());
            assert!(readiness.is_closed());
            result.err().unwrap().to_string()
        });
        for _ in 0..load.concurrency {
            arrivals.recv().await.unwrap();
        }
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert!(!measured.is_finished());
        release.send(()).unwrap();
        let error = measured.await.unwrap();
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert!(!matches!(*observed.borrow(), Phase::Measuring { .. }));
        error
    }

    #[tokio::test]
    async fn startup_worker_panic_aborts_parked_peers_before_measurement() {
        let error = interrupted_admission(Phase::Preparing, None).await;
        assert!(error.starts_with("worker startup: "));
        assert!(error.contains("panicked"));
        assert!(error.contains("injected worker panic"));
    }

    #[tokio::test]
    async fn boundary_worker_panic_aborts_parked_peers_before_measurement() {
        let error = interrupted_admission(Phase::Draining, None).await;
        assert!(error.starts_with("worker measurement boundary: "));
        assert!(error.contains("panicked"));
        assert!(error.contains("injected worker panic"));
    }

    #[tokio::test]
    async fn reported_startup_error_aborts_parked_peers_and_preserves_the_cause() {
        let error =
            interrupted_admission(Phase::Preparing, Some("injected admission failure")).await;
        assert_eq!(error, "worker startup: injected admission failure");
    }

    #[tokio::test]
    async fn reported_boundary_error_aborts_parked_peers_and_preserves_the_cause() {
        let error =
            interrupted_admission(Phase::Draining, Some("injected admission failure")).await;
        assert_eq!(
            error,
            "worker measurement boundary: injected admission failure"
        );
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
            first.error(&std::io::Error::other(format!("first {index}")));
            second.error(&std::io::Error::other(format!("second {index}")));
        }
        first.error(&std::io::Error::other("dropped"));
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
    fn error_samples_preserve_the_protocol_cause() {
        #[derive(Debug)]
        struct TransportError(std::io::Error);

        impl std::fmt::Display for TransportError {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("http2 error")
            }
        }

        impl std::error::Error for TransportError {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let error = TransportError(std::io::Error::other("too_many_internal_resets"));
        let mut totals = Totals::default();
        totals.error(&error);
        assert_eq!(totals.errors, 1);
        assert_eq!(
            totals.error_samples,
            ["http2 error: too_many_internal_resets"]
        );
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
