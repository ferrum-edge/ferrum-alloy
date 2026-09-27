//! Response lifecycle accounting: header availability versus body
//! completion, and exactly-once finalization on every exit path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use ferrum_alloy_telemetry::metrics::Metrics;
use ferrum_alloy_telemetry::{TelemetryConfig, TelemetryLayer};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::channel::Channel;
use http_body_util::{BodyExt, Empty, Full};
use tower::{Layer, Service, ServiceExt, service_fn};

fn layer() -> (TelemetryLayer, Arc<Metrics>) {
    let layer = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let metrics = layer.metrics();
    (layer, metrics)
}

fn request(method: Method) -> Request<Empty<Bytes>> {
    Request::builder()
        .method(method)
        .uri("/stream")
        .body(Empty::new())
        .unwrap()
}

fn outcome(metrics: &Metrics, label: &str) -> u64 {
    metrics.body_outcomes.get(label)
}

fn total_outcomes(metrics: &Metrics) -> u64 {
    ferrum_alloy_telemetry::metrics::BODY_OUTCOMES
        .iter()
        .map(|label| metrics.body_outcomes.get(label))
        .sum()
}

#[tokio::test]
async fn headers_do_not_finalize_a_streaming_response() {
    let (layer, metrics) = layer();
    let (mut sender, body) = Channel::<Bytes, Infallible>::new(4);
    let mut body = Some(body);
    let service = layer.layer(service_fn(move |_req| {
        let body = body.take().unwrap();
        async move { Ok::<_, Infallible>(Response::new(body)) }
    }));
    let response = service.oneshot(request(Method::GET)).await.unwrap();

    // Headers exist, but the request is still in flight.
    assert_eq!(metrics.in_flight(), 1);
    assert_eq!(metrics.request_count("GET", "__not_routed__", 200), 0);
    assert_eq!(total_outcomes(&metrics), 0);

    let reader = tokio::spawn(async move { response.into_body().collect().await.unwrap() });
    sender
        .send_data(Bytes::from_static(b"event: one\n\n"))
        .await
        .unwrap();
    sender
        .send_data(Bytes::from_static(b"event: two\n\n"))
        .await
        .unwrap();
    assert_eq!(metrics.in_flight(), 1, "still streaming");
    drop(sender);
    let collected = reader.await.unwrap();
    assert_eq!(collected.to_bytes().len(), 24);

    assert_eq!(metrics.in_flight(), 0);
    assert_eq!(outcome(&metrics, "completed"), 1);
    assert_eq!(metrics.request_count("GET", "__not_routed__", 200), 1);
}

#[tokio::test]
async fn trailers_are_part_of_the_body_lifecycle() {
    let (layer, metrics) = layer();
    let (mut sender, body) = Channel::<Bytes, Infallible>::new(4);
    let mut body = Some(body);
    let service = layer.layer(service_fn(move |_req| {
        let body = body.take().unwrap();
        async move { Ok::<_, Infallible>(Response::new(body)) }
    }));
    let response = service.oneshot(request(Method::POST)).await.unwrap();
    let reader = tokio::spawn(async move { response.into_body().collect().await.unwrap() });
    sender
        .send_data(Bytes::from_static(b"payload"))
        .await
        .unwrap();
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", "0".parse().unwrap());
    sender.send_trailers(trailers).await.unwrap();
    drop(sender);
    let collected = reader.await.unwrap();
    assert_eq!(collected.trailers().unwrap()["grpc-status"], "0");
    assert_eq!(outcome(&metrics, "completed"), 1);
    assert_eq!(total_outcomes(&metrics), 1);
    assert_eq!(metrics.in_flight(), 0);
}

#[tokio::test]
async fn empty_bodies_finalize_when_headers_are_produced() {
    let (layer, metrics) = layer();
    let service = layer.layer(service_fn(|_req| async {
        Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new()))
    }));
    let response = service.oneshot(request(Method::GET)).await.unwrap();
    // Hyper never polls an ended body; accounting must not wait for it.
    assert_eq!(outcome(&metrics, "completed"), 1);
    assert_eq!(metrics.in_flight(), 0);
    drop(response);
    assert_eq!(
        total_outcomes(&metrics),
        1,
        "dropping afterwards must not finalize twice"
    );
}

#[tokio::test]
async fn protocol_bodyless_responses_are_not_sent_rather_than_cancelled() {
    let cases = [
        (Method::HEAD, StatusCode::OK),
        (Method::GET, StatusCode::NO_CONTENT),
        (Method::GET, StatusCode::NOT_MODIFIED),
    ];
    for (method, status) in cases {
        let (layer, metrics) = layer();
        let service = layer.layer(service_fn(move |_req| async move {
            let mut response = Response::new(Full::new(Bytes::from_static(b"ignored by hyper")));
            *response.status_mut() = status;
            Ok::<_, Infallible>(response)
        }));
        let response = service.oneshot(request(method.clone())).await.unwrap();
        drop(response);
        assert_eq!(outcome(&metrics, "not_sent"), 1, "{method} {status}");
        assert_eq!(outcome(&metrics, "cancelled"), 0, "{method} {status}");
        assert_eq!(metrics.in_flight(), 0);
    }
}

#[tokio::test]
async fn switching_protocols_is_recorded_as_upgraded() {
    let (layer, metrics) = layer();
    let service = layer.layer(service_fn(|_req| async {
        let mut response = Response::new(Empty::<Bytes>::new());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        Ok::<_, Infallible>(response)
    }));
    drop(service.oneshot(request(Method::GET)).await.unwrap());
    assert_eq!(outcome(&metrics, "upgraded"), 1);
    assert_eq!(metrics.in_flight(), 0);
}

#[tokio::test]
async fn body_errors_finalize_as_error() {
    let (layer, metrics) = layer();
    let (mut sender, body) = Channel::<Bytes, std::io::Error>::new(4);
    let mut body = Some(body);
    let service = layer.layer(service_fn(move |_req| {
        let body = body.take().unwrap();
        async move { Ok::<_, Infallible>(Response::new(body)) }
    }));
    let response = service.oneshot(request(Method::GET)).await.unwrap();
    let reader = tokio::spawn(async move { response.into_body().collect().await });
    sender
        .send_data(Bytes::from_static(b"partial"))
        .await
        .unwrap();
    sender.abort(std::io::Error::other("upstream failed"));
    assert!(reader.await.unwrap().is_err());
    assert_eq!(outcome(&metrics, "error"), 1);
    assert_eq!(total_outcomes(&metrics), 1);
    assert_eq!(metrics.in_flight(), 0);
}

#[tokio::test]
async fn dropping_a_body_mid_stream_is_cancelled_exactly_once() {
    let (layer, metrics) = layer();
    let (mut sender, body) = Channel::<Bytes, Infallible>::new(4);
    let mut body = Some(body);
    let service = layer.layer(service_fn(move |_req| {
        let body = body.take().unwrap();
        async move { Ok::<_, Infallible>(Response::new(body)) }
    }));
    let response = service.oneshot(request(Method::GET)).await.unwrap();
    let mut body = response.into_body();
    sender
        .send_data(Bytes::from_static(b"first"))
        .await
        .unwrap();
    let frame = body.frame().await.unwrap().unwrap();
    assert_eq!(frame.into_data().unwrap(), "first");
    // Client disconnects: the server drops the body.
    drop(body);
    assert_eq!(outcome(&metrics, "cancelled"), 1);
    assert_eq!(total_outcomes(&metrics), 1);
    assert_eq!(metrics.in_flight(), 0);
    drop(sender);
    assert_eq!(total_outcomes(&metrics), 1);
}

#[tokio::test]
async fn dropping_the_future_before_headers_is_recorded() {
    let (layer, metrics) = layer();
    let mut service = layer.layer(service_fn(|_req| async {
        pending::<()>().await;
        Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new()))
    }));
    let mut future = Box::pin(service.ready().await.unwrap().call(request(Method::GET)));
    // Poll once so the inner future starts.
    let waker = std::task::Waker::noop();
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(waker))
            .is_pending()
    );
    assert_eq!(metrics.in_flight(), 1);
    drop(future);
    assert_eq!(outcome(&metrics, "cancelled_before_headers"), 1);
    assert_eq!(metrics.in_flight(), 0);
    // No response existed; the status label is 0.
    assert_eq!(metrics.request_count("GET", "__not_routed__", 0), 1);
}

#[tokio::test]
async fn inner_service_errors_are_finalized() {
    let (layer, metrics) = layer();
    let service = layer.layer(service_fn(|_req: Request<Empty<Bytes>>| async {
        Err::<Response<Empty<Bytes>>, _>(std::io::Error::other("boom"))
    }));
    assert!(service.oneshot(request(Method::GET)).await.is_err());
    assert_eq!(outcome(&metrics, "service_error"), 1);
    assert_eq!(metrics.in_flight(), 0);
}

#[tokio::test]
async fn body_bytes_count_data_handed_to_hyper() {
    let (layer, metrics) = layer();
    let service = layer.layer(service_fn(|_req| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"0123456789"))))
    }));
    let response = service.oneshot(request(Method::GET)).await.unwrap();
    let body = response.into_body();
    assert_eq!(body.size_hint().exact(), Some(10), "size hint is preserved");
    let bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(bytes.len(), 10);
    assert_eq!(outcome(&metrics, "completed"), 1);
}

#[tokio::test]
async fn many_concurrent_requests_are_each_counted_once() {
    let (layer, metrics) = layer();
    let service = layer.layer(service_fn(|_req| async {
        tokio::task::yield_now().await;
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
    }));
    let mut tasks = Vec::new();
    for _ in 0..200 {
        let service = service.clone();
        tasks.push(tokio::spawn(async move {
            let response = service.oneshot(request(Method::GET)).await.unwrap();
            response.into_body().collect().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(metrics.request_count("GET", "__not_routed__", 200), 200);
    assert_eq!(total_outcomes(&metrics), 200);
    assert_eq!(metrics.in_flight(), 0);
}

/// A body whose `is_end_stream` becomes true after the last data frame, like
/// `Full`: Hyper stops polling without seeing `None`.
#[tokio::test]
async fn end_of_stream_after_last_frame_completes_without_a_final_poll() {
    struct TwoFrames(u8);
    impl Body for TwoFrames {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
            self.0 += 1;
            match self.0 {
                1 | 2 => Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(b"x"))))),
                _ => panic!("polled after end of stream"),
            }
        }
        fn is_end_stream(&self) -> bool {
            self.0 >= 2
        }
    }
    let (layer, metrics) = layer();
    let service = layer.layer(service_fn(|_req| async {
        Ok::<_, Infallible>(Response::new(TwoFrames(0)))
    }));
    let response = service.oneshot(request(Method::GET)).await.unwrap();
    let mut body = response.into_body();
    body.frame().await.unwrap().unwrap();
    assert_eq!(total_outcomes(&metrics), 0);
    body.frame().await.unwrap().unwrap();
    assert!(body.is_end_stream());
    assert_eq!(
        outcome(&metrics, "completed"),
        1,
        "finalized at the last frame"
    );
    drop(body);
    assert_eq!(total_outcomes(&metrics), 1);
}
