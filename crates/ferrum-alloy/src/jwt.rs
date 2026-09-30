//! JWT bearer verification with JWKS (feature `jwt`), using the maintained
//! `jsonwebtoken` crate.
//!
//! Policy is explicit and fails closed:
//!
//! * only the configured asymmetric algorithms (never `none` or HMAC);
//! * `iss`, `aud`, and `exp` are required; `nbf` is checked when present;
//! * keys come only from the configured JWKS URL — never from `jku`, `x5u`,
//!   or embedded `jwk` headers in the token;
//! * a JWK is used only to verify signatures: one whose `use` is not `sig`,
//!   or whose `key_ops` lack `verify`, is ignored;
//! * a JWK verifies only algorithms its key type and curve support and, when
//!   it declares `alg`, only that algorithm (RFC 8725, section 3.1);
//! * a token is verified against exactly one key: the one whose `kid` and
//!   algorithm match the token's. A token without `kid` is accepted only when
//!   exactly one key could verify it; several matching keys are ambiguous and
//!   the token is rejected;
//! * JWKS refreshes are single-flight, rate-limited (also for unknown
//!   `kid`s), time-bounded, size-bounded, and never follow redirects. Each
//!   runs in its own task, so a caller that stops waiting never cancels it;
//! * a fetched key set is trusted for a bounded lifetime
//!   (`jwks_max_age_ms`, shortened by the response's `Cache-Control:
//!   max-age`) and then revalidated even for known `kid`s, so a key removed
//!   from the JWKS stops verifying;
//! * an expired key set keeps verifying known `kid`s for at most
//!   `jwks_max_stale_ms` while it is revalidated in the background or while
//!   refreshes fail, then verification fails closed;
//! * a JWKS outage is `503 auth-unavailable`, not `401`, also when a failed
//!   refresh leaves a cached key set that holds no key for the token.
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
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde_json::{Map, Value};
use tokio::sync::watch;
use tower_layer::Layer;
use tower_service::Service;

use crate::config::{JwtSettings, MAX_JWKS_LIFETIME_MS};
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
    /// The algorithms this key may verify: those its key type supports,
    /// narrowed to its `alg` when the JWK declares one. Never empty.
    algorithms: Vec<Algorithm>,
    key: DecodingKey,
}

impl Key {
    /// The key a JWK contributes, or `None` when it may never verify a token
    /// signature: its `use` is not `sig`, its `key_ops` lack `verify`, its key
    /// type supports no accepted algorithm, or it declares an `alg` that its
    /// key type cannot use.
    fn from_jwk(jwk: &Jwk) -> Option<Self> {
        let common = &jwk.common;
        let signature_use = matches!(common.public_key_use, None | Some(PublicKeyUse::Signature));
        let verify_operation = match &common.key_operations {
            None => true,
            Some(operations) => operations.contains(&KeyOperations::Verify),
        };
        if !signature_use || !verify_operation {
            return None;
        }
        let algorithms: Vec<Algorithm> = key_type_algorithms(&jwk.algorithm)
            .iter()
            .copied()
            .filter(|&algorithm| declared_algorithm_permits(common.key_algorithm, algorithm))
            .collect();
        if algorithms.is_empty() {
            return None;
        }
        let key = DecodingKey::from_jwk(jwk).ok()?;
        Some(Self {
            kid: common.key_id.clone(),
            algorithms,
            key,
        })
    }

    /// Whether this key may verify a token with `kid` and `algorithm`. Any
    /// key's `kid` matches a token without one.
    fn matches(&self, kid: Option<&str>, algorithm: Algorithm) -> bool {
        let kid_matches = kid.is_none() || self.kid.as_deref() == kid;
        kid_matches && self.algorithms.contains(&algorithm)
    }
}

/// The accepted algorithms a JWK's key type and curve support.
fn key_type_algorithms(parameters: &AlgorithmParameters) -> &'static [Algorithm] {
    match parameters {
        AlgorithmParameters::RSA(_) => &[
            Algorithm::RS256,
            Algorithm::RS384,
            Algorithm::RS512,
            Algorithm::PS256,
            Algorithm::PS384,
            Algorithm::PS512,
        ],
        AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
            EllipticCurve::P256 => &[Algorithm::ES256],
            EllipticCurve::P384 => &[Algorithm::ES384],
            _ => &[],
        },
        AlgorithmParameters::OctetKeyPair(okp) if okp.curve == EllipticCurve::Ed25519 => {
            &[Algorithm::EdDSA]
        }
        // Symmetric (`oct`) and unknown key types never verify.
        _ => &[],
    }
}

/// Whether a JWK's declared `alg`, if any, permits `algorithm`. An `alg`
/// this crate does not recognize permits nothing.
fn declared_algorithm_permits(declared: Option<KeyAlgorithm>, algorithm: Algorithm) -> bool {
    match declared {
        None => true,
        Some(declared) => declared == KeyAlgorithm::from(algorithm),
    }
}

/// Keys to verify a token with.
struct Keys {
    keys: Arc<Vec<Key>>,
    /// Whether the key source vouches for these keys now: they come from a
    /// successful refresh, from a fresh cache, or from a cache whose latest
    /// refresh succeeded within `jwks_min_refresh_interval_ms`. `false` for
    /// cached keys used because a refresh failed or could not run: a token
    /// they cannot verify may be signed by a key the JWKS now holds.
    current: bool,
}

impl Keys {
    /// Keys the key source vouches for now.
    fn current(keys: Arc<Vec<Key>>) -> Self {
        Self {
            keys,
            current: true,
        }
    }
}

struct KeyState {
    keys: Arc<Vec<Key>>,
    /// When the current keys were requested; `None` before the first
    /// successful fetch.
    fetched_at: Option<Instant>,
    /// How long the current keys are fresh.
    lifetime: Duration,
    /// When the last refresh started.
    last_attempt: Option<Instant>,
    /// Refreshes started so far.
    attempts: u64,
    /// The latest refresh: `None` in its channel while it runs, then its
    /// outcome.
    latest: Option<watch::Receiver<Outcome>>,
}

/// The outcome of one refresh, published to every caller waiting on it:
/// the fetched keys, or why there are none. `None` until it completes.
type Outcome = Option<Result<Arc<Vec<Key>>, AuthError>>;

/// What [`JwtVerifier::refresh`] did.
enum Refresh {
    /// Nothing: the latest refresh started within
    /// `jwks_min_refresh_interval_ms`. `failed` is whether it failed.
    RateLimited { failed: bool },
    /// Nothing: the state lock is poisoned, or there is no runtime to fetch
    /// on.
    Unavailable,
    /// A refresh is running or started after the caller's snapshot; its
    /// outcome arrives on the receiver.
    Started(watch::Receiver<Outcome>),
}

/// A snapshot of the cached key set.
struct Cached {
    keys: Arc<Vec<Key>>,
    freshness: Freshness,
    /// [`KeyState::attempts`] when the snapshot was taken.
    attempts: u64,
}

/// How much the cached key set may be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    /// Within its lifetime.
    Fresh,
    /// Expired, but within `jwks_max_stale_ms`: usable only while a refresh
    /// is in flight, fails, or is rate limited.
    Stale,
    /// Never fetched, or past the stale bound: never used.
    Expired,
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
    max_age: Duration,
    max_stale: Duration,
    state: RwLock<KeyState>,
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
        if settings.jwks_max_age_ms == 0 {
            return Err(AlloyError::Integration(
                "auth.jwt.jwks_max_age_ms must be greater than zero".into(),
            ));
        }
        if settings.jwks_max_age_ms > MAX_JWKS_LIFETIME_MS
            || settings.jwks_max_stale_ms > MAX_JWKS_LIFETIME_MS
        {
            return Err(AlloyError::Integration(
                "auth.jwt.jwks_max_age_ms and auth.jwt.jwks_max_stale_ms must not exceed 24 hours"
                    .into(),
            ));
        }
        if settings.jwks_min_refresh_interval_ms > settings.jwks_max_age_ms {
            return Err(AlloyError::Integration(
                "auth.jwt.jwks_min_refresh_interval_ms must not exceed auth.jwt.jwks_max_age_ms"
                    .into(),
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
                max_age: Duration::from_millis(settings.jwks_max_age_ms),
                max_stale: Duration::from_millis(settings.jwks_max_stale_ms),
                state: RwLock::new(KeyState {
                    keys: Arc::new(Vec::new()),
                    fetched_at: None,
                    lifetime: Duration::ZERO,
                    last_attempt: None,
                    attempts: 0,
                    latest: None,
                }),
            }),
        })
    }

    /// The cached keys and how far they may be trusted. A poisoned lock
    /// yields no keys.
    fn cached(&self) -> Cached {
        let Ok(state) = self.inner.state.read() else {
            return Cached {
                keys: Arc::default(),
                freshness: Freshness::Expired,
                attempts: 0,
            };
        };
        let stale_until = state.lifetime.saturating_add(self.inner.max_stale);
        let freshness = match state.fetched_at.map(|at| at.elapsed()) {
            Some(age) if age < state.lifetime => Freshness::Fresh,
            Some(age) if age < stale_until => Freshness::Stale,
            _ => Freshness::Expired,
        };
        Cached {
            keys: Arc::clone(&state.keys),
            freshness,
            attempts: state.attempts,
        }
    }

    /// The freshness lifetime of a JWKS response: `Cache-Control: max-age`
    /// minus `Age` when present, else `jwks_max_age_ms`, bounded below by
    /// `jwks_min_refresh_interval_ms` and above by `jwks_max_age_ms`.
    /// `no-cache` and `no-store` count as `max-age=0`.
    fn lifetime(&self, headers: &http::HeaderMap) -> Duration {
        let mut max_age: Option<u64> = None;
        for value in headers.get_all(http::header::CACHE_CONTROL) {
            let Ok(value) = value.to_str() else {
                continue;
            };
            for directive in value.split(',').map(str::trim) {
                let seconds = if directive.eq_ignore_ascii_case("no-cache")
                    || directive.eq_ignore_ascii_case("no-store")
                {
                    Some(0)
                } else {
                    match directive.split_once('=') {
                        Some((name, seconds)) if name.trim().eq_ignore_ascii_case("max-age") => {
                            seconds.trim().trim_matches('"').parse::<u64>().ok()
                        }
                        _ => None,
                    }
                };
                // Several directives: the most conservative wins.
                if let Some(seconds) = seconds {
                    max_age = Some(max_age.map_or(seconds, |current| current.min(seconds)));
                }
            }
        }
        let age = headers
            .get(http::header::AGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(Duration::ZERO, Duration::from_secs);
        let lifetime = max_age.map_or(self.inner.max_age, Duration::from_secs);
        lifetime
            .saturating_sub(age)
            .min(self.inner.max_age)
            .max(self.inner.min_refresh)
    }

    async fn fetch(&self) -> Result<(Vec<Key>, Duration), AuthError> {
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
        let lifetime = self.lifetime(response.headers());
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
        let keys: Vec<Key> = set.keys.iter().filter_map(Key::from_jwk).collect();
        Ok((keys, lifetime))
    }

    /// Starts a refresh, or joins the one in flight. Refreshes are single
    /// flight and start at most once per `jwks_min_refresh_interval_ms`.
    /// `seen` is [`Cached::attempts`] from the caller's snapshot: a caller
    /// whose snapshot predates the latest refresh uses that refresh's
    /// outcome instead of fetching again.
    ///
    /// The fetch runs in its own task, so a caller that stops waiting (a
    /// disconnected client, a deadline) never cancels it. If the task ends
    /// without publishing an outcome, a later request clears its rate-limit
    /// timestamp and can start a replacement refresh.
    fn refresh(&self, seen: u64) -> Refresh {
        self.clear_abandoned();
        // Fast path under the read lock: join the latest refresh, or honour
        // the rate limit.
        match self.inner.state.read() {
            Ok(state) => {
                if let Some(pending) = self.pending(&state, seen) {
                    return pending;
                }
            }
            // A poisoned lock never refreshes; `cached` already fails closed.
            Err(_) => return Refresh::Unavailable,
        }
        let Ok(mut state) = self.inner.state.write() else {
            return Refresh::Unavailable;
        };
        if let Some(pending) = self.pending(&state, seen) {
            return pending;
        }
        // Outside a Tokio runtime there is nothing to run the fetch on.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Refresh::Unavailable;
        };
        // Recorded before the fetch, so the rate limit counts from here. Age
        // is measured from here too, so fetch latency counts against the
        // lifetime.
        let started = Instant::now();
        let (sender, receiver) = watch::channel(None);
        state.last_attempt = Some(started);
        state.attempts = state.attempts.wrapping_add(1);
        state.latest = Some(receiver.clone());
        drop(state);
        let verifier = self.clone();
        runtime.spawn(async move {
            let fetched = verifier.fetch().await;
            verifier.record(started, fetched, &sender);
        });
        Refresh::Started(receiver)
    }

    /// The refresh a caller must use instead of starting one: the latest
    /// one while it runs or when it started after the caller's snapshot, or
    /// none while the rate limit holds.
    fn pending(&self, state: &KeyState, seen: u64) -> Option<Refresh> {
        if let Some(latest) = &state.latest {
            // A task that ended without an outcome (a panic, or a runtime
            // shutting down) closed its channel: it is not running.
            let running = latest.borrow().is_none() && latest.has_changed().is_ok();
            let completed_after_snapshot = latest.borrow().is_some() && state.attempts != seen;
            if running || completed_after_snapshot {
                return Some(Refresh::Started(latest.clone()));
            }
        }
        let recent = state.last_attempt.map(|at| at.elapsed());
        if recent.is_some_and(|age| age < self.inner.min_refresh) {
            let failed = state.latest.as_ref().is_some_and(refresh_failed);
            return Some(Refresh::RateLimited { failed });
        }
        None
    }

    /// Removes the rate limit left by a refresh task that ended without
    /// publishing an outcome. Normal completions keep their timestamp.
    fn clear_abandoned(&self) {
        let Ok(mut state) = self.inner.state.write() else {
            return;
        };
        let abandoned = state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.borrow().is_none() && latest.has_changed().is_err());
        if abandoned {
            state.last_attempt = None;
            state.latest = None;
        }
    }

    /// Records a finished refresh and publishes its outcome to the callers
    /// waiting on it. A failed refresh keeps the previous keys and their
    /// fetch time, so they age out on schedule.
    fn record(
        &self,
        started: Instant,
        fetched: Result<(Vec<Key>, Duration), AuthError>,
        sender: &watch::Sender<Outcome>,
    ) {
        let outcome = match fetched {
            Ok((keys, lifetime)) => match self.inner.state.write() {
                Ok(mut state) => {
                    let keys = Arc::new(keys);
                    state.keys = Arc::clone(&keys);
                    state.fetched_at = Some(started);
                    state.lifetime = lifetime;
                    Ok(keys)
                }
                // A poisoned lock fails closed, as in `cached`.
                Err(_) => Err(AuthError::KeysUnavailable),
            },
            Err(error) => {
                tracing::warn!(target: "ferrum_alloy::jwt", "JWKS refresh failed");
                Err(error)
            }
        };
        sender.send_replace(Some(outcome));
    }

    /// Waits for a refresh (single flight, rate limited) and returns the
    /// keys to verify with. A successful refresh's keys are used as fetched,
    /// even when the fetch took longer than their lifetime. Otherwise the
    /// cached keys are used while fresh or stale, and never once expired;
    /// they are current only when the rate limit holds against a refresh
    /// that succeeded.
    async fn refreshed(&self, seen: u64) -> Result<Keys, AuthError> {
        let (current, failure) = match self.refresh(seen) {
            Refresh::RateLimited { failed } => (!failed, AuthError::KeysUnavailable),
            Refresh::Unavailable => (false, AuthError::KeysUnavailable),
            Refresh::Started(mut receiver) => {
                let outcome = receiver
                    .wait_for(|outcome| outcome.is_some())
                    .await
                    .ok()
                    .and_then(|outcome| outcome.clone());
                if outcome.is_none() {
                    self.clear_abandoned();
                }
                match outcome {
                    Some(Ok(keys)) if !keys.is_empty() => return Ok(Keys::current(keys)),
                    Some(Ok(_)) => return Err(AuthError::KeysUnavailable),
                    Some(Err(error)) => (false, error),
                    None => (false, AuthError::KeysUnavailable),
                }
            }
        };
        let cached = self.cached();
        if cached.freshness == Freshness::Expired || cached.keys.is_empty() {
            return Err(failure);
        }
        Ok(Keys {
            keys: cached.keys,
            current,
        })
    }

    /// The one key that may verify a token with `kid` and `algorithm`, if
    /// any. Several such keys are ambiguous, and none of them is tried.
    fn select<'k>(
        keys: &'k [Key],
        kid: Option<&str>,
        algorithm: Algorithm,
    ) -> Result<Option<&'k DecodingKey>, AuthError> {
        let mut candidates = keys.iter().filter(|k| k.matches(kid, algorithm));
        match (candidates.next(), candidates.next()) {
            (None, _) => Ok(None),
            (Some(key), None) => Ok(Some(&key.key)),
            (Some(_), Some(_)) => Err(AuthError::Invalid("ambiguous signing key")),
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
        let alg = header.alg;
        let cached = self.cached();
        let keys = match cached.freshness {
            // Expiry takes precedence over selection errors: the refreshed
            // set may no longer be ambiguous or may contain usable keys.
            Freshness::Expired => self.refreshed(cached.attempts).await?,
            Freshness::Fresh => {
                if Self::select(&cached.keys, kid, alg)?.is_some() {
                    Keys::current(cached.keys)
                } else {
                    self.refreshed(cached.attempts).await?
                }
            }
            // Past its lifetime but within the stale bound: verify now and
            // revalidate in the background, so no request waits on the JWKS.
            // A stale set that cannot answer (no key, or several) waits for
            // the refresh instead.
            Freshness::Stale => {
                if matches!(Self::select(&cached.keys, kid, alg), Ok(Some(_))) {
                    self.refresh(cached.attempts);
                    Keys::current(cached.keys)
                } else {
                    self.refreshed(cached.attempts).await?
                }
            }
        };
        let key = match Self::select(&keys.keys, kid, alg) {
            Ok(Some(key)) => key,
            // No cached key can verify the token, and the key source could
            // not say whether it holds one now: an outage, not a bad token.
            _ if !keys.current => return Err(AuthError::KeysUnavailable),
            Ok(None) => return Err(AuthError::Invalid("unknown signing key")),
            Err(error) => return Err(error),
        };
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

/// Whether a refresh completed and failed.
fn refresh_failed(latest: &watch::Receiver<Outcome>) -> bool {
    matches!(*latest.borrow(), Some(Err(_)))
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
