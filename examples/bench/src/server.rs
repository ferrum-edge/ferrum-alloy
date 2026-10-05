//! The servers under test, and the OTLP collector stub, each on its own
//! multi-threaded Tokio runtime.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::diagnostics::{DiagnosticsAccess, DiagnosticsRequest, TenantTag};
use ferrum_alloy::telemetry::otel::OtelPipeline;
use ferrum_alloy::{AlloyApp, TelemetryInit};
use hyper::body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

use crate::Failure;
use crate::alloc::{self, Role};
use crate::dims::{CANCEL_FRAMES, FRAME_BYTES, LARGE_BYTES, STREAM_FRAMES, Scenario};
use crate::pki::ServerTls;
use crate::probe::{COLLECTOR_THREAD, SERVER_THREAD};

/// Worker threads of the server runtime (and, separately, the client's).
pub(crate) const THREADS: usize = 4;

static FRAME: [u8; FRAME_BYTES] = [b'x'; FRAME_BYTES];

/// A response body of `remaining` 1 KiB frames with no known length, so
/// HTTP/1.1 sends it chunked and every frame passes through the body
/// wrappers individually.
struct Frames {
    remaining: usize,
}

impl Frames {
    fn body(frames: usize) -> Body {
        Body::new(Self { remaining: frames })
    }
}

impl hyper::body::Body for Frames {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        self.remaining -= 1;
        Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(&FRAME)))))
    }
}

async fn small() -> axum::Json<serde_json::Value> {
    let body = serde_json::json!({ "id": 42, "name": "tea", "quantity": 2, "tags": ["a", "b"] });
    axum::Json(body)
}

async fn stream() -> Body {
    Frames::body(STREAM_FRAMES)
}

async fn stream_long() -> Body {
    Frames::body(CANCEL_FRAMES)
}

pub(crate) fn router() -> Router {
    let large = Bytes::from(vec![b'x'; LARGE_BYTES]);
    Router::new()
        .route("/small", get(small))
        .route("/large", get(move || std::future::ready(large.clone())))
        .route("/stream", get(stream))
        .route("/stream-long", get(stream_long))
}

/// Attributes every request to one tenant, so its evidence is retained.
async fn tag_tenant(tenant: TenantTag, request: Request, next: Next) -> Response {
    tenant.set("bench");
    next.run(request).await
}

/// Retrieval is not measured, so no caller is admitted.
async fn deny(_: DiagnosticsRequest) -> DiagnosticsAccess {
    DiagnosticsAccess::Deny
}

/// Reports the listener's address once the server is ready, or why it
/// could not start.
struct Ready {
    sender: mpsc::Sender<Result<SocketAddr, String>>,
    addr: SocketAddr,
}

impl Ready {
    fn ok(self) {
        let _ = self.sender.send(Ok(self.addr));
    }

    fn fail(self, error: impl std::fmt::Display) {
        let _ = self.sender.send(Err(error.to_string()));
    }
}

/// Resolves when the owning [`Server`] is dropped.
async fn stopped(stop: oneshot::Receiver<()>) {
    let _ = stop.await;
}

/// A running server. Dropping it stops the server; its runtime then shuts
/// down in the background.
pub(crate) struct Server {
    pub(crate) addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

/// Runs `serve` on a new runtime whose threads are named `name` and
/// attributed to `role`, and waits until it calls [`Ready`]. `serve` should
/// return once its stop receiver resolves.
fn spawn<F, Fut>(name: &'static str, role: Role, serve: F) -> Result<Server, Failure>
where
    F: FnOnce(TcpListener, Ready, oneshot::Receiver<()>) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let (sender, receiver) = mpsc::channel();
    let (stop, stop_receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            alloc::set_role(role);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(THREADS)
                .thread_name(name)
                .on_thread_start(move || alloc::set_role(role))
                .enable_all()
                .build();
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match TcpListener::bind("127.0.0.1:0").await {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        return;
                    }
                };
                let addr = match listener.local_addr() {
                    Ok(addr) => addr,
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        return;
                    }
                };
                serve(listener, Ready { sender, addr }, stop_receiver).await;
            });
        })?;
    match receiver.recv() {
        Ok(Ok(addr)) => Ok(Server { addr, _stop: stop }),
        Ok(Err(error)) => Err(format!("{name}: {error}").into()),
        Err(_) => Err(format!("{name} exited before it was ready").into()),
    }
}

/// Starts the server for `scenario`. `pipeline` is kept alive for as long
/// as the server runs.
pub(crate) fn start(
    scenario: Scenario,
    tls: Option<ServerTls>,
    pipeline: Option<OtelPipeline>,
    #[cfg(test)] health: Option<Arc<crate::client::tests::HealthDiagnostics>>,
) -> Result<Server, Failure> {
    spawn(
        SERVER_THREAD,
        Role::Service,
        move |listener, ready, stop| async move {
            let _pipeline = pipeline;
            if scenario == Scenario::Plain {
                ready.ok();
                let tls = tls.map(|tls| TlsAcceptor::from(tls.rustls));
                serve_plain(
                    listener,
                    tls,
                    stopped(stop),
                    #[cfg(test)]
                    health,
                )
                .await;
                return;
            }
            let diagnostics = scenario == Scenario::AlloyDiagnostics;
            let mut config = AlloyConfig::default();
            // Retrieval needs the management router; it is built but never
            // served, since only the application listener is measured.
            config.management.enabled = diagnostics;
            config.server.max_connections = 100_000;
            config.server.tls = tls.map(|tls| tls.alloy);
            let app = AlloyApp::new("bench")
                .config(config)
                .telemetry(TelemetryInit::ApplicationOwned)
                .shutdown_signal(stopped(stop));
            let router = router();
            #[cfg(test)]
            let router = observe_router(router, health.clone());
            let app = if diagnostics {
                let router = router.layer(middleware::from_fn(tag_tenant));
                app.router(router).diagnostics_authorizer(deny)
            } else {
                app.router(router)
            };
            let parts = app.into_parts();
            match parts {
                Ok(parts) => {
                    #[cfg(test)]
                    let parts = match health {
                        Some(health) => {
                            parts.bench_plaintext_observer(Arc::new(ServerWire(health)))
                        }
                        None => parts,
                    };
                    ready.ok();
                    let _ = parts.serve_on(listener, None).await;
                }
                Err(error) => ready.fail(error),
            }
        },
    )
}

/// The baseline: hyper-util's automatic HTTP/1.1 + HTTP/2 connection
/// builder (what `axum::serve` uses) with `TCP_NODELAY`, as Alloy sets it,
/// and optional TLS.
async fn serve_plain(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    stop: impl Future,
    #[cfg(test)] health: Option<Arc<crate::client::tests::HealthDiagnostics>>,
) {
    let router = router();
    #[cfg(test)]
    let router = observe_router(router, health.clone());
    let mut stop = std::pin::pin!(stop);
    loop {
        let accepted = tokio::select! {
            _ = &mut stop => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _peer)) = accepted else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        #[cfg(test)]
        let observation = health
            .as_ref()
            .and_then(|health| health.server_wire(stream.local_addr().ok(), _peer, tls.is_some()));
        #[cfg(test)]
        let router = router
            .clone()
            .layer(axum::Extension(axum::extract::ConnectInfo(_peer)));
        let service = TowerToHyperService::new(router.clone());
        let tls = tls.clone();
        tokio::spawn(async move {
            let builder = Builder::new(TokioExecutor::new());
            match tls {
                None => {
                    #[cfg(test)]
                    let stream = {
                        let mut stream = ferrum_alloy::bench_diagnostics::SocketIo::new(stream);
                        stream.observe(observation.clone().map(|wire| {
                            wire as Arc<dyn ferrum_alloy::bench_diagnostics::IoObserver>
                        }));
                        stream
                    };
                    #[cfg(test)]
                    let stream = ferrum_alloy::bench_diagnostics::PlaintextIo::optional(
                        stream,
                        observation.map(|wire| {
                            wire as Arc<dyn ferrum_alloy::bench_diagnostics::IoObserver>
                        }),
                    );
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                }
                Some(acceptor) => {
                    #[cfg(test)]
                    let stream = ferrum_alloy::bench_diagnostics::SocketIo::new(stream);
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    #[cfg(test)]
                    let stream = {
                        let mut stream = stream;
                        let observer = observation.clone().map(|wire| {
                            wire as Arc<dyn ferrum_alloy::bench_diagnostics::IoObserver>
                        });
                        stream.get_mut().0.observe(observer.clone());
                        ferrum_alloy::bench_diagnostics::TlsIo::optional(stream.into(), observer)
                    };
                    #[cfg(test)]
                    let stream = ferrum_alloy::bench_diagnostics::PlaintextIo::optional(
                        stream,
                        observation.map(|wire| {
                            wire as Arc<dyn ferrum_alloy::bench_diagnostics::IoObserver>
                        }),
                    );
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                }
            }
        });
    }
}

#[cfg(test)]
struct ServerWire(Arc<crate::client::tests::HealthDiagnostics>);

#[cfg(test)]
impl std::fmt::Debug for ServerWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("instance-owned benchmark server observer")
    }
}

#[cfg(test)]
impl ferrum_alloy::bench_diagnostics::ConnectionObserver for ServerWire {
    fn accepted(
        &self,
        local: Option<SocketAddr>,
        remote: SocketAddr,
        tls: bool,
    ) -> Option<Arc<dyn ferrum_alloy::bench_diagnostics::IoObserver>> {
        self.0
            .server_wire(local, remote, tls)
            .map(|wire| wire as Arc<dyn ferrum_alloy::bench_diagnostics::IoObserver>)
    }
}

#[cfg(test)]
fn observe_router(
    router: Router,
    health: Option<Arc<crate::client::tests::HealthDiagnostics>>,
) -> Router {
    let Some(health) = health else {
        return router;
    };
    router.layer(middleware::from_fn(move |request: Request, next: Next| {
        let socket = request
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|info| info.0);
        let observation = health.observer.request(socket);
        crate::health::response(next.run(request), observation)
    }))
}

/// What the collector stub received.
#[derive(Debug, Default)]
pub(crate) struct CollectorStats {
    pub(crate) requests: AtomicU64,
    pub(crate) bytes: AtomicU64,
}

/// An OTLP/HTTP endpoint that accepts every export with `200 OK` and an
/// empty `ExportTraceServiceResponse`. It stands in for a healthy
/// Collector without decoding or forwarding anything.
pub(crate) struct Collector {
    pub(crate) endpoint: String,
    pub(crate) stats: Arc<CollectorStats>,
    _server: Server,
}

async fn export(State(stats): State<Arc<CollectorStats>>, body: Bytes) -> impl IntoResponse {
    stats.requests.fetch_add(1, Ordering::Relaxed);
    stats.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
    let content_type = [(http::header::CONTENT_TYPE, "application/x-protobuf")];
    (content_type, Bytes::new())
}

pub(crate) fn start_collector() -> Result<Collector, Failure> {
    let stats = Arc::new(CollectorStats::default());
    let recorded = Arc::clone(&stats);
    let server = spawn(
        COLLECTOR_THREAD,
        Role::Collector,
        move |listener, ready, stop| {
            let app = Router::new()
                .route("/v1/traces", post(export))
                .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
                .with_state(recorded);
            async move {
                ready.ok();
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(stopped(stop))
                    .await;
            }
        },
    )?;
    Ok(Collector {
        endpoint: format!("http://{}/v1/traces", server.addr),
        stats,
        _server: server,
    })
}
