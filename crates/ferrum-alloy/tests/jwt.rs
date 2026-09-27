//! JWT/JWKS verification against a real local JWKS endpoint.

#![cfg(feature = "jwt")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::response::IntoResponse;
use axum::routing::get;
use ferrum_alloy::config::JwtSettings;
use ferrum_alloy::jwt::{JwtVerifier, Principal};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rcgen::KeyPair;
use serde_json::{Value, json};
use tower::ServiceExt;

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]),
            2 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8),
            _ => u32::from(chunk[0]) << 16,
        };
        let symbols = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for symbol in &symbols[..chunk.len() + 1] {
            out.push(ALPHABET[*symbol as usize] as char);
        }
    }
    out
}

struct SigningKey {
    kid: String,
    pair: KeyPair,
}

impl SigningKey {
    fn new(kid: &str) -> Self {
        Self {
            kid: kid.into(),
            pair: KeyPair::generate().unwrap(),
        }
    }

    fn jwk(&self) -> Value {
        let raw = self.pair.public_key_raw();
        assert_eq!(raw[0], 4, "uncompressed P-256 point");
        json!({
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": self.kid,
            "x": b64url(&raw[1..33]), "y": b64url(&raw[33..65]),
        })
    }

    fn sign(&self, claims: &Value, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(str::to_owned);
        encode(
            &header,
            claims,
            &EncodingKey::from_ec_der(&self.pair.serialize_der()),
        )
        .unwrap()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims() -> Value {
    json!({ "iss": "https://issuer.test", "aud": "orders-api", "sub": "user-1", "exp": now() + 300, "scope": "orders:read" })
}

struct Jwks {
    addr: SocketAddr,
    body: Arc<Mutex<String>>,
    /// `Cache-Control` sent with the JWKS, if any.
    cache_control: Arc<Mutex<Option<String>>>,
    /// Response status; set a 5xx to simulate an outage.
    status: Arc<Mutex<StatusCode>>,
    /// How long each response is held back.
    delay: Arc<Mutex<Duration>>,
    fetches: Arc<AtomicUsize>,
}

impl Jwks {
    fn serve(&self, keys: &[&SigningKey]) {
        *self.body.lock().unwrap() = key_set(keys);
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }

    fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// Waits until the JWKS has been requested `count` times, for fetches
    /// that run in the background.
    async fn wait_for_fetches(&self, count: usize) {
        for _ in 0..500 {
            if self.fetches() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("expected {count} JWKS fetches, saw {}", self.fetches());
    }
}

fn key_set(keys: &[&SigningKey]) -> String {
    json!({ "keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>() }).to_string()
}

async fn jwks_server(keys: &[&SigningKey]) -> Jwks {
    let body = Arc::new(Mutex::new(key_set(keys)));
    let cache_control = Arc::new(Mutex::new(None::<String>));
    let status = Arc::new(Mutex::new(StatusCode::OK));
    let delay = Arc::new(Mutex::new(Duration::ZERO));
    let fetches = Arc::new(AtomicUsize::new(0));
    let (b, c, s, d, f) = (
        Arc::clone(&body),
        Arc::clone(&cache_control),
        Arc::clone(&status),
        Arc::clone(&delay),
        Arc::clone(&fetches),
    );
    let app = Router::new().route(
        "/jwks",
        get(move || {
            let (b, c, s, d, f) = (
                Arc::clone(&b),
                Arc::clone(&c),
                Arc::clone(&s),
                Arc::clone(&d),
                Arc::clone(&f),
            );
            async move {
                f.fetch_add(1, Ordering::SeqCst);
                let delay = *d.lock().unwrap();
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let status = *s.lock().unwrap();
                let mut response = (status, b.lock().unwrap().clone()).into_response();
                if let Some(value) = c.lock().unwrap().clone() {
                    response
                        .headers_mut()
                        .insert(http::header::CACHE_CONTROL, value.parse().unwrap());
                }
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    Jwks {
        addr,
        body,
        cache_control,
        status,
        delay,
        fetches,
    }
}

fn settings(jwks: SocketAddr, refresh_ms: u64) -> JwtSettings {
    JwtSettings {
        issuer: "https://issuer.test".into(),
        audiences: vec!["orders-api".into()],
        algorithms: vec!["ES256".into()],
        jwks_url: Some(format!("http://127.0.0.1:{}/jwks", jwks.port())),
        jwks_min_refresh_interval_ms: refresh_ms,
        jwks_max_age_ms: 300_000,
        jwks_max_stale_ms: 300_000,
        jwks_max_bytes: 64 * 1024,
        jwks_timeout_ms: 2_000,
        leeway_seconds: 0,
    }
}

fn protected(verifier: &JwtVerifier) -> Router {
    Router::new()
        .route(
            "/me",
            get(|principal: Principal| async move { principal.subject.unwrap_or_default() }),
        )
        .route_layer(verifier.layer())
}

async fn call(router: &Router, token: Option<&str>) -> (StatusCode, String, http::HeaderMap) {
    let mut request = Request::get("/me");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned(), headers)
}

#[tokio::test]
async fn valid_tokens_are_accepted_and_invalid_ones_rejected() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let verifier = JwtVerifier::new(&settings(jwks.addr, 60_000)).unwrap();
    let router = protected(&verifier);

    let (status, body, _) = call(&router, Some(&key.sign(&claims(), Some("k1")))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "user-1");

    let (status, _, headers) = call(&router, None).await;
    assert_eq!(status, 401);
    assert_eq!(headers["www-authenticate"], "Bearer");

    let mut expired = claims();
    expired["exp"] = json!(now() - 10);
    let mut wrong_aud = claims();
    wrong_aud["aud"] = json!("billing-api");
    let mut wrong_iss = claims();
    wrong_iss["iss"] = json!("https://evil.test");
    let mut no_exp = claims();
    no_exp.as_object_mut().unwrap().remove("exp");
    for (name, claims) in [
        ("expired", expired),
        ("audience", wrong_aud),
        ("issuer", wrong_iss),
        ("no exp", no_exp),
    ] {
        let (status, _, headers) = call(&router, Some(&key.sign(&claims, Some("k1")))).await;
        assert_eq!(status, 401, "{name}");
        assert_eq!(
            headers["www-authenticate"], "Bearer error=\"invalid_token\"",
            "{name}"
        );
    }

    // A token signed by a different key with the same kid.
    let impostor = SigningKey::new("k1");
    let (status, _, _) = call(&router, Some(&impostor.sign(&claims(), Some("k1")))).await;
    assert_eq!(status, 401);
    assert_eq!(jwks.fetches.load(Ordering::SeqCst), 1, "keys are cached");
}

#[tokio::test]
async fn algorithm_confusion_and_unsigned_tokens_are_rejected() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let router = protected(&JwtVerifier::new(&settings(jwks.addr, 60_000)).unwrap());

    let payload = b64url(claims().to_string().as_bytes());
    let none = format!("{}.{payload}.", b64url(br#"{"alg":"none","typ":"JWT"}"#));
    assert_eq!(call(&router, Some(&none)).await.0, 401);

    // HS256 signed with the public key bytes (classic RS/HS confusion shape).
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("k1".into());
    let hs = encode(
        &header,
        &claims(),
        &EncodingKey::from_secret(key.pair.public_key_raw()),
    )
    .unwrap();
    assert_eq!(call(&router, Some(&hs)).await.0, 401);

    assert_eq!(call(&router, Some("not.a.token")).await.0, 401);
    assert_eq!(call(&router, Some(&"a".repeat(20_000))).await.0, 401);
}

#[tokio::test]
async fn unknown_kids_refresh_at_most_once_per_interval() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let router = protected(&JwtVerifier::new(&settings(jwks.addr, 60_000)).unwrap());
    assert_eq!(
        call(&router, Some(&key.sign(&claims(), Some("k1"))))
            .await
            .0,
        200
    );
    for i in 0..10 {
        let (status, _, _) = call(
            &router,
            Some(&key.sign(&claims(), Some(&format!("unknown-{i}")))),
        )
        .await;
        assert_eq!(status, 401);
    }
    assert_eq!(
        jwks.fetches.load(Ordering::SeqCst),
        1,
        "unknown kids cannot force refresh floods"
    );
}

#[tokio::test]
async fn rotated_keys_are_picked_up_after_the_refresh_interval() {
    let old = SigningKey::new("old");
    let jwks = jwks_server(&[&old]).await;
    let router = protected(&JwtVerifier::new(&settings(jwks.addr, 50)).unwrap());
    assert_eq!(
        call(&router, Some(&old.sign(&claims(), Some("old"))))
            .await
            .0,
        200
    );

    let new = SigningKey::new("new");
    *jwks.body.lock().unwrap() = json!({ "keys": [new.jwk()] }).to_string();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let (status, body, _) = call(&router, Some(&new.sign(&claims(), Some("new")))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(jwks.fetches.load(Ordering::SeqCst), 2);
}

/// A verifier with a short key-set lifetime for the expiry tests.
fn short_lived(jwks: SocketAddr, max_age_ms: u64, max_stale_ms: u64) -> JwtVerifier {
    let config = JwtSettings {
        jwks_max_age_ms: max_age_ms,
        jwks_max_stale_ms: max_stale_ms,
        ..settings(jwks, max_age_ms.min(500))
    };
    JwtVerifier::new(&config).unwrap()
}

async fn status_of(verifier: &JwtVerifier, token: &str) -> StatusCode {
    call(&protected(verifier), Some(token)).await.0
}

#[tokio::test]
async fn removed_keys_stop_verifying_after_the_max_age() {
    let old = SigningKey::new("old");
    let jwks = jwks_server(&[&old]).await;
    let verifier = short_lived(jwks.addr, 2_000, 0);
    let token = || old.sign(&claims(), Some("old"));
    assert_eq!(status_of(&verifier, &token()).await, 200);

    // The issuer retires `old`. Within the max age the cached set is used.
    let new = SigningKey::new("new");
    jwks.serve(&[&new]);
    assert_eq!(status_of(&verifier, &token()).await, 200);
    assert_eq!(jwks.fetches(), 1, "fresh keys are cached");

    // After the max age, the known kid alone forces revalidation, and the
    // removed key is no longer trusted.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(status_of(&verifier, &token()).await, 401);
    assert_eq!(jwks.fetches(), 2, "expired keys are revalidated");
    for _ in 0..3 {
        assert_eq!(status_of(&verifier, &token()).await, 401);
    }
    let fresh = new.sign(&claims(), Some("new"));
    assert_eq!(status_of(&verifier, &fresh).await, 200);
    assert_eq!(jwks.fetches(), 2, "the refreshed set is cached again");
}

#[tokio::test]
async fn a_replaced_key_with_the_same_kid_is_picked_up_after_the_max_age() {
    let first = SigningKey::new("k1");
    let jwks = jwks_server(&[&first]).await;
    let verifier = short_lived(jwks.addr, 2_000, 0);
    let first_token = first.sign(&claims(), Some("k1"));
    assert_eq!(status_of(&verifier, &first_token).await, 200);

    let second = SigningKey::new("k1");
    jwks.serve(&[&second]);
    let second_token = second.sign(&claims(), Some("k1"));
    assert_eq!(status_of(&verifier, &second_token).await, 401, "cached");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(status_of(&verifier, &second_token).await, 200);
    assert_eq!(status_of(&verifier, &first_token).await, 401);
    assert_eq!(jwks.fetches(), 2);
}

#[tokio::test]
async fn cache_control_max_age_shortens_but_never_extends_the_lifetime() {
    // (Cache-Control, configured max age ms, refresh interval ms, sleep ms)
    for (cache_control, max_age_ms, refresh_ms, sleep_ms) in [
        // max-age=1 under a 60 s maximum: revalidated after 1 s.
        ("public, max-age=1", 60_000, 20, 1_300),
        // max-age=3600 over a 2 s maximum: the maximum wins.
        ("max-age=3600", 2_000, 20, 2_500),
        // no-store: bounded below by the 250 ms refresh interval.
        ("no-store", 60_000, 250, 600),
    ] {
        let old = SigningKey::new("old");
        let jwks = jwks_server(&[&old]).await;
        *jwks.cache_control.lock().unwrap() = Some(cache_control.into());
        let verifier = JwtVerifier::new(&JwtSettings {
            jwks_max_age_ms: max_age_ms,
            jwks_max_stale_ms: 0,
            ..settings(jwks.addr, refresh_ms)
        })
        .unwrap();
        let token = old.sign(&claims(), Some("old"));
        assert_eq!(status_of(&verifier, &token).await, 200, "{cache_control}");
        jwks.serve(&[&SigningKey::new("new")]);
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        assert_eq!(status_of(&verifier, &token).await, 401, "{cache_control}");
        assert_eq!(jwks.fetches(), 2, "{cache_control}");
    }
}

#[tokio::test]
async fn failed_refreshes_serve_stale_keys_only_within_the_grace_period() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let verifier = short_lived(jwks.addr, 300, 1_500);
    let token = || key.sign(&claims(), Some("k1"));
    assert_eq!(status_of(&verifier, &token()).await, 200);

    // Expired but within the grace period: the stale set keeps verifying
    // while the background refresh fails.
    *jwks.status.lock().unwrap() = StatusCode::INTERNAL_SERVER_ERROR;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(status_of(&verifier, &token()).await, 200);
    jwks.wait_for_fetches(2).await;
    assert_eq!(status_of(&verifier, &token()).await, 200);

    // Past max age + grace: fail closed with 503.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let (status, body, _) = call(&protected(&verifier), Some(&token())).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("auth-unavailable"), "{body}");

    // The issuer recovers: the next request refreshes and succeeds.
    *jwks.status.lock().unwrap() = StatusCode::OK;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(status_of(&verifier, &token()).await, 200);
}

#[tokio::test]
async fn a_zero_grace_period_fails_closed_at_the_max_age() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let verifier = short_lived(jwks.addr, 300, 0);
    let token = key.sign(&claims(), Some("k1"));
    assert_eq!(status_of(&verifier, &token).await, 200);
    *jwks.status.lock().unwrap() = StatusCode::SERVICE_UNAVAILABLE;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(status_of(&verifier, &token).await, 503);
}

#[tokio::test]
async fn stale_keys_answer_known_kids_without_waiting_for_a_slow_refresh() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let verifier = JwtVerifier::new(&JwtSettings {
        jwks_max_age_ms: 300,
        jwks_max_stale_ms: 60_000,
        jwks_timeout_ms: 30_000,
        ..settings(jwks.addr, 20)
    })
    .unwrap();
    let token = key.sign(&claims(), Some("k1"));
    assert_eq!(status_of(&verifier, &token).await, 200);

    // The JWKS becomes very slow and the cached set goes stale.
    jwks.set_delay(Duration::from_secs(20));
    tokio::time::sleep(Duration::from_millis(500)).await;
    for _ in 0..5 {
        let request = status_of(&verifier, &token);
        let answered = tokio::time::timeout(Duration::from_secs(5), request).await;
        assert_eq!(
            answered.expect("a known kid does not wait for the refresh"),
            200
        );
    }
    jwks.wait_for_fetches(2).await;
    assert_eq!(jwks.fetches(), 2, "one background refresh");
}

#[tokio::test]
async fn a_refresh_outlives_the_caller_that_started_it() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    jwks.set_delay(Duration::from_millis(1_000));
    // A long refresh interval: an attempt that was never recorded would lock
    // every caller out for a minute.
    let verifier = JwtVerifier::new(&settings(jwks.addr, 60_000)).unwrap();
    let token = key.sign(&claims(), Some("k1"));

    // The first caller gives up while the fetch is in flight.
    let gave_up = tokio::time::timeout(Duration::from_millis(100), verifier.verify(&token)).await;
    assert!(gave_up.is_err(), "the first caller was cancelled mid-fetch");

    // The fetch still completes and its keys are recorded for later callers.
    jwks.wait_for_fetches(1).await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let (status, body, _) = call(&protected(&verifier), Some(&token)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(jwks.fetches(), 1, "the cancelled caller's refresh was kept");
}

#[tokio::test]
async fn a_slow_successful_refresh_is_used_even_past_its_lifetime() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    // Every fetch takes longer than the 100 ms key-set lifetime.
    jwks.set_delay(Duration::from_millis(700));
    let verifier = short_lived(jwks.addr, 100, 0);
    let token = key.sign(&claims(), Some("k1"));
    let (status, body, _) = call(&protected(&verifier), Some(&token)).await;
    assert_eq!(status, 200, "first fetch: {body}");
    let (status, body, _) = call(&protected(&verifier), Some(&token)).await;
    assert_eq!(status, 200, "revalidation: {body}");
    assert_eq!(jwks.fetches(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_on_an_expired_set_refresh_once() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let verifier = short_lived(jwks.addr, 400, 0);
    let token = key.sign(&claims(), Some("k1"));
    verifier.verify(&token).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let (verifier, token) = (verifier.clone(), token.clone());
        tasks.spawn(async move { verifier.verify(&token).await.is_ok() });
    }
    while let Some(accepted) = tasks.join_next().await {
        assert!(accepted.unwrap());
    }
    assert_eq!(jwks.fetches(), 2, "one refresh for all callers");
}

#[tokio::test]
async fn kid_less_tokens_need_an_unambiguous_key_set() {
    let a = SigningKey::new("a");
    let single = jwks_server(&[&a]).await;
    let router = protected(&JwtVerifier::new(&settings(single.addr, 60_000)).unwrap());
    assert_eq!(call(&router, Some(&a.sign(&claims(), None))).await.0, 200);

    let b = SigningKey::new("b");
    let double = jwks_server(&[&a, &b]).await;
    let router = protected(&JwtVerifier::new(&settings(double.addr, 60_000)).unwrap());
    let (status, _, _) = call(&router, Some(&a.sign(&claims(), Some("a")))).await;
    assert_eq!(status, 200, "warm the cache");
    assert_eq!(call(&router, Some(&a.sign(&claims(), None))).await.0, 401);
}

#[tokio::test]
async fn expired_ambiguous_keys_are_refreshed_before_rejecting_a_kidless_token() {
    let a = SigningKey::new("a");
    let b = SigningKey::new("b");
    let jwks = jwks_server(&[&a, &b]).await;
    let verifier = short_lived(jwks.addr, 200, 0);
    let token = a.sign(&claims(), None);
    assert_eq!(
        status_of(&verifier, &a.sign(&claims(), Some("a"))).await,
        200
    );

    // The expired two-key set is ambiguous, but the issuer has since
    // removed one key, leaving a set that can verify tokens without `kid`.
    jwks.serve(&[&a]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(status_of(&verifier, &token).await, 200);
    assert_eq!(jwks.fetches(), 2);
}

#[tokio::test]
async fn jwks_with_only_unusable_keys_returns_503() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[]).await;
    let mut unusable = key.jwk();
    unusable["use"] = json!("enc");
    *jwks.body.lock().unwrap() = json!({ "keys": [unusable] }).to_string();
    let router = protected(&JwtVerifier::new(&settings(jwks.addr, 60_000)).unwrap());

    let (status, body, _) = call(&router, Some(&key.sign(&claims(), Some("k1")))).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("auth-unavailable"), "{body}");
}

#[tokio::test]
async fn jwks_outages_are_503_not_401() {
    let key = SigningKey::new("k1");
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let router = protected(&JwtVerifier::new(&settings(closed, 60_000)).unwrap());
    let (status, body, _) = call(&router, Some(&key.sign(&claims(), Some("k1")))).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("auth-unavailable"));

    let big = jwks_server(&[&key]).await;
    *big.body.lock().unwrap() = format!("{{\"keys\":[],\"pad\":\"{}\"}}", "x".repeat(100_000));
    let router = protected(&JwtVerifier::new(&settings(big.addr, 60_000)).unwrap());
    assert_eq!(
        call(&router, Some(&key.sign(&claims(), Some("k1"))))
            .await
            .0,
        503,
        "oversized JWKS rejected"
    );
}

#[tokio::test]
async fn the_authorizer_decides_after_authentication() {
    let key = SigningKey::new("k1");
    let jwks = jwks_server(&[&key]).await;
    let verifier = JwtVerifier::new(&settings(jwks.addr, 60_000)).unwrap();
    let router = Router::new()
        .route(
            "/me",
            get(|principal: Principal| async move { principal.subject.unwrap_or_default() }),
        )
        .route_layer(verifier.layer().with_authorizer(
            |principal: &Principal, _: &http::request::Parts| {
                if principal.has_scope("orders:write") {
                    Ok(())
                } else {
                    Err("orders:write scope required".to_owned())
                }
            },
        ));
    let (status, body, _) = call(&router, Some(&key.sign(&claims(), Some("k1")))).await;
    assert_eq!(status, 403);
    assert!(body.contains("orders:write scope required"));
    let mut write = claims();
    write["scope"] = json!("orders:read orders:write");
    assert_eq!(
        call(&router, Some(&key.sign(&write, Some("k1")))).await.0,
        200
    );
}

#[test]
fn unsafe_jwt_settings_are_rejected() {
    let base = settings("127.0.0.1:1".parse().unwrap(), 1_000);
    let mut http_remote = base.clone();
    http_remote.jwks_url = Some("http://idp.example.com/jwks".into());
    assert!(JwtVerifier::new(&http_remote).is_err());
    let mut hmac = base.clone();
    hmac.algorithms = vec!["HS256".into()];
    assert!(JwtVerifier::new(&hmac).is_err());
    let mut none = base.clone();
    none.algorithms = vec!["none".into()];
    assert!(JwtVerifier::new(&none).is_err());
    let mut no_max_age = base.clone();
    no_max_age.jwks_max_age_ms = 0;
    assert!(JwtVerifier::new(&no_max_age).is_err());
    let day_ms = 24 * 60 * 60 * 1000;
    let mut long_max_age = base.clone();
    long_max_age.jwks_max_age_ms = day_ms + 1;
    assert!(JwtVerifier::new(&long_max_age).is_err());
    let mut long_max_stale = base.clone();
    long_max_stale.jwks_max_stale_ms = day_ms + 1;
    assert!(JwtVerifier::new(&long_max_stale).is_err());
    let mut day = base.clone();
    day.jwks_max_age_ms = day_ms;
    day.jwks_max_stale_ms = day_ms;
    assert!(JwtVerifier::new(&day).is_ok(), "24 hours is allowed");
    let mut inverted = base;
    inverted.jwks_max_age_ms = 500;
    assert!(
        JwtVerifier::new(&inverted).is_err(),
        "the max age cannot be shorter than the refresh interval"
    );
}
