//! Instrumented outbound HTTP client (feature `http-client`).
//!
//! * Explicit connect and whole-request timeouts.
//! * `traceparent` and the accepted `tracestate` are sent only to hosts listed
//!   in `http_client.propagate_trace_context_to`. Caller-supplied trace headers
//!   are replaced, never forwarded. Credentials, cookies, `baggage`, and
//!   gateway identity are never added automatically.
//! * Redirects are off by default. When enabled, only same-origin redirects
//!   are followed; a cross-origin redirect is returned to the caller.
//! * No automatic retries: a retry needs an explicit policy, a replayable
//!   body, and awareness of the gateway's own retries.
//! * TLS uses rustls with the `ring` provider and the platform trust store.
//!
//! The client span measures until response **headers**; reading the body is
//! outside it. The span is a CLIENT span parented by the request's server
//! span when a [`RequestContext`] is supplied.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrum_alloy_telemetry::RequestContext;
use ferrum_alloy_telemetry::metrics::method_label;
use ferrum_alloy_telemetry::trace_context::{SpanId, TRACEPARENT, TRACESTATE};
use reqwest::redirect;
use tracing::Instrument;
use tracing::field::Empty;

use crate::config::HttpClientSettings;
use crate::error::AlloyError;

#[derive(Debug, Clone)]
enum HostRule {
    Exact(String),
    Suffix(String),
}

impl HostRule {
    fn parse(rule: &str) -> Result<Self, AlloyError> {
        let rule = rule.trim().to_ascii_lowercase();
        if rule.is_empty() || rule.contains(['/', ':', ' ', '*', '@']) {
            return Err(AlloyError::Integration(format!(
                "http_client.propagate_trace_context_to: invalid host rule {rule:?} (use host or .suffix)"
            )));
        }
        Ok(match rule.strip_prefix('.') {
            Some(suffix) if !suffix.is_empty() => Self::Suffix(format!(".{suffix}")),
            _ => Self::Exact(rule),
        })
    }

    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(exact) => host == exact,
            Self::Suffix(suffix) => host.ends_with(suffix.as_str()),
        }
    }
}

/// Instrumented HTTP client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AlloyClient {
    client: reqwest::Client,
    propagate: Arc<Vec<HostRule>>,
}

/// A rustls client configuration on the platform trust store, or the reason
/// that store is unavailable. Shared by [`AlloyClient`] and the JWKS client
/// (`jwt.rs`); what an unavailable store means is each caller's policy.
pub(crate) enum PlatformTls {
    /// The platform verifier is in place.
    Verified(rustls::ClientConfig),
    /// The platform trust store could not be loaded. `empty_roots` trusts no
    /// certificate: plain-HTTP destinations still work, and HTTPS fails
    /// verification instead of silently trusting.
    Unavailable {
        error: rustls::Error,
        empty_roots: rustls::ClientConfig,
    },
}

/// Builds the TLS client configuration for Alloy's outbound HTTP clients:
/// the `ring` provider passed explicitly (nothing is installed globally),
/// rustls' safe default protocol versions, and the platform trust store.
/// The error is a protocol-version configuration failure.
pub(crate) fn platform_tls() -> Result<PlatformTls, rustls::Error> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?;
    let verified = builder.clone().with_platform_verifier();
    Ok(platform_or_empty_roots(builder, verified))
}

/// The platform-verified configuration, or, when the platform verifier
/// failed, the same builder with an empty root store.
fn platform_or_empty_roots(
    builder: rustls::ConfigBuilder<rustls::ClientConfig, rustls::WantsVerifier>,
    verified: Result<
        rustls::ConfigBuilder<rustls::ClientConfig, rustls::client::WantsClientCert>,
        rustls::Error,
    >,
) -> PlatformTls {
    match verified {
        Ok(verified) => PlatformTls::Verified(verified.with_no_client_auth()),
        Err(error) => PlatformTls::Unavailable {
            error,
            empty_roots: builder
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        },
    }
}

/// [`AlloyClient`]'s policy for an unavailable platform trust store: its
/// destinations are chosen per request, so construction never fails on it;
/// it warns and uses the empty root store.
fn client_tls(platform: PlatformTls) -> rustls::ClientConfig {
    match platform {
        PlatformTls::Verified(config) => config,
        PlatformTls::Unavailable { error, empty_roots } => {
            // Plain-HTTP destinations still work; HTTPS requests fail
            // certificate verification instead of silently trusting.
            tracing::warn!(
                target: "ferrum_alloy::http_client",
                %error,
                "platform trust store unavailable; HTTPS requests will fail verification"
            );
            empty_roots
        }
    }
}

fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

impl AlloyClient {
    /// Builds a client from `[http_client]`.
    pub fn new(settings: &HttpClientSettings) -> Result<Self, AlloyError> {
        if settings.connect_timeout_ms == 0 {
            return Err(AlloyError::Integration(
                "http_client.connect_timeout_ms must be greater than zero".into(),
            ));
        }
        if settings.request_timeout_ms == 0 {
            return Err(AlloyError::Integration(
                "http_client.request_timeout_ms must be greater than zero".into(),
            ));
        }
        let tls = client_tls(
            platform_tls().map_err(|e| AlloyError::Integration(format!("http_client TLS: {e}")))?,
        );
        let max_redirects = settings.max_redirects;
        let policy = if max_redirects == 0 {
            redirect::Policy::none()
        } else {
            redirect::Policy::custom(move |attempt| {
                let Some(first) = attempt.previous().first().cloned() else {
                    return attempt.stop();
                };
                if attempt.previous().len() > max_redirects {
                    attempt.stop()
                } else if same_origin(&first, attempt.url()) {
                    attempt.follow()
                } else {
                    // Never carry propagated context or credentials to another origin.
                    attempt.stop()
                }
            })
        };
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .connect_timeout(Duration::from_millis(settings.connect_timeout_ms))
            .timeout(Duration::from_millis(settings.request_timeout_ms))
            .redirect(policy)
            .referer(false)
            .build()
            .map_err(|e| AlloyError::Integration(format!("http_client: {e}")))?;
        let propagate = settings
            .propagate_trace_context_to
            .iter()
            .map(|rule| HostRule::parse(rule))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            client,
            propagate: Arc::new(propagate),
        })
    }

    /// The underlying reqwest client, for requests that need no Alloy
    /// instrumentation or propagation.
    pub fn inner(&self) -> &reqwest::Client {
        &self.client
    }

    /// Starts a request. Send it with [`AlloyClient::execute`].
    pub fn request(
        &self,
        method: reqwest::Method,
        url: impl reqwest::IntoUrl,
    ) -> reqwest::RequestBuilder {
        self.client.request(method, url)
    }

    fn propagates_to(&self, url: &url::Url) -> bool {
        url.host_str()
            .map(str::to_ascii_lowercase)
            .is_some_and(|host| self.propagate.iter().any(|rule| rule.matches(&host)))
    }

    /// Sends `request` in a CLIENT span, propagating trace context only to
    /// allowed hosts.
    pub async fn execute(
        &self,
        context: Option<&RequestContext>,
        mut request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let method = method_label(request.method());
        let url = request.url().clone();
        let propagate = context.is_some() && self.propagates_to(&url);
        let span = match context {
            Some(context) => tracing::info_span!(
                target: "ferrum_alloy::http_client",
                parent: &context.span,
                "http.client.request",
                otel.name = method,
                otel.kind = "client",
                otel.status_code = Empty,
                http.request.method = method,
                server.address = url.host_str().unwrap_or_default(),
                server.port = url.port_or_known_default().unwrap_or_default(),
                url.scheme = url.scheme(),
                http.response.status_code = Empty,
                error.type = Empty,
                alloy.operation.name = "http.client",
                alloy.operation.kind = "client",
                alloy.operation.duration_ms = Empty,
                alloy.operation.outcome = Empty,
                alloy.trace.propagated = propagate,
            ),
            None => tracing::info_span!(
                target: "ferrum_alloy::http_client",
                "http.client.request",
                otel.name = method,
                otel.kind = "client",
                otel.status_code = Empty,
                http.request.method = method,
                server.address = url.host_str().unwrap_or_default(),
                server.port = url.port_or_known_default().unwrap_or_default(),
                url.scheme = url.scheme(),
                http.response.status_code = Empty,
                error.type = Empty,
                alloy.operation.name = "http.client",
                alloy.operation.kind = "client",
                alloy.operation.duration_ms = Empty,
                alloy.operation.outcome = Empty,
                alloy.trace.propagated = false,
            ),
        };
        // Never forward caller-supplied trace context or baggage. The outbound
        // context is rebuilt below from the accepted context and the
        // destination policy, so a stale header cannot ride along unseen.
        request.headers_mut().remove(TRACEPARENT);
        request.headers_mut().remove(TRACESTATE);
        request.headers_mut().remove("baggage");
        if propagate && let Some(context) = context {
            let span_id = ferrum_alloy_telemetry::exported_ids(&span)
                .map_or_else(SpanId::random, |(_, span_id, _)| span_id);
            let traceparent = context.child_traceparent(span_id).to_header_value();
            if let Ok(value) = http::HeaderValue::from_str(&traceparent) {
                request.headers_mut().insert(TRACEPARENT, value);
            }
            if let Some(state) = context
                .tracestate
                .as_ref()
                .and_then(|s| s.to_header_value())
            {
                request.headers_mut().insert(TRACESTATE, state);
            }
        }

        let started = Instant::now();
        let result = self.client.execute(request).instrument(span.clone()).await;
        span.record(
            "alloy.operation.duration_ms",
            started.elapsed().as_secs_f64() * 1_000.0,
        );
        match &result {
            Ok(response) => {
                span.record("http.response.status_code", response.status().as_u16());
                span.record("alloy.operation.outcome", "completed");
                if response.status().is_server_error() {
                    span.record("otel.status_code", "error");
                    span.record("error.type", response.status().as_str());
                }
            }
            Err(error) => {
                let kind = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connect"
                } else if error.is_redirect() {
                    "redirect"
                } else {
                    "request"
                };
                span.record("alloy.operation.outcome", "error");
                span.record("otel.status_code", "error");
                span.record("error.type", kind);
            }
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    fn ring_builder() -> rustls::ConfigBuilder<rustls::ClientConfig, rustls::WantsVerifier> {
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
    }

    /// What [`platform_tls`] returns on a host without a platform trust
    /// store.
    pub(crate) fn unavailable() -> PlatformTls {
        platform_or_empty_roots(
            ring_builder(),
            Err(rustls::Error::General("no platform roots".into())),
        )
    }

    /// The configuration uses the `ring` provider passed explicitly,
    /// whichever trust store it has.
    fn assert_shared_core(config: &rustls::ClientConfig) {
        let ring = rustls::crypto::ring::default_provider();
        let suites = |provider: &rustls::crypto::CryptoProvider| {
            provider
                .cipher_suites
                .iter()
                .map(|suite| suite.suite())
                .collect::<Vec<_>>()
        };
        assert_eq!(suites(config.crypto_provider()), suites(&ring));
    }

    /// An in-memory handshake from `client` to a server presenting a
    /// self-signed certificate for `localhost`; returns the client's error.
    fn handshake_error(client: rustls::ClientConfig) -> rustls::Error {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let key = rustls_pki_types::PrivateKeyDer::Pkcs8(
            rustls_pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
        );
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
        let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
        let mut client = rustls::ClientConnection::new(Arc::new(client), name).unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server)).unwrap();
        for _ in 0..8 {
            let mut flight = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut flight).unwrap();
            }
            let mut flight = flight.as_slice();
            while !flight.is_empty() {
                server.read_tls(&mut flight).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut flight = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut flight).unwrap();
            }
            let mut flight = flight.as_slice();
            while !flight.is_empty() {
                client.read_tls(&mut flight).unwrap();
                if let Err(error) = client.process_new_packets() {
                    return error;
                }
            }
        }
        panic!("the client accepted a certificate no root vouches for");
    }

    /// Shared invariant: without a platform trust store the fallback keeps
    /// the shared provider and versions and trusts no certificate, and the
    /// platform's error is handed to the caller.
    #[test]
    fn an_unavailable_platform_trust_store_falls_back_to_trusting_nothing() {
        let PlatformTls::Unavailable { error, empty_roots } = unavailable() else {
            panic!("a failed platform verifier is reported as unavailable");
        };
        assert_eq!(error, rustls::Error::General("no platform roots".into()));
        assert_shared_core(&empty_roots);
        assert!(
            matches!(
                handshake_error(empty_roots),
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
            ),
            "the empty root store refuses the self-signed server"
        );
    }

    #[test]
    fn a_working_platform_verifier_is_used() {
        let builder = ring_builder();
        let platform = platform_or_empty_roots(
            builder.clone(),
            Ok(builder.with_root_certificates(rustls::RootCertStore::empty())),
        );
        assert!(matches!(platform, PlatformTls::Verified(_)));
        // On this host, whichever trust store it has.
        match platform_tls().unwrap() {
            PlatformTls::Verified(config)
            | PlatformTls::Unavailable {
                empty_roots: config,
                ..
            } => {
                assert_shared_core(&config);
            }
        }
    }

    /// Collects the messages of warning events of this thread.
    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<String>>>);

    struct Message<'a>(&'a mut String);

    impl tracing::field::Visit for Message<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut message = String::new();
                event.record(&mut Message(&mut message));
                self.0.lock().unwrap().push(message);
            }
        }
    }

    /// Intentional difference: `AlloyClient` never fails construction on an
    /// unavailable trust store (its destinations are chosen per request); it
    /// warns and uses the empty root store.
    #[test]
    fn the_client_warns_and_continues_without_a_platform_trust_store() {
        use tracing_subscriber::layer::SubscriberExt;

        let warnings = Warnings::default();
        let subscriber = tracing_subscriber::registry().with(warnings.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let config = client_tls(unavailable());
        assert_shared_core(&config);
        let warned = std::mem::take(&mut *warnings.0.lock().unwrap());
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].contains("platform trust store unavailable"),
            "{warned:?}"
        );
    }
}
