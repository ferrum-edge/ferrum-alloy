//! Shared helpers: run an Alloy app on ephemeral ports and talk to it over
//! real TCP connections.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::{AlloyApp, AlloyError, Lifecycle, ServerStats, TelemetryInit};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[cfg(feature = "tls")]
pub mod pki;

pub const TOKEN: &str = "test-management-token-0123456789abcdef";

/// A config suitable for tests: management enabled with a token.
pub fn config() -> AlloyConfig {
    let mut config = AlloyConfig::default();
    config.management.token = Some(ferrum_alloy::config::Secret::new(TOKEN));
    config.shutdown.drain_timeout_ms = 2_000;
    config
}

pub struct TestServer {
    pub addr: SocketAddr,
    pub management: SocketAddr,
    pub lifecycle: Lifecycle,
    /// Connection counters of the application listener.
    pub stats: Arc<ServerStats>,
    pub task: JoinHandle<Result<(), AlloyError>>,
}

impl TestServer {
    pub async fn shutdown(self) -> Result<(), AlloyError> {
        self.lifecycle.trigger_shutdown();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("server stopped in time")
            .expect("server task")
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn management_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.management)
    }
}

/// Starts `app` with `config` on ephemeral loopback ports.
pub async fn start(app: AlloyApp, config: AlloyConfig) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_on(app, config, listener).await
}

/// Starts `app` with `config`, serving the application on `listener` and
/// management on an ephemeral loopback port.
pub async fn start_on(app: AlloyApp, mut config: AlloyConfig, listener: TcpListener) -> TestServer {
    let management = TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.server.bind = listener.local_addr().unwrap();
    config.management.bind = management.local_addr().unwrap();
    let addr = listener.local_addr().unwrap();
    let management_addr = management.local_addr().unwrap();
    let parts = app
        .config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .shutdown_signal(std::future::pending())
        .into_parts()
        .unwrap();
    let lifecycle = parts.lifecycle.clone();
    let stats = parts.app_stats();
    let task = tokio::spawn(parts.serve_on(listener, Some(management)));
    TestServer {
        addr,
        management: management_addr,
        lifecycle,
        stats,
        task,
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "body is not JSON ({e}): {:?}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub async fn send(request: Request<Full<Bytes>>) -> Reply {
    let client = Client::builder(TokioExecutor::new()).build_http();
    let response = client.request(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body,
    }
}

pub async fn fetch(url: &str) -> Reply {
    send(Request::get(url).body(Full::default()).unwrap()).await
}

pub async fn fetch_with(url: &str, headers: &[(&str, &str)]) -> Reply {
    let mut builder = Request::get(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    send(builder.body(Full::default()).unwrap()).await
}

/// Sends raw bytes and returns whatever the server writes back before
/// closing or the timeout.
pub async fn raw(addr: SocketAddr, request: &[u8], wait: Duration) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(wait, stream.read_to_end(&mut out)).await;
    out
}
