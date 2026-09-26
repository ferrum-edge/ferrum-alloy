//! Instrumented outbound HTTP client (feature `http-client`).
//!
//! * Explicit connect and whole-request timeouts.
//! * `traceparent` is sent only to hosts listed in
//!   `http_client.propagate_trace_context_to`. Credentials, cookies,
//!   `baggage`, and gateway identity are never added automatically.
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
use ferrum_alloy_telemetry::trace_context::{SpanId, TRACEPARENT};
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

fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

impl AlloyClient {
    /// Builds a client from `[http_client]`.
    pub fn new(settings: &HttpClientSettings) -> Result<Self, AlloyError> {
        use rustls_platform_verifier::BuilderVerifierExt;
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| AlloyError::Integration(format!("http_client TLS: {e}")))?;
        let tls = match builder.clone().with_platform_verifier() {
            Ok(builder) => builder.with_no_client_auth(),
            Err(error) => {
                // Plain-HTTP destinations still work; HTTPS requests fail
                // certificate verification instead of silently trusting.
                tracing::warn!(
                    target: "ferrum_alloy::http_client",
                    %error,
                    "platform trust store unavailable; HTTPS requests will fail verification"
                );
                builder
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth()
            }
        };
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
                request.headers_mut().insert("tracestate", state);
            }
        } else {
            // Never forward caller-supplied context to hosts outside the list.
            request.headers_mut().remove(TRACEPARENT);
            request.headers_mut().remove("tracestate");
        }
        request.headers_mut().remove("baggage");

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
