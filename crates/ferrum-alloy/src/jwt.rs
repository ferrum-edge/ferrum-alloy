//! JWT bearer verification with JWKS (feature `jwt`), using the maintained
//! `jsonwebtoken` crate.
//!
//! Policy is explicit and fails closed:
//!
//! * only the configured asymmetric algorithms (never `none` or HMAC);
//! * `iss`, `aud`, and `exp` are required; `nbf` is checked when present;
//! * keys come only from the configured JWKS URL — never from `jku`, `x5u`,
//!   or embedded `jwk` headers in the token;
//! * a token without `kid` is accepted only when exactly one key could
//!   verify it;
//! * JWKS refreshes are single-flight, rate-limited (also for unknown
//!   `kid`s), time-bounded, size-bounded, and never follow redirects;
//! * a JWKS outage is `503 auth-unavailable`, not `401`.
//!
//! Authentication proves who the caller is; [`Authorize`] is where the
//! application decides what they may do. Forwarded identity headers never
//! bypass token verification.

use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use http::Request;
use http::header::{AUTHORIZATION, HeaderValue, WWW_AUTHENTICATE};
use http::request::Parts;
use jsonwebtoken::jwk::{JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde_json::{Map, Value};
use tower_layer::Layer;
use tower_service::Service;

use crate::config::JwtSettings;
use crate::error::AlloyError;
use crate::problem::{Problem, ProblemKind};

/// A verified caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    /// `sub`.
    pub subject: Option<String>,
    /// `iss`.
    pub issuer: String,
    /// `aud` values.
    pub audiences: Vec<String>,
    /// Scopes from `scope` (space-separated) or `scp` (array).
    pub scopes: Vec<String>,
    /// All claims.
    pub claims: Map<String, Value>,
}

impl Principal {
    /// Returns `true` when the token grants `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| unauthorized(None))
    }
}

/// Why a token was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No bearer token.
    #[error("missing bearer token")]
    Missing,
    /// The token is malformed, expired, or fails a policy check.
    #[error("invalid token: {0}")]
    Invalid(&'static str),
    /// Keys could not be fetched.
    #[error("signing keys unavailable")]
    KeysUnavailable,
}

/// Application authorization policy, evaluated after authentication.
pub trait Authorize: Send + Sync + 'static {
    /// `Err(detail)` denies with `403`. Keep `detail` free of secrets.
    fn authorize(&self, principal: &Principal, request: &Parts) -> Result<(), String>;
}

impl<F> Authorize for F
where
    F: Fn(&Principal, &Parts) -> Result<(), String> + Send + Sync + 'static,
{
    fn authorize(&self, principal: &Principal, request: &Parts) -> Result<(), String> {
        self(principal, request)
    }
}

struct Key {
    kid: Option<String>,
    key: DecodingKey,
}

struct KeyState {
    keys: Arc<Vec<Key>>,
    last_attempt: Option<Instant>,
}

struct Inner {
    issuer: String,
    audiences: Vec<String>,
    algorithms: Vec<Algorithm>,
    leeway: u64,
    jwks_url: url::Url,
    client: reqwest::Client,
    max_bytes: usize,
    min_refresh: Duration,
    state: RwLock<KeyState>,
    refresh: tokio::sync::Mutex<()>,
}

/// Verifies bearer tokens against a JWKS.
#[derive(Clone)]
pub struct JwtVerifier {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("issuer", &self.inner.issuer)
            .field("audiences", &self.inner.audiences)
            .field("algorithms", &self.inner.algorithms)
            .finish_non_exhaustive()
    }
}

fn parse_algorithm(name: &str) -> Result<Algorithm, AlloyError> {
    let algorithm = match name {
        "RS256" => Algorithm::RS256,
        "RS384" => Algorithm::RS384,
        "RS512" => Algorithm::RS512,
        "PS256" => Algorithm::PS256,
        "PS384" => Algorithm::PS384,
        "PS512" => Algorithm::PS512,
        "ES256" => Algorithm::ES256,
        "ES384" => Algorithm::ES384,
        "EdDSA" => Algorithm::EdDSA,
        other => {
            return Err(AlloyError::Integration(format!(
                "auth.jwt.algorithms: {other:?} is not an accepted asymmetric algorithm"
            )));
        }
    };
    Ok(algorithm)
}

impl JwtVerifier {
    /// Builds a verifier from `[auth.jwt]`. Keys are fetched on first use.
    pub fn new(settings: &JwtSettings) -> Result<Self, AlloyError> {
        let url_text = settings
            .jwks_url
            .as_deref()
            .ok_or_else(|| AlloyError::Integration("auth.jwt.jwks_url is required".into()))?;
        let jwks_url = url::Url::parse(url_text)
            .map_err(|e| AlloyError::Integration(format!("auth.jwt.jwks_url: {e}")))?;
        let loopback = jwks_url
            .host_str()
            .is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "[::1]");
        if jwks_url.scheme() != "https" && !(jwks_url.scheme() == "http" && loopback) {
            return Err(AlloyError::Integration(
                "auth.jwt.jwks_url must use https (http is allowed only for loopback)".into(),
            ));
        }
        if settings.issuer.is_empty() || settings.audiences.is_empty() {
            return Err(AlloyError::Integration(
                "auth.jwt requires issuer and audiences".into(),
            ));
        }
        let algorithms = settings
            .algorithms
            .iter()
            .map(|a| parse_algorithm(a))
            .collect::<Result<Vec<_>, _>>()?;
        if algorithms.is_empty() {
            return Err(AlloyError::Integration(
                "auth.jwt.algorithms must not be empty".into(),
            ));
        }
        let tls = {
            use rustls_platform_verifier::BuilderVerifierExt;
            let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| AlloyError::Integration(format!("JWKS TLS: {e}")))?;
            match builder.clone().with_platform_verifier() {
                Ok(builder) => builder.with_no_client_auth(),
                // An https JWKS needs the platform trust store: fail at startup.
                Err(error) if jwks_url.scheme() == "https" => {
                    return Err(AlloyError::Integration(format!(
                        "JWKS TLS: platform trust store unavailable: {error}"
                    )));
                }
                Err(_) => builder
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth(),
            }
        };
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .timeout(Duration::from_millis(settings.jwks_timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AlloyError::Integration(format!("JWKS client: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                issuer: settings.issuer.clone(),
                audiences: settings.audiences.clone(),
                algorithms,
                leeway: settings.leeway_seconds,
                jwks_url,
                client,
                max_bytes: settings.jwks_max_bytes,
                min_refresh: Duration::from_millis(settings.jwks_min_refresh_interval_ms),
                state: RwLock::new(KeyState {
                    keys: Arc::new(Vec::new()),
                    last_attempt: None,
                }),
                refresh: tokio::sync::Mutex::new(()),
            }),
        })
    }

    fn keys(&self) -> Arc<Vec<Key>> {
        self.inner
            .state
            .read()
            .map(|s| Arc::clone(&s.keys))
            .unwrap_or_default()
    }

    async fn fetch(&self) -> Result<Vec<Key>, AuthError> {
        let mut response = self
            .inner
            .client
            .get(self.inner.jwks_url.clone())
            .send()
            .await
            .map_err(|_| AuthError::KeysUnavailable)?;
        if !response.status().is_success() {
            return Err(AuthError::KeysUnavailable);
        }
        if response
            .content_length()
            .is_some_and(|len| len > self.inner.max_bytes as u64)
        {
            return Err(AuthError::KeysUnavailable);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AuthError::KeysUnavailable)?
        {
            if body.len() + chunk.len() > self.inner.max_bytes {
                return Err(AuthError::KeysUnavailable);
            }
            body.extend_from_slice(&chunk);
        }
        let set: JwkSet = serde_json::from_slice(&body).map_err(|_| AuthError::KeysUnavailable)?;
        let keys: Vec<Key> = set
            .keys
            .iter()
            .filter(|jwk| !matches!(jwk.common.public_key_use, Some(ref u) if *u != PublicKeyUse::Signature))
            .filter_map(|jwk| {
                DecodingKey::from_jwk(jwk).ok().map(|key| Key {
                    kid: jwk.common.key_id.clone(),
                    key,
                })
            })
            .collect();
        Ok(keys)
    }

    /// Refreshes keys at most once per `jwks_min_refresh_interval_ms`.
    /// Returns `false` when a refresh was not attempted (rate limited).
    async fn refresh(&self) -> Result<bool, AuthError> {
        let _guard = self.inner.refresh.lock().await;
        let recently = self
            .inner
            .state
            .read()
            .ok()
            .and_then(|s| s.last_attempt)
            .is_some_and(|at| at.elapsed() < self.inner.min_refresh);
        if recently {
            return Ok(false);
        }
        if let Ok(mut state) = self.inner.state.write() {
            state.last_attempt = Some(Instant::now());
        }
        let keys = self.fetch().await?;
        if let Ok(mut state) = self.inner.state.write() {
            state.keys = Arc::new(keys);
        }
        Ok(true)
    }

    fn select<'k>(
        keys: &'k [Key],
        kid: Option<&str>,
    ) -> Result<Option<&'k DecodingKey>, AuthError> {
        match kid {
            Some(kid) => Ok(keys
                .iter()
                .find(|k| k.kid.as_deref() == Some(kid))
                .map(|k| &k.key)),
            None => match keys {
                [only] => Ok(Some(&only.key)),
                [] => Ok(None),
                _ => Err(AuthError::Invalid(
                    "token has no kid and the key set is ambiguous",
                )),
            },
        }
    }

    /// Verifies a compact JWS and returns the principal.
    pub async fn verify(&self, token: &str) -> Result<Principal, AuthError> {
        if token.len() > 16 * 1024 {
            return Err(AuthError::Invalid("token too large"));
        }
        let header = decode_header(token).map_err(|_| AuthError::Invalid("malformed token"))?;
        if !self.inner.algorithms.contains(&header.alg) {
            return Err(AuthError::Invalid("algorithm not allowed"));
        }
        let kid = header.kid.as_deref();
        let mut keys = self.keys();
        if Self::select(&keys, kid)?.is_none() {
            // Unknown kid or no keys yet: refresh (rate limited) once.
            let attempted = self.refresh().await;
            keys = self.keys();
            if keys.is_empty() {
                return Err(match attempted {
                    Err(error) => error,
                    Ok(_) => AuthError::KeysUnavailable,
                });
            }
        }
        let key = Self::select(&keys, kid)?.ok_or(AuthError::Invalid("unknown signing key"))?;
        let mut validation = Validation::new(header.alg);
        validation.algorithms = vec![header.alg];
        validation.set_issuer(&[self.inner.issuer.as_str()]);
        validation.set_audience(&self.inner.audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.leeway = self.inner.leeway;
        let data = decode::<Map<String, Value>>(token, key, &validation)
            .map_err(|_| AuthError::Invalid("signature or claims rejected"))?;
        let claims = data.claims;
        let audiences = match claims.get("aud") {
            Some(Value::String(aud)) => vec![aud.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let scopes = match (claims.get("scope"), claims.get("scp")) {
            (Some(Value::String(scope)), _) => {
                scope.split_whitespace().map(str::to_owned).collect()
            }
            (_, Some(Value::Array(items))) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        Ok(Principal {
            subject: claims.get("sub").and_then(Value::as_str).map(str::to_owned),
            issuer: self.inner.issuer.clone(),
            audiences,
            scopes,
            claims,
        })
    }

    /// A layer requiring a valid token on the wrapped routes.
    pub fn layer(&self) -> JwtLayer {
        JwtLayer {
            verifier: self.clone(),
            authorizer: None,
        }
    }
}

fn unauthorized(error: Option<&AuthError>) -> Problem {
    let challenge = match error {
        None | Some(AuthError::Missing) => HeaderValue::from_static("Bearer"),
        Some(_) => HeaderValue::from_static("Bearer error=\"invalid_token\""),
    };
    Problem::new(ProblemKind::Unauthorized)
        .with_detail("A valid bearer token is required.")
        .with_header(WWW_AUTHENTICATE, challenge)
}

fn rejection(error: &AuthError) -> Response {
    match error {
        AuthError::KeysUnavailable => Problem::new(ProblemKind::AuthUnavailable)
            .with_detail("Signing keys could not be retrieved; try again later.")
            .into_response(),
        other => unauthorized(Some(other)).into_response(),
    }
}

/// Layer produced by [`JwtVerifier::layer`].
#[derive(Clone)]
pub struct JwtLayer {
    verifier: JwtVerifier,
    authorizer: Option<Arc<dyn Authorize>>,
}

impl std::fmt::Debug for JwtLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtLayer").finish_non_exhaustive()
    }
}

impl JwtLayer {
    /// Adds an authorization policy evaluated after authentication.
    #[must_use]
    pub fn with_authorizer(mut self, authorizer: impl Authorize) -> Self {
        self.authorizer = Some(Arc::new(authorizer));
        self
    }
}

impl<S> Layer<S> for JwtLayer {
    type Service = JwtService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        JwtService {
            inner,
            verifier: self.verifier.clone(),
            authorizer: self.authorizer.clone(),
        }
    }
}

/// Service produced by [`JwtLayer`].
#[derive(Clone)]
pub struct JwtService<S> {
    inner: S,
    verifier: JwtVerifier,
    authorizer: Option<Arc<dyn Authorize>>,
}

impl<S> std::fmt::Debug for JwtService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtService").finish_non_exhaustive()
    }
}

impl<S> Service<Request<Body>> for JwtService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let verifier = self.verifier.clone();
        let authorizer = self.authorizer.clone();
        Box::pin(async move {
            let token = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, token)| token.trim().to_owned());
            let Some(token) = token else {
                return Ok(rejection(&AuthError::Missing));
            };
            let principal = match verifier.verify(&token).await {
                Ok(principal) => principal,
                Err(error) => {
                    tracing::debug!(target: "ferrum_alloy::jwt", %error, "bearer token rejected");
                    return Ok(rejection(&error));
                }
            };
            let (mut parts, body) = request.into_parts();
            if let Some(authorizer) = &authorizer
                && let Err(detail) = authorizer.authorize(&principal, &parts)
            {
                return Ok(Problem::new(ProblemKind::Forbidden)
                    .with_detail(detail)
                    .into_response());
            }
            parts.extensions.insert(principal);
            inner.call(Request::from_parts(parts, body)).await
        })
    }
}

/// Extension so callers can write `settings.verifier()`.
impl JwtSettings {
    /// Builds a [`JwtVerifier`].
    pub fn verifier(&self) -> Result<JwtVerifier, AlloyError> {
        JwtVerifier::new(self)
    }
}
