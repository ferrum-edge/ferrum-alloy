//! JWT/JWKS verification against a real local JWKS endpoint.

#![cfg(feature = "jwt")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
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
    fetches: Arc<AtomicUsize>,
}

async fn jwks_server(keys: &[&SigningKey]) -> Jwks {
    let body = Arc::new(Mutex::new(
        json!({ "keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>() }).to_string(),
    ));
    let fetches = Arc::new(AtomicUsize::new(0));
    let (b, f) = (Arc::clone(&body), Arc::clone(&fetches));
    let app = Router::new().route(
        "/jwks",
        get(move || {
            let (b, f) = (Arc::clone(&b), Arc::clone(&f));
            async move {
                f.fetch_add(1, Ordering::SeqCst);
                b.lock().unwrap().clone()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    Jwks {
        addr,
        body,
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
    let mut none = base;
    none.algorithms = vec!["none".into()];
    assert!(JwtVerifier::new(&none).is_err());
}
