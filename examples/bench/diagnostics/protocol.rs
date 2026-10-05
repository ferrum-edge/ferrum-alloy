//! Included only by the reviewed experiment overlay, never the ordinary graph.

use std::fmt::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use h2::alloy_diagnostics::{Event, QueueState};

const RUNTIME: u64 = 1 << 21;

// README-v2 defines every index and flag. Cursor zero means absent. No history
// or timestamp slots; the live and frozen records each occupy exactly 256 bytes.
#[derive(Clone, Debug, Default)]
#[repr(C)]
pub(crate) struct ProtocolState {
    pub(crate) w: [u64; 23],
    pub(crate) n: [u32; 16],
    pub(crate) f: u64,
}

#[repr(C)]
pub(super) struct LiveProtocol {
    w: [AtomicU64; 23],
    n: [AtomicU32; 16],
    pub(super) flags: AtomicU64,
}

impl Default for LiveProtocol {
    fn default() -> Self {
        Self {
            w: std::array::from_fn(|_| AtomicU64::new(0)),
            n: std::array::from_fn(|_| AtomicU32::new(0)),
            flags: AtomicU64::new(0),
        }
    }
}

impl LiveProtocol {
    // Caller holds the existing endpoint mutex, including for frozen copies.
    pub(super) fn snapshot(&self) -> ProtocolState {
        ProtocolState {
            w: std::array::from_fn(|index| self.w[index].load(Ordering::Relaxed)),
            n: std::array::from_fn(|index| self.n[index].load(Ordering::Relaxed)),
            f: self.flags.load(Ordering::Acquire),
        }
    }

    pub(super) fn replace(&self, state: &ProtocolState) {
        for (target, value) in self.w.iter().zip(state.w) {
            target.store(value, Ordering::Relaxed);
        }
        for (target, value) in self.n.iter().zip(state.n) {
            target.store(value, Ordering::Relaxed);
        }
        // Runtime entry can race with an event copy; never erase its sticky bit.
        let _ = self
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |flags| {
                Some(state.f | (flags & RUNTIME))
            });
    }

    pub(super) fn event(&self, event: Event, cursor: u64) {
        let mut state = self.snapshot();
        state.event(event, cursor);
        self.replace(&state);
    }

    pub(super) fn runtime_entry(&self) {
        self.flags.fetch_or(RUNTIME, Ordering::AcqRel);
    }
}

impl ProtocolState {
    pub(crate) fn headers(&self) -> u64 {
        self.w[0]
    }

    pub(crate) fn first_cancel(&self) -> Option<(u32, u64)> {
        (self.w[3] != 0).then_some((self.n[1], self.w[3]))
    }

    pub(crate) fn first_application(&self) -> Option<(u64, QueueState, QueueState)> {
        (self.w[5] != 0).then(|| {
            let before = self.queue(19, 4, 1);
            let after = self.queue(20, 7, 4);
            (self.w[5], before, after)
        })
    }

    fn queue(&self, w: usize, n: usize, bit: u32) -> QueueState {
        QueueState {
            empty: self.f & (1 << bit) != 0,
            buffered: self.w[w] as usize,
            requested: self.n[n],
            stream_capacity: self.n[n + 1],
            connection_capacity: self.n[n + 2],
            staged: ((self.f >> (bit + 1)) & 3) as u8,
        }
    }

    fn set_queue(&mut self, queue: QueueState, w: usize, n: usize, bit: u32) {
        self.w[w] = queue.buffered as u64;
        self.n[n..n + 3].copy_from_slice(&[
            queue.requested,
            queue.stream_capacity,
            queue.connection_capacity,
        ]);
        self.f &= !(7 << bit);
        self.f |= (u64::from(queue.empty) | (u64::from(queue.staged) << 1)) << bit;
    }

    fn event(&mut self, event: Event, cursor: u64) {
        self.f |= 1;
        match event {
            Event::Headers(stream) => {
                self.w[0] = self.w[0].saturating_add(1);
                self.w[1] = cursor;
                self.n[0] = stream;
            }
            Event::Cancel(stream) => {
                self.w[2] = self.w[2].saturating_add(1);
                if self.w[3] == 0 {
                    self.w[3] = cursor;
                    self.n[1] = stream;
                }
            }
            Event::Data { stream, bytes } => {
                self.w[13] = self.w[13].saturating_add(1);
                self.w[14] = self.w[14].saturating_add(bytes);
                self.w[15] = cursor;
                self.n[2] = stream;
            }
            Event::Applied {
                stream,
                before,
                after,
            } => {
                self.w[4] = self.w[4].saturating_add(1);
                if self.w[3] != 0 && self.n[1] == stream && self.w[5] == 0 {
                    self.w[5] = cursor;
                    self.set_queue(before, 19, 4, 1);
                    self.set_queue(after, 20, 7, 4);
                }
                if self.f & (1 << 16) == 0 {
                    self.w[16] = cursor;
                    self.n[3] = stream;
                    self.set_queue(before, 21, 10, 7);
                    self.set_queue(after, 22, 13, 10);
                    if !after.empty
                        || after.buffered != 0
                        || after.requested != 0
                        || after.stream_capacity != 0
                        || before.staged == 1 && after.staged != 3
                    {
                        self.f |= 1 << 16;
                    }
                }
            }
            Event::Pending(stage) => {
                let index = stage as usize;
                self.w[6 + index] = self.w[6 + index].saturating_add(1);
                self.w[12] = cursor;
                self.f = (self.f & !(7 << 13)) | ((index as u64) << 13);
            }
            Event::Assignment {
                origin,
                runtime_before,
                runtime_after,
            } => {
                if self.w[17] == 0 {
                    self.w[17] = cursor;
                    self.f |= (origin as u64) << 17;
                    self.f |= u64::from(runtime_before) << 22;
                    self.f |= u64::from(runtime_after) << 28;
                }
            }
            Event::DropEntry(prior) => {
                if self.w[18] == 0 {
                    self.w[18] = cursor;
                    if let Some(present) = prior {
                        self.f |= 1 << 20;
                        self.f |= u64::from(present) << 23;
                    }
                }
            }
            Event::Terminal {
                class,
                initiator,
                code,
            } => {
                if self.f & (3 << 24) == 0 {
                    self.f |= u64::from(class) << 24;
                    self.f |= u64::from(initiator) << 26;
                    self.f |= u64::from(code) << 32;
                }
            }
        }
    }

    pub(super) fn write(&self, text: &mut impl Write) {
        let _ = text.write_str("p2 ");
        for value in self.w {
            write_number(text, value, 13);
        }
        for value in self.n {
            write_number(text, u64::from(value), 7);
        }
        write_number(text, self.f, 13);
        let _ = text.write_char('\n');
    }

    pub(super) fn saturated() -> Self {
        Self {
            w: [u64::MAX; 23],
            n: [u32::MAX; 16],
            f: u64::MAX,
        }
    }
}

fn write_number(text: &mut impl Write, mut value: u64, width: usize) {
    let mut digits = [b'0'; 13];
    for digit in digits[..width].iter_mut().rev() {
        let remainder = (value % 36) as u8;
        *digit = if remainder < 10 {
            b'0' + remainder
        } else {
            b'a' + remainder - 10
        };
        value /= 36;
    }
    // Only the literal lowercase radix alphabet enters the scratch buffer.
    if let Ok(number) = std::str::from_utf8(&digits[..width]) {
        let _ = text.write_str(number);
    }
}

pub(super) fn schema(text: &mut impl Write) {
    let _ = writeln!(
        text,
        "p2 fork=h2-0.4.19 qualification=false radix=36 widths=23x13,16x7,1x13 \
         order=README-v2 cursor0=absent flags=README-v2 waiter/readiness=unknown"
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
            if ready(&wire.protocol_snapshot()) {
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
            let mut connection =
                ProtocolFuture::new(h2::server::handshake(server_io), server_observer)
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
                    body.send_data(Bytes::from_static(b"private-first-data"), false)
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
        let (response, mut reset) = sender.send_request(http::Request::new(()), true).unwrap();
        let response = response.await.unwrap();
        let id = response.body().stream_id().as_u32();
        let mut received = response.into_body();
        let first = received.data().await.unwrap().unwrap();
        received
            .flow_control()
            .release_capacity(first.len())
            .unwrap();
        assert_eq!(first, "private-first-data");
        assert!(server.snapshot().directions[Direction::Tx.index()].data_complete > 0);
        let mut body = body_rx.await.unwrap();
        if staged {
            gate.held.store(true, Ordering::SeqCst);
            body.send_data(Bytes::from(vec![b'x'; 32 * 1024]), false)
                .unwrap();
            gate.blocked.notified().await;
        }
        reset.send_reset(h2::Reason::CANCEL);
        until(&server, |state| state.first_application().is_some()).await;
        let applied = server.snapshot();
        let protocol = server.protocol_snapshot();
        assert_eq!(protocol.first_cancel().unwrap().0, id);
        let (cursor, before, after) = protocol.first_application().unwrap();
        assert!(cursor > protocol.first_cancel().unwrap().1);
        let prefix = applied
            .streams
            .iter()
            .flatten()
            .find(|stream| stream.id == id)
            .unwrap();
        assert!(prefix.first_cancel.unwrap().sequence < protocol.first_cancel().unwrap().1);
        assert!(after.empty);
        assert_eq!(after.buffered, 0);
        assert_eq!(after.requested, 0);
        assert_eq!(after.stream_capacity, 0);
        if staged {
            assert!(before.buffered > 0);
            assert_eq!(before.staged, 1);
            assert_eq!(after.staged, 3);
            assert!(protocol.w[9] + protocol.w[10] > 0);
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
            std::future::poll_fn(|cx| sender.poll_ready(cx))
                .await
                .unwrap();
            let (response, stream) = sender.send_request(http::Request::new(()), true).unwrap();
            assert!(response.await.unwrap().body().is_end_stream());
            drop(stream);
        }
        until(&client, |state| state.headers() == 3).await;
        assert_eq!(server.protocol_snapshot().headers(), 3);
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
        assert_eq!(protocol.w[2], 1);
        assert_eq!(protocol.w[4], 1);
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
                        response
                            .send_response(http::Response::new(()), true)
                            .unwrap();
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
                std::future::poll_fn(|cx| sender.poll_ready(cx))
                    .await
                    .unwrap();
                let (response, stream) = sender.send_request(http::Request::new(()), true).unwrap();
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
    fn later_cancel_selection_retains_first_violation_and_original_first_tuple() {
        let live = LiveProtocol::default();
        let before = QueueState {
            buffered: 64,
            requested: 32,
            stream_capacity: 16,
            connection_capacity: 8,
            staged: 1,
            ..QueueState::default()
        };
        let after = QueueState {
            empty: true,
            staged: 3,
            // Reclaimed connection capacity may already have been reassigned.
            connection_capacity: 0,
            ..QueueState::default()
        };
        live.event(Event::Cancel(5), 1);
        live.event(
            Event::Applied {
                stream: 5,
                before,
                after,
            },
            2,
        );
        let first = live.snapshot().first_application();
        live.event(Event::Cancel(7), 3);
        live.event(
            Event::Applied {
                stream: 7,
                before,
                after,
            },
            4,
        );
        let frozen = live.snapshot();
        assert_eq!(frozen.n[3], 7);
        assert_eq!(frozen.f & (1 << 16), 0);
        let broken = QueueState {
            buffered: 1,
            ..after
        };
        live.event(Event::Cancel(9), 5);
        live.event(
            Event::Applied {
                stream: 9,
                before,
                after: broken,
            },
            6,
        );
        live.event(
            Event::Applied {
                stream: 11,
                before,
                after,
            },
            7,
        );
        let state = live.snapshot();
        assert_eq!(state.w[4], 4);
        assert_eq!(state.first_cancel(), Some((5, 1)));
        assert_eq!(state.first_application(), first);
        assert_eq!((state.n[3], state.w[16], state.w[22]), (9, 6, 1));
        assert_ne!(state.f & (1 << 16), 0);
        assert_eq!((frozen.n[3], frozen.w[16], frozen.w[4]), (7, 4, 2));
    }

    #[test]
    fn protocol_counter_width_and_value_footprints_are_frozen() {
        assert_eq!(std::mem::size_of::<ProtocolState>(), 256);
        assert_eq!(std::mem::size_of::<LiveProtocol>(), 256);
        let mut text = String::new();
        let live = LiveProtocol::default();
        live.replace(&ProtocolState::saturated());
        let frozen = live.snapshot();
        frozen.write(&mut text);
        assert_eq!(frozen.w, [u64::MAX; 23]);
        assert_eq!(frozen.n, [u32::MAX; 16]);
        assert_eq!(frozen.f, u64::MAX);
        assert_eq!(text.len(), 428);
        let mut schema_text = String::new();
        schema(&mut schema_text);
        assert_eq!(schema_text.len(), 142);
        assert_eq!(4 * text.len() + schema_text.len(), 1854);
        assert!(4 * text.len() + schema_text.len() <= 1884);
        assert!(text.contains("3w5e11264sgsf"));
        assert!(text.contains("1z141z3"));
        let mut provenance_text = String::new();
        provenance(&mut provenance_text);
        assert_eq!(provenance_text.len(), 332);
        assert!(!text.contains("age"));
    }
}
