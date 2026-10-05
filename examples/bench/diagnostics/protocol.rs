//! Included only by the reviewed experiment overlay, never the ordinary graph.

use std::fmt::Write;

use h2::alloy_diagnostics::{Event, QueueState};

// One first-CANCEL witness, not a per-stream registry or event history. Cursors
// share the endpoint sequence; no timestamp marks are added to the reservation.
#[derive(Clone, Debug, Default)]
pub(super) struct ProtocolState {
    pub(super) observed: bool,
    headers: u64,
    last_headers: Option<(u32, u64)>,
    cancels: u64,
    first_cancel: Option<(u32, u64)>,
    applied: u64,
    first_application: Option<(u64, QueueState, QueueState)>,
    pending: [u64; 6],
    last_pending: Option<(u8, u64)>,
}

impl ProtocolState {
    pub(super) fn headers(&self) -> u64 {
        self.headers
    }

    pub(super) fn event(&mut self, event: Event, cursor: u64) {
        self.observed = true;
        match event {
            Event::Headers(stream) => {
                self.headers = self.headers.saturating_add(1);
                self.last_headers = Some((stream, cursor));
            }
            Event::Cancel(stream) => {
                self.cancels = self.cancels.saturating_add(1);
                let _ = self.first_cancel.get_or_insert((stream, cursor));
            }
            Event::Applied {
                stream,
                before,
                after,
            } => {
                self.applied = self.applied.saturating_add(1);
                if self.first_cancel.is_some_and(|(id, _)| id == stream) {
                    let _ = self.first_application.get_or_insert((cursor, before, after));
                }
            }
            Event::Pending(stage) => {
                let index = stage as usize;
                self.pending[index] = self.pending[index].saturating_add(1);
                self.last_pending = Some((stage as u8, cursor));
            }
        }
    }

    pub(super) fn write(&self, text: &mut impl Write) {
        if !self.observed {
            let _ = writeln!(text, "protocol absent");
            return;
        }
        let _ = write!(text, "pd {:x} ", self.headers);
        write_pair(text, self.last_headers);
        let _ = write!(text, " {:x} ", self.cancels);
        write_pair(text, self.first_cancel);
        let _ = write!(text, " {:x} ", self.applied);
        if let Some((cursor, before, after)) = self.first_application {
            let _ = write!(text, "{cursor:x} ");
            write_queue(text, before);
            let _ = write!(text, " ");
            write_queue(text, after);
        } else {
            let _ = write!(text, "- - -");
        }
        let _ = write!(text, "\npp");
        for count in self.pending {
            let _ = write!(text, " {count:x}");
        }
        let _ = write!(text, " ");
        write_pair(
            text,
            self.last_pending.map(|(stage, cursor)| (u32::from(stage), cursor)),
        );
        let _ = writeln!(text);
    }

    pub(super) fn saturated() -> Self {
        let queue = QueueState {
            empty: true,
            buffered: usize::MAX,
            requested: u32::MAX,
            stream_capacity: u32::MAX,
            connection_capacity: u32::MAX,
            staged: 3,
        };
        Self {
            observed: true,
            headers: u64::MAX,
            last_headers: Some((u32::MAX, u64::MAX)),
            cancels: u64::MAX,
            first_cancel: Some((u32::MAX, u64::MAX)),
            applied: u64::MAX,
            first_application: Some((u64::MAX, queue, queue)),
            pending: [u64::MAX; 6],
            last_pending: Some((5, u64::MAX)),
        }
    }
}

fn write_pair(text: &mut impl Write, pair: Option<(u32, u64)>) {
    if let Some((id, cursor)) = pair {
        let _ = write!(text, "{id:x}:{cursor:x}");
    } else {
        let _ = write!(text, "-");
    }
}

fn write_queue(text: &mut impl Write, queue: QueueState) {
    let _ = write!(
        text,
        "{}/{:x}/{:x}/{:x}/{:x}/{:x}",
        u8::from(queue.empty),
        queue.buffered,
        queue.requested,
        queue.stream_capacity,
        queue.connection_capacity,
        queue.staged,
    );
}

pub(super) fn schema(text: &mut impl Write) {
    let _ = writeln!(
        text,
        "protocol fork=h2-0.4.19/alloy-numeric-v1 qualification=false cursors=endpoint-seq \
         pd=headers,last-id:cursor,cancels,first-id:cursor,applied,first-apply-cursor,before,after \
         queue=empty/buffered/requested/stream-cap/conn-cap/staged \
         staged=0(none)/1(this)/2(other)/3(invalidated) \
         pp=goaway/control/decode/send-ready/send-flush/shutdown,last-stage:cursor \
         counts=hex first-only '-'=unobserved waiter/runtime-readiness=unobserved"
    );
}

pub(super) fn provenance(text: &mut impl Write) {
    let _ = writeln!(
        text,
        "pf archive=ef8e5e5a340588f4452631496976cf8636d4a7ecf600239fdc27615d2530bc16 \
         h2={} alloy={} graph={} head={}",
        env!("ALLOY_BENCH_H2_PATCH_SHA256"),
        env!("ALLOY_BENCH_OBSERVER_PATCH_SHA256"),
        env!("ALLOY_BENCH_GRAPH_SHA256"),
        env!("DIAGNOSTIC_HEAD"),
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use bytes::Bytes;
    use ferrum_alloy::bench_diagnostics::{IoObserver, ProtocolFuture};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Notify, oneshot};
    use tokio::task::JoinSet;

    use super::*;
    use crate::health::{Direction, Observer, WireIo, WireObservation};

    const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";

    #[derive(Default)]
    struct WriteGate {
        held: AtomicBool,
        target: Mutex<Option<Waker>>,
        blocked: Notify,
    }

    impl WriteGate {
        fn release(&self) {
            self.held.store(false, Ordering::SeqCst);
            let target = self.target.lock().unwrap().take();
            if let Some(target) = target {
                target.wake();
            }
        }
    }

    struct GatedSocket {
        inner: TcpStream,
        gate: Arc<WriteGate>,
    }

    impl AsyncRead for GatedSocket {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for GatedSocket {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if this.gate.held.load(Ordering::SeqCst) {
                *this.gate.target.lock().unwrap() = Some(cx.waker().clone());
                // Close the registration/release race before returning Pending.
                if this.gate.held.load(Ordering::SeqCst) {
                    this.gate.blocked.notify_one();
                    return Poll::Pending;
                }
            }
            Pin::new(&mut this.inner).poll_write(cx, bytes)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    fn observer(wire: &Arc<WireObservation>) -> Option<Arc<dyn IoObserver>> {
        Some(Arc::clone(wire) as Arc<dyn IoObserver>)
    }

    async fn until(wire: &WireObservation, ready: impl Fn(&ProtocolState) -> bool) {
        loop {
            let changed = wire.protocol_changed.notified();
            if ready(&wire.snapshot().protocol) {
                return;
            }
            changed.await;
        }
    }

    async fn retained_socket(staged: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let socket = TcpStream::connect(address).await.unwrap();
        let local = socket.local_addr().unwrap();
        let (server_socket, remote) = listener.accept().await.unwrap();
        assert_eq!(local, remote);
        let observations = Observer::default();
        let unrelated = Observer::default();
        let client = observations
            .wire_endpoint(INSTANCE, 0, 1, Some(local), Some(address), false)
            .unwrap();
        let server = observations
            .server_wire(INSTANCE, Some(address), remote, false)
            .unwrap();
        let gate = Arc::new(WriteGate::default());
        let server_io = WireIo::new(
            GatedSocket {
                inner: server_socket,
                gate: Arc::clone(&gate),
            },
            Arc::clone(&server),
        );
        let mut tasks = JoinSet::new();
        let (body_tx, body_rx) = oneshot::channel();
        let server_observer = observer(&server);
        tasks.spawn(async move {
            let mut connection = ProtocolFuture::new(
                h2::server::handshake(server_io),
                server_observer,
            )
            .await
            .unwrap();
            let mut body_tx = Some(body_tx);
            while let Some(request) = connection.accept().await {
                let (request, mut response) = request.unwrap();
                assert_eq!(request.method(), http::Method::GET);
                assert!(request.body().is_end_stream());
                let first = body_tx.is_some();
                let mut body = response
                    .send_response(http::Response::new(()), !first)
                    .unwrap();
                if first {
                    body.send_data(
                        Bytes::from_static(b"private-first-data"),
                        false,
                    )
                    .unwrap();
                    body_tx.take().unwrap().send(body).unwrap();
                }
            }
        });
        let (mut sender, connection) = ProtocolFuture::new(
            h2::client::handshake(WireIo::new(socket, Arc::clone(&client))),
            observer(&client),
        )
        .await
        .unwrap();
        tasks.spawn(async move {
            let _ = connection.await;
        });
        let (response, mut reset) = sender
            .send_request(http::Request::new(()), true)
            .unwrap();
        let response = response.await.unwrap();
        let id = response.body().stream_id().as_u32();
        let mut received = response.into_body();
        let first = received.data().await.unwrap().unwrap();
        received.flow_control().release_capacity(first.len()).unwrap();
        assert_eq!(first, "private-first-data");
        assert!(server.snapshot().directions[Direction::Tx.index()].data_complete > 0);
        let mut body = body_rx.await.unwrap();
        if staged {
            gate.held.store(true, Ordering::SeqCst);
            body.send_data(
                Bytes::from(vec![b'x'; 32 * 1024]),
                false,
            )
            .unwrap();
            gate.blocked.notified().await;
        }
        reset.send_reset(h2::Reason::CANCEL);
        until(&server, |state| state.first_application.is_some()).await;
        let applied = server.snapshot();
        let protocol = &applied.protocol;
        assert_eq!(protocol.first_cancel.unwrap().0, id);
        let (cursor, before, after) = protocol.first_application.unwrap();
        assert!(cursor > protocol.first_cancel.unwrap().1);
        let prefix = applied
            .streams
            .iter()
            .flatten()
            .find(|stream| stream.id == id)
            .unwrap();
        assert!(prefix.first_cancel.unwrap().sequence < protocol.first_cancel.unwrap().1);
        assert!(after.empty);
        assert_eq!(after.buffered, 0);
        assert_eq!(after.requested, 0);
        assert_eq!(after.stream_capacity, 0);
        if staged {
            assert!(before.buffered > 0);
            assert_eq!(before.staged, 1);
            assert_eq!(after.staged, 3);
            assert!(after.connection_capacity >= before.connection_capacity);
            assert!(protocol.pending[3] + protocol.pending[4] > 0);
        }
        let frozen = observations.capture();
        let sampled_at = std::time::Instant::now();
        let mut frozen_text = String::new();
        frozen.write_wire_core(&mut frozen_text, sampled_at);
        gate.release();
        drop(body);
        drop(received);
        drop(reset);
        for _ in 0..2 {
            std::future::poll_fn(|cx| sender.poll_ready(cx)).await.unwrap();
            let (response, stream) = sender
                .send_request(http::Request::new(()), true)
                .unwrap();
            assert!(response.await.unwrap().body().is_end_stream());
            drop(stream);
        }
        until(&client, |state| state.headers == 3).await;
        assert_eq!(server.snapshot().protocol.headers, 3);
        if staged {
            let drained = server.snapshot();
            let stream = drained
                .streams
                .iter()
                .flatten()
                .find(|stream| stream.id == id)
                .unwrap();
            assert!(stream.directions[Direction::Tx.index()].data.bytes > first.len() as u64);
            assert!(stream.late.complete > 0);
            // This follows actual codec invalidation: these accepted bytes are
            // not evidence of newly produced DATA after reset application.
        }
        assert_eq!(client.remote, server.socket);
        assert_eq!(client.socket, server.remote);
        assert_eq!(observations.wires().len(), 1);
        assert_eq!(observations.server_wires().len(), 1);
        assert!(unrelated.wires().is_empty());
        assert!(unrelated.server_wires().is_empty());
        let mut again = String::new();
        frozen.clone().write_wire_core(&mut again, sampled_at);
        assert_eq!(frozen_text, again);
        assert!(!again.contains("private-first-data"));
        assert_eq!(protocol.cancels, 1);
        assert_eq!(protocol.applied, 1);
        drop(sender);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protocol_cancel_clears_queue_and_staged_data_then_reuses_original_socket() {
        // CONTROLLED observer qualification; this is not the cause of #142.
        tokio::time::timeout(Duration::from_secs(10), async {
            // Concurrent independent instances expose mixed thread-local attribution.
            tokio::join!(retained_socket(false), retained_socket(true));
        })
        .await
        .unwrap();
    }

    #[derive(Debug, Default)]
    struct Ambient(std::sync::atomic::AtomicU64);

    impl h2::alloy_diagnostics::Observer for Ambient {
        fn event(&self, _event: Event) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct AmbientPoll<F> {
        inner: Pin<Box<F>>,
        observer: Arc<Ambient>,
    }

    impl<F> AmbientPoll<F> {
        fn new(inner: F, observer: &Arc<Ambient>) -> Self {
            Self {
                inner: Box::pin(inner),
                observer: Arc::clone(observer),
            }
        }
    }

    impl<F: std::future::Future> std::future::Future for AmbientPoll<F> {
        type Output = F::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            h2::alloy_diagnostics::scope(
                Some(Arc::clone(&this.observer) as Arc<dyn h2::alloy_diagnostics::Observer>),
                || this.inner.as_mut().poll(cx),
            )
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unobserved_original_connection_masks_ambient_observer() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = TcpStream::connect(address).await.unwrap();
            let original = client.local_addr().unwrap();
            let (server, remote) = listener.accept().await.unwrap();
            assert_eq!(remote, original);
            let ambient = Arc::new(Ambient::default());
            let server_ambient = Arc::clone(&ambient);
            let mut tasks = JoinSet::new();
            tasks.spawn(AmbientPoll::new(
                async move {
                    let mut connection = ProtocolFuture::new(h2::server::handshake(server), None)
                        .await
                        .unwrap();
                    while let Some(request) = connection.accept().await {
                        let (_, mut response) = request.unwrap();
                        response.send_response(http::Response::new(()), true).unwrap();
                    }
                },
                &server_ambient,
            ));
            let (mut sender, connection) = AmbientPoll::new(
                ProtocolFuture::new(h2::client::handshake(client), None),
                &ambient,
            )
            .await
            .unwrap();
            tasks.spawn(AmbientPoll::new(
                async move {
                    let _ = connection.await;
                },
                &ambient,
            ));
            for _ in 0..2 {
                std::future::poll_fn(|cx| sender.poll_ready(cx)).await.unwrap();
                let (response, stream) = sender
                    .send_request(http::Request::new(()), true)
                    .unwrap();
                assert!(response.await.unwrap().body().is_end_stream());
                drop(stream);
            }
            assert_eq!(ambient.0.load(Ordering::SeqCst), 0);
            drop(sender);
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        })
        .await
        .unwrap();
    }

    #[test]
    fn protocol_counter_width_and_value_footprints_are_frozen() {
        assert!(std::mem::size_of::<ProtocolState>() <= 256);
        let mut text = String::new();
        ProtocolState::saturated().write(&mut text);
        assert_eq!(text.len(), 343);
        let mut schema_text = String::new();
        schema(&mut schema_text);
        assert!(schema_text.len() <= 512);
        let mut provenance_text = String::new();
        provenance(&mut provenance_text);
        assert_eq!(provenance_text.len(), 332);
        assert!(!text.contains("age"));
    }
}
