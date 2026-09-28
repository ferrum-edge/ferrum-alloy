//! The application builder.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use ferrum_alloy_telemetry::peer::SharedClassifier;
use ferrum_alloy_telemetry::{Metrics, RecordRouteLayer, TelemetryLayer, TrustedPeers};
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;
use tower_http::catch_panic::CatchPanicLayer;

use crate::config::{AlloyConfig, ConfigIssue, EdgeMode, Overrides};
#[cfg(feature = "diagnostics")]
use crate::diagnostics::{EvidenceStore, Retrieval};
use crate::error::AlloyError;
use crate::health::{self, HealthCheck, Readiness};
use crate::lifecycle::{self, Lifecycle};
use crate::limits::{AdmissionLayer, BodyLimitLayer, HeadersDeadlineLayer};
use crate::management::{self, ManagementState};
use crate::normalize::{NormalizeLayer, panic_response};
#[cfg(feature = "openapi-ui")]
use crate::openapi_ui::DocsUi;
use crate::rate_limit::RateLimiter;
use crate::server::{self, ServeOptions, ServerStats};

/// Who owns the global tracing subscriber.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TelemetryInit {
    /// Alloy installs a subscriber (logs, and OTLP export when configured)
    /// unless one is already installed. If the application already installed
    /// one and OTLP export is enabled but not bridged, startup fails instead
    /// of silently dropping traces.
    #[default]
    Auto,
    /// The application owns its subscriber. Alloy never touches global
    /// state; compose `ferrum_alloy::telemetry::otel::OtelPipeline::layer`
    /// yourself for export, and leave `otlp.enabled = false`.
    ApplicationOwned,
}

/// Builds and runs an Alloy service around an ordinary axum `Router`.
///
/// ```no_run
/// use axum::{Router, routing::get};
/// use ferrum_alloy::AlloyApp;
///
/// # async fn doc() -> Result<(), Box<dyn std::error::Error>> {
/// let router = Router::new().route("/hello", get(|| async { "Hello from Ferrum Alloy" }));
/// AlloyApp::new("hello-api").router(router).run().await?;
/// # Ok(()) }
/// ```
#[must_use]
pub struct AlloyApp {
    name: String,
    router: Option<Router>,
    config_file: Option<PathBuf>,
    explicit_config: Option<AlloyConfig>,
    overrides: Overrides,
    checks: Vec<(String, Arc<dyn HealthCheck>)>,
    telemetry_init: TelemetryInit,
    classifier: Option<SharedClassifier>,
    shutdown_signal: Option<BoxFuture<'static, ()>>,
    openapi: Option<Arc<Vec<u8>>>,
    #[cfg(feature = "diagnostics")]
    diagnostics: Option<Arc<dyn crate::diagnostics::DiagnosticsAuthorizer>>,
    prepared: Option<Prepared>,
}

struct Prepared {
    config: AlloyConfig,
    warnings: Vec<ConfigIssue>,
    telemetry: TelemetryGuard,
    metrics: Arc<Metrics>,
}

impl std::fmt::Debug for AlloyApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlloyApp")
            .field("name", &self.name)
            .field("config_file", &self.config_file)
            .field("telemetry_init", &self.telemetry_init)
            .finish_non_exhaustive()
    }
}

impl AlloyApp {
    /// A new application named `name` (an explicit `service.name` override).
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let mut overrides = Overrides::default();
        overrides.set(&["service", "name"], toml::Value::String(name.clone()));
        Self {
            name,
            router: None,
            config_file: None,
            explicit_config: None,
            overrides,
            checks: Vec::new(),
            telemetry_init: TelemetryInit::Auto,
            classifier: None,
            shutdown_signal: None,
            openapi: None,
            #[cfg(feature = "diagnostics")]
            diagnostics: None,
            prepared: None,
        }
    }

    /// The application router, with its state already supplied
    /// (`Router::with_state`). Its handlers, extractors, and layers are used
    /// as-is.
    ///
    /// Alloy's own paths on the application listener (the health endpoints,
    /// and with `openapi.public` the OpenAPI document and UI) take precedence
    /// over it, so startup fails with [`AlloyError::ShadowedRoute`] when one
    /// of its routes matches one of those paths, for any method. Finding out
    /// runs none of the router's handlers, layers, or fallbacks.
    pub fn router(mut self, router: Router) -> Self {
        self.router = Some(router);
        self
    }

    /// Reads configuration from this TOML file (it must exist). Without it,
    /// `FERRUM_ALLOY_CONFIG` may name a file; otherwise no file is read.
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Uses this configuration as-is: no file or environment is read.
    pub fn config(mut self, config: AlloyConfig) -> Self {
        self.explicit_config = Some(config);
        self
    }

    /// Overrides `server.bind`.
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.overrides
            .set(&["server", "bind"], toml::Value::String(addr.to_string()));
        self
    }

    /// Overrides `management.bind`.
    pub fn management_bind(mut self, addr: SocketAddr) -> Self {
        self.overrides.set(
            &["management", "bind"],
            toml::Value::String(addr.to_string()),
        );
        self
    }

    /// Overrides `service.version` (typically `env!("CARGO_PKG_VERSION")`).
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.overrides
            .set(&["service", "version"], toml::Value::String(version.into()));
        self
    }

    /// Registers a cached readiness check.
    pub fn readiness_check(mut self, name: impl Into<String>, check: impl HealthCheck) -> Self {
        self.checks.push((name.into(), Arc::new(check)));
        self
    }

    /// Chooses who owns the global subscriber.
    pub fn telemetry(mut self, init: TelemetryInit) -> Self {
        self.telemetry_init = init;
        self
    }

    /// Replaces the peer trust classifier built from `[trust]`.
    pub fn trust_classifier(mut self, classifier: SharedClassifier) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Replaces the OS shutdown signal (SIGTERM/SIGINT/Ctrl-C).
    pub fn shutdown_signal(mut self, signal: impl Future<Output = ()> + Send + 'static) -> Self {
        self.shutdown_signal = Some(Box::pin(signal));
        self
    }

    /// Registers the OpenAPI document served on the management listener.
    #[cfg(feature = "openapi")]
    pub fn openapi(mut self, document: &utoipa::openapi::OpenApi) -> Self {
        self.openapi = document
            .to_json()
            .ok()
            .map(|json| Arc::new(json.into_bytes()));
        self
    }

    /// Enables authorized, tenant-scoped diagnostic retrieval (ADR 0008).
    ///
    /// Requests the application tags with a tenant
    /// ([`crate::diagnostics::TenantTag`]) are retained in memory within the
    /// `[diagnostics]` bounds, and `GET /diagnostics/v1/requests/{request_id}`
    /// on the management listener serves one tenant's evidence to callers
    /// that `authorizer` admits for that tenant. Startup fails when the
    /// management listener or its rate limit is disabled.
    #[cfg(feature = "diagnostics")]
    pub fn diagnostics_authorizer(
        mut self,
        authorizer: impl crate::diagnostics::DiagnosticsAuthorizer,
    ) -> Self {
        self.diagnostics = Some(Arc::new(authorizer));
        self
    }

    /// Loads and validates configuration and initializes telemetry. Call it
    /// before building resources that need configuration (database pools);
    /// later calls return the same configuration.
    pub fn prepare(&mut self) -> Result<&AlloyConfig, AlloyError> {
        if self.prepared.is_none() {
            let config = match self.explicit_config.take() {
                Some(mut config) => {
                    if config.service.name.is_none() {
                        config.service.name = Some(self.name.clone());
                    }
                    config
                }
                None => AlloyConfig::load(self.config_file.as_deref(), &self.overrides)?.0,
            };
            let warnings = config.validate(crate::enabled_features())?;
            let metrics = Arc::new(Metrics::default());
            let telemetry = TelemetryGuard::init(
                self.telemetry_init,
                &config,
                &self.name,
                Arc::clone(&metrics),
            )?;
            for warning in &warnings {
                tracing::warn!(target: "ferrum_alloy::config", "{}", warning.message);
            }
            self.prepared = Some(Prepared {
                config,
                warnings,
                telemetry,
                metrics,
            });
        }
        match &self.prepared {
            Some(prepared) => Ok(&prepared.config),
            None => Err(AlloyError::Internal(
                "configuration was not prepared".into(),
            )),
        }
    }

    /// Composes everything without serving: the escape hatch for serving the
    /// router yourself. Keep the returned parts alive: they own the telemetry
    /// pipeline and lifecycle.
    pub fn into_parts(mut self) -> Result<AlloyParts, AlloyError> {
        self.prepare()?;
        let Some(prepared) = self.prepared.take() else {
            return Err(AlloyError::Internal(
                "configuration was not prepared".into(),
            ));
        };
        let user_router = self.router.take().ok_or(AlloyError::MissingRouter)?;
        let config = prepared.config;
        let lifecycle = Lifecycle::new(Arc::clone(&prepared.metrics));
        let classifier: SharedClassifier = match self.classifier.take() {
            Some(classifier) => classifier,
            None => Arc::new(
                TrustedPeers::new(&config.trust)
                    .map_err(|e| AlloyError::Internal(e.to_string()))?,
            ),
        };
        let telemetry_layer = TelemetryLayer::new(config.telemetry.clone())
            .map_err(|e| AlloyError::Internal(e.to_string()))?
            .with_classifier(Arc::clone(&classifier))
            .with_metrics(Arc::clone(&prepared.metrics));
        #[cfg(feature = "diagnostics")]
        let evidence = match self.diagnostics.take() {
            Some(authorizer) => {
                crate::diagnostics::check_config(&config)?;
                let store = Arc::new(EvidenceStore::new(&config.diagnostics));
                Some((authorizer, store))
            }
            None => None,
        };
        #[cfg(feature = "diagnostics")]
        let telemetry_layer = match &evidence {
            Some((_, store)) => telemetry_layer.with_evidence_sink(store.clone()),
            None => telemetry_layer,
        };
        let readiness = Arc::new(Readiness::new(
            std::mem::take(&mut self.checks),
            Duration::from_millis(config.health.cache_ttl_ms),
            Duration::from_millis(config.health.check_timeout_ms),
        ));

        let mut app = Router::new();
        // Alloy's paths on the application listener, with the setting that
        // places each there. They take precedence over the application's
        // router, so none may match one of its routes.
        let mut served: Vec<(&'static str, String)> = Vec::new();
        if config.health.app_endpoints {
            let health = &config.health;
            served.push(("health.liveness_path", health.liveness_path.clone()));
            served.push(("health.readiness_path", health.readiness_path.clone()));
            let (r, l) = (Arc::clone(&readiness), lifecycle.clone());
            app = app
                .route(
                    &config.health.liveness_path,
                    get(|| async { health::liveness() }),
                )
                .route(
                    &config.health.readiness_path,
                    get(move || {
                        let (r, l) = (Arc::clone(&r), l.clone());
                        async move { health::readiness(&r, &l).await }
                    }),
                );
        }
        // The documentation UI goes wherever the document is served.
        #[cfg(feature = "openapi-ui")]
        let openapi_ui = {
            let openapi = &config.openapi;
            let served = openapi.ui && openapi.serve && self.openapi.is_some();
            served.then(|| DocsUi::new(&openapi.ui_path, &openapi.path))
        };
        if config.openapi.public
            && config.openapi.serve
            && let Some(document) = &self.openapi
        {
            served.push(("openapi.path", config.openapi.path.clone()));
            let document = Arc::clone(document);
            app = app.route(
                &config.openapi.path,
                get(move || {
                    let document = Arc::clone(&document);
                    async move { management::openapi_response(Some(&document)) }
                }),
            );
            #[cfg(feature = "openapi-ui")]
            if let Some(ui) = &openapi_ui {
                served.extend(ui.paths().map(|path| ("openapi.ui_path", path)));
                app = app.merge(ui.routes::<()>());
            }
        }
        crate::shadow::check(&user_router, &served)?;
        let limit = config.server.request_body_limit_bytes;
        #[allow(unused_mut, reason = "optional layers are feature-gated")]
        let mut app = app
            .fallback_service(user_router.layer(RecordRouteLayer))
            .layer(NormalizeLayer)
            .layer(HeadersDeadlineLayer::new(Duration::from_millis(
                config.server.request_timeout_ms,
            )))
            .layer(AdmissionLayer::new(
                config.server.max_in_flight_requests,
                Duration::from_millis(config.server.admission_wait_timeout_ms),
            ))
            .layer(DefaultBodyLimit::max(
                usize::try_from(limit).unwrap_or(usize::MAX),
            ))
            .layer(BodyLimitLayer::new(limit))
            .layer(CatchPanicLayer::custom(panic_response));
        #[cfg(feature = "compression")]
        if config.compression.enabled {
            app = app.layer(crate::http_layers::compression(&config.compression));
        }
        #[cfg(feature = "cors")]
        if config.cors.enabled {
            app = app.layer(crate::http_layers::cors(&config.cors)?);
        }
        #[cfg(feature = "edge")]
        {
            let mut policy_config = ferrum_alloy_edge::EdgePolicyConfig::default();
            policy_config.mode = match config.edge.mode {
                EdgeMode::Standalone => ferrum_alloy_edge::DeploymentMode::Standalone,
                EdgeMode::GatewayPreferred => ferrum_alloy_edge::DeploymentMode::GatewayPreferred,
                EdgeMode::GatewayRequired => ferrum_alloy_edge::DeploymentMode::GatewayRequired,
            };
            policy_config.accept_consumer_identity = config.edge.accept_consumer_identity;
            let exempt_paths = config.health.app_endpoints.then(|| {
                vec![
                    config.health.liveness_path.clone(),
                    config.health.readiness_path.clone(),
                ]
            });
            let mut policy =
                ferrum_alloy_edge::EdgePolicy::new(policy_config, Arc::clone(&classifier));
            if let Some(paths) = exempt_paths {
                policy = policy.with_exempt_paths(paths);
            }
            app = app.layer(ferrum_alloy_edge::EdgeLayer::new(policy));
        }
        #[cfg(not(feature = "edge"))]
        let _ = EdgeMode::Standalone;
        let app = app.layer(telemetry_layer);

        let app_stats = Arc::new(ServerStats::default());
        let service_name = config
            .service
            .name
            .clone()
            .unwrap_or_else(|| self.name.clone());
        let management_router = config.management.enabled.then(|| {
            let rate_limit = &config.management.rate_limit;
            let rate_limiter = rate_limit
                .enabled
                .then(|| Arc::new(RateLimiter::new(rate_limit)));
            management::router(
                ManagementState {
                    readiness: Arc::clone(&readiness),
                    lifecycle: lifecycle.clone(),
                    token: config.management.token.clone(),
                    service: service_name.clone(),
                    version: config.service.version.clone(),
                    app_stats: Arc::clone(&app_stats),
                    openapi: self.openapi.clone().filter(|_| config.openapi.serve),
                    #[cfg(feature = "openapi-ui")]
                    openapi_ui,
                    rate_limiter,
                    #[cfg(feature = "diagnostics")]
                    diagnostics: evidence.map(|(authorizer, store)| Retrieval {
                        authorizer,
                        store,
                        service: service_name.clone(),
                    }),
                },
                &config.openapi.path,
            )
        });

        #[cfg(feature = "tls")]
        let tls = config
            .server
            .tls
            .as_ref()
            .map(crate::tls::load)
            .transpose()
            .map_err(|e| AlloyError::Tls(e.to_string()))?;

        Ok(AlloyParts {
            router: app,
            management_router,
            lifecycle,
            config,
            warnings: prepared.warnings,
            telemetry: prepared.telemetry,
            service_name,
            shutdown_signal: self.shutdown_signal.take(),
            app_stats,
            #[cfg(feature = "tls")]
            tls,
        })
    }

    /// Binds, serves, and shuts down gracefully on SIGTERM/SIGINT.
    pub async fn run(self) -> Result<(), AlloyError> {
        self.into_parts()?.serve().await
    }
}

/// Everything [`AlloyApp`] composed. Serve [`AlloyParts::router`] yourself,
/// or call [`AlloyParts::serve`].
#[must_use = "dropping the parts shuts down telemetry export"]
pub struct AlloyParts {
    /// The composed application router: health endpoints, Alloy middleware,
    /// and your routes. Serve it with a listener that inserts
    /// `ferrum_alloy::telemetry::PeerInfo` (or axum `ConnectInfo`) so peer
    /// trust can be evaluated.
    pub router: Router,
    /// The management router, when enabled. Serve it with a listener that
    /// inserts `ferrum_alloy::telemetry::PeerInfo` (or axum `ConnectInfo`):
    /// its rate limits key clients by that transport address, and requests
    /// without either all share one budget. Behind a proxy or sidecar, every
    /// client is the proxy's address; Istio connects from 127.0.0.6. Add that
    /// address to `management.rate_limit.exempt_networks` only if bypassing
    /// the limits for all proxied clients is intended.
    pub management_router: Option<Router>,
    /// Shutdown coordination.
    pub lifecycle: Lifecycle,
    /// The validated configuration.
    pub config: AlloyConfig,
    /// Non-fatal configuration warnings.
    pub warnings: Vec<ConfigIssue>,
    /// Owns the telemetry pipeline. Call [`TelemetryGuard::shutdown`] at exit.
    pub telemetry: TelemetryGuard,
    service_name: String,
    shutdown_signal: Option<BoxFuture<'static, ()>>,
    app_stats: Arc<ServerStats>,
    #[cfg(feature = "tls")]
    tls: Option<crate::tls::TlsServer>,
}

impl std::fmt::Debug for AlloyParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlloyParts")
            .field("service", &self.service_name)
            .field("management", &self.management_router.is_some())
            .finish_non_exhaustive()
    }
}

impl AlloyParts {
    /// Connection counters of the application listener, as rendered on the
    /// management `/metrics` endpoint. They stay readable after serving
    /// returns.
    pub fn app_stats(&self) -> Arc<ServerStats> {
        Arc::clone(&self.app_stats)
    }

    /// Binds the configured addresses and serves until shutdown.
    pub async fn serve(self) -> Result<(), AlloyError> {
        let app = TcpListener::bind(self.config.server.bind)
            .await
            .map_err(|source| AlloyError::Bind {
                listener: "application",
                addr: self.config.server.bind,
                source,
            })?;
        let management = match &self.management_router {
            Some(_) => Some(
                TcpListener::bind(self.config.management.bind)
                    .await
                    .map_err(|source| AlloyError::Bind {
                        listener: "management",
                        addr: self.config.management.bind,
                        source,
                    })?,
            ),
            None => None,
        };
        self.serve_on(app, management).await
    }

    /// Serves on already-bound listeners (useful for tests and socket
    /// activation).
    ///
    /// After shutdown it returns only once every connection socket on both
    /// listeners is closed and no HTTP/2 stream handler is still running:
    /// connections still open when `shutdown.drain_timeout_ms` runs out are
    /// force-closed and stream tasks still running are cancelled, and it
    /// also waits for those tasks to finish unwinding. Upgraded (WebSocket)
    /// sessions are not connections here; see [`Lifecycle::shutdown_token`].
    pub async fn serve_on(
        mut self,
        app_listener: TcpListener,
        management_listener: Option<TcpListener>,
    ) -> Result<(), AlloyError> {
        let config = &self.config;
        let options = ServeOptions {
            name: "application",
            max_connections: config.server.max_connections,
            max_header_count: config.server.max_header_count,
            max_header_bytes: config.server.max_header_bytes,
            http2_max_concurrent_streams: config.server.http2_max_concurrent_streams,
            header_read_timeout: Duration::from_millis(config.server.header_read_timeout_ms),
            idle_timeout: Duration::from_millis(config.server.idle_timeout_ms),
            write_stall_timeout: Duration::from_millis(config.server.write_stall_timeout_ms),
            drain_timeout: Duration::from_millis(config.shutdown.drain_timeout_ms),
            #[cfg(feature = "tls")]
            tls: self.tls.clone(),
        };
        let app_addr = app_listener.local_addr().ok();
        let management_addr = management_listener
            .as_ref()
            .and_then(|l| l.local_addr().ok());
        tracing::info!(
            target: "ferrum_alloy::lifecycle",
            service = %self.service_name,
            listen = ?app_addr,
            management = ?management_addr,
            tls = config.server.tls.is_some(),
            features = ?crate::enabled_features(),
            "serving"
        );
        let app_task = tokio::spawn(server::serve(
            app_listener,
            self.router.clone(),
            options.clone(),
            self.lifecycle.clone(),
            Arc::clone(&self.app_stats),
        ));
        // New handshakes keep getting reloaded material until accepting stops.
        #[cfg(feature = "tls")]
        if let Some(tls) = self.tls.clone() {
            tokio::spawn(crate::tls::reload_until(
                tls,
                Arc::clone(&self.app_stats),
                self.lifecycle.stop_accepting().clone(),
            ));
        }
        let management_task = match (management_listener, self.management_router.clone()) {
            (Some(listener), Some(router)) => Some(tokio::spawn(server::serve(
                listener,
                router,
                ServeOptions {
                    name: "management",
                    max_connections: 64,
                    #[cfg(feature = "tls")]
                    tls: None,
                    ..options
                },
                self.lifecycle.clone(),
                Arc::new(ServerStats::default()),
            ))),
            _ => None,
        };

        let signal = self.shutdown_signal.take();
        let token = self.lifecycle.shutdown_token();
        tokio::select! {
            () = async {
                match signal {
                    Some(signal) => signal.await,
                    None => lifecycle::shutdown_signal().await,
                }
            } => {}
            () = token.cancelled() => {}
        }
        self.lifecycle.trigger_shutdown();
        let grace = Duration::from_millis(self.config.shutdown.readiness_grace_ms);
        tracing::info!(target: "ferrum_alloy::lifecycle", grace_ms = grace.as_millis() as u64, "shutdown started; readiness reports draining");
        if !grace.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(grace) => {}
                () = lifecycle::shutdown_signal() => {
                    tracing::warn!(target: "ferrum_alloy::lifecycle", "second shutdown signal; skipping readiness grace");
                }
            }
        }
        self.lifecycle.stop_accepting().cancel();
        let mut result = Ok(());
        for task in std::iter::once(app_task).chain(management_task) {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(AlloyError::Serve(error)),
                Err(error) => {
                    result = Err(AlloyError::Internal(format!(
                        "listener task failed: {error}"
                    )))
                }
            }
        }
        let flush = Duration::from_millis(self.config.shutdown.telemetry_flush_timeout_ms);
        if let Err(error) = self.telemetry.shutdown(flush) {
            tracing::warn!(target: "ferrum_alloy::lifecycle", %error, "telemetry flush incomplete");
        }
        tracing::info!(target: "ferrum_alloy::lifecycle", "shutdown complete");
        result
    }
}

/// Owns telemetry resources created by Alloy.
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    pipeline: Option<ferrum_alloy_telemetry::otel::OtelPipeline>,
    /// How telemetry was set up (`alloy`, `application`, `existing`).
    pub mode: &'static str,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl TelemetryGuard {
    fn init(
        mode: TelemetryInit,
        config: &AlloyConfig,
        name: &str,
        metrics: Arc<Metrics>,
    ) -> Result<Self, AlloyError> {
        use ferrum_alloy_telemetry::init;
        let _ = (name, &metrics);
        if mode == TelemetryInit::ApplicationOwned {
            if config.otlp.enabled {
                return Err(AlloyError::Telemetry(
                    "otlp.enabled requires Alloy-managed telemetry; with TelemetryInit::ApplicationOwned compose OtelPipeline::layer into your own subscriber and set otlp.enabled = false".into(),
                ));
            }
            return Ok(Self::external("application"));
        }
        if init::global_subscriber_installed() {
            #[cfg(feature = "otel")]
            let bridged = ferrum_alloy_telemetry::otel::layer_active();
            #[cfg(not(feature = "otel"))]
            let bridged = false;
            if config.otlp.enabled && !bridged {
                return Err(AlloyError::Telemetry(
                    "a global tracing subscriber is already installed without an OpenTelemetry layer, so OTLP export would silently lose traces; use TelemetryInit::ApplicationOwned and compose OtelPipeline::layer, or let Alloy install the subscriber".into(),
                ));
            }
            return Ok(Self::external("existing"));
        }
        #[cfg(feature = "otel")]
        if config.otlp.enabled {
            let resource = ferrum_alloy_telemetry::otel::ServiceResource {
                name: config
                    .service
                    .name
                    .clone()
                    .unwrap_or_else(|| name.to_owned()),
                version: config.service.version.clone(),
                instance_id: None,
                environment: Some(config.service.environment.clone()),
            };
            let mut otlp = ferrum_alloy_telemetry::otel::OtlpConfig::default();
            otlp.enabled = true;
            otlp.endpoint = config.otlp.endpoint.clone();
            otlp.timeout_ms = config.otlp.timeout_ms;
            otlp.max_export_retries = config.otlp.max_export_retries;
            otlp.sampling_ratio = config.otlp.sampling_ratio;
            otlp.max_queue_spans = config.otlp.max_queue_spans;
            otlp.max_queue_bytes = config.otlp.max_queue_bytes;
            otlp.max_export_batch = config.otlp.max_export_batch;
            otlp.max_request_bytes = config.otlp.max_request_bytes;
            otlp.scheduled_delay_ms = config.otlp.scheduled_delay_ms;
            let pipeline = init::init_logging_and_otel(&config.logging, &resource, &otlp, metrics)
                .map_err(|e| AlloyError::Telemetry(e.to_string()))?;
            return Ok(Self {
                pipeline: Some(pipeline),
                mode: "alloy",
            });
        }
        init::init_logging(&config.logging).map_err(|e| AlloyError::Telemetry(e.to_string()))?;
        Ok(Self::external("alloy"))
    }

    fn external(mode: &'static str) -> Self {
        Self {
            #[cfg(feature = "otel")]
            pipeline: None,
            mode,
        }
    }

    /// Flushes and stops export, waiting at most `timeout`.
    pub fn shutdown(&mut self, timeout: Duration) -> Result<(), String> {
        #[cfg(feature = "otel")]
        if let Some(pipeline) = self.pipeline.take() {
            return pipeline.shutdown(timeout).map_err(|e| e.to_string());
        }
        let _ = timeout;
        Ok(())
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_secs(1));
    }
}
