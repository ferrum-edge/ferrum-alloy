//! Internal, opt-in benchmark observation seam; not a supported application API.
//!
//! No observer is installed by default. Plaintext callbacks borrow accepted
//! prefixes synchronously; socket callbacks receive counts only. Observers must
//! retain bounded numerical summaries, never payloads, headers, encrypted bytes,
//! TLS material or error text. No global hook exists.

use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The delegated operation; flush and shutdown never supply frame bytes.
#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Read,
    Write,
    WriteVectored,
    Flush,
    Shutdown,
}

/// Numerical classification only; the original result is returned unchanged.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Pending,
    Ok,
    Error,
}

impl Outcome {
    fn of<T>(result: &Poll<io::Result<T>>) -> Self {
        match result {
            Poll::Pending => Self::Pending,
            Poll::Ready(Ok(_)) => Self::Ok,
            Poll::Ready(Err(_)) => Self::Error,
        }
    }
}

/// Instance-owned passive observer. Callbacks must not wait for protocol progress or change I/O.
pub trait IoObserver: Send + Sync {
    /// Only newly received or successfully accepted write bytes are supplied.
    fn prefix(&self, operation: Operation, bytes: &[u8]);
    /// One classification after exactly one inner delegation. EOF is explicit.
    fn outcome(&self, operation: Operation, outcome: Outcome, eof: bool);
    /// Socket poll entry. Only capacity/length is supplied, never encrypted bytes.
    fn socket_start(&self, _operation: Operation, _requested: Option<usize>) {}
    /// Socket poll return; bytes are the actual filled/accepted count.
    fn socket_outcome(&self, _operation: Operation, _outcome: Outcome, _bytes: usize, _eof: bool) {}
    /// A socket-registered waker was invoked, before forwarding to its original target.
    fn socket_wake(&self, _operation: Operation) {}
    /// Original socket wrapper destruction; not an EOF or a successful shutdown.
    fn socket_drop(&self) {}
    /// Rustls demand flags after an established TLS poll (or at attachment).
    /// These are not buffer lengths, delivery acknowledgements or kernel queue state.
    fn tls_state(&self, _flags: [bool; 3]) {}
}

/// Selects a bounded observation for a real accepted socket, before serving it.
pub trait ConnectionObserver: Send + Sync + std::fmt::Debug {
    /// `local`/`remote` are actual socket endpoints. Encrypted bytes are never supplied.
    fn accepted(
        &self,
        local: Option<SocketAddr>,
        remote: SocketAddr,
        tls: bool,
    ) -> Option<Arc<dyn IoObserver>>;
}

struct SocketWake {
    observer: Arc<dyn IoObserver>,
    operation: Operation,
    target: Mutex<Option<Waker>>,
}

impl Wake for SocketWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.observer.socket_wake(self.operation);
        let target = self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(target) = target {
            target.wake();
        }
    }
}

/// Numerical socket boundary below TLS. Handshake I/O is excluded by attaching
/// the observer only after establishment. Each poll delegates exactly once.
pub struct SocketIo<I> {
    inner: I,
    observer: Option<Arc<dyn IoObserver>>,
    wakes: [Option<Arc<SocketWake>>; 3],
}

impl<I> SocketIo<I> {
    /// Initially passive, including during TLS establishment.
    pub fn new(inner: I) -> Self {
        Self {
            inner,
            observer: None,
            wakes: std::array::from_fn(|_| None),
        }
    }

    /// Attach on the original socket without moving, duplicating or redialing it.
    pub fn observe(&mut self, observer: Option<Arc<dyn IoObserver>>) {
        self.wakes = std::array::from_fn(|index| {
            observer.as_ref().map(|observer| {
                Arc::new(SocketWake {
                    observer: Arc::clone(observer),
                    operation: [Operation::Read, Operation::Write, Operation::Flush][index],
                    target: Mutex::new(None),
                })
            })
        });
        self.observer = observer;
    }

    /// Inspect the original fixture socket.
    pub fn inner_mut(&mut self) -> &mut I {
        &mut self.inner
    }

    fn start(&self, index: usize, requested: Option<usize>, cx: &Context<'_>) -> Option<Waker> {
        let wake = self.wakes[index].as_ref()?;
        *wake
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cx.waker().clone());
        wake.observer.socket_start(wake.operation, requested);
        Some(Waker::from(Arc::clone(wake)))
    }

    fn finish<T>(
        &self,
        operation: Operation,
        result: &Poll<io::Result<T>>,
        bytes: usize,
        eof: bool,
    ) {
        if let Some(observer) = &self.observer {
            observer.socket_outcome(operation, Outcome::of(result), bytes, eof);
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for SocketIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let capacity = buf.remaining();
        let waker = this.start(0, Some(capacity), cx);
        let result = match &waker {
            Some(waker) => {
                Pin::new(&mut this.inner).poll_read(&mut Context::from_waker(waker), buf)
            }
            None => Pin::new(&mut this.inner).poll_read(cx, buf),
        };
        let bytes = buf.filled().len() - before;
        let eof = matches!(&result, Poll::Ready(Ok(()))) && bytes == 0 && capacity > 0;
        this.finish(Operation::Read, &result, bytes, eof);
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for SocketIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let waker = this.start(1, Some(buf.len()), cx);
        let result = match &waker {
            Some(waker) => {
                Pin::new(&mut this.inner).poll_write(&mut Context::from_waker(waker), buf)
            }
            None => Pin::new(&mut this.inner).poll_write(cx, buf),
        };
        let bytes = match &result {
            Poll::Ready(Ok(bytes)) => *bytes,
            _ => 0,
        };
        this.finish(Operation::Write, &result, bytes, false);
        result
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let requested = bufs
            .iter()
            .fold(0_usize, |total, buf| total.saturating_add(buf.len()));
        let waker = this.start(1, Some(requested), cx);
        let result = match &waker {
            Some(waker) => {
                Pin::new(&mut this.inner).poll_write_vectored(&mut Context::from_waker(waker), bufs)
            }
            None => Pin::new(&mut this.inner).poll_write_vectored(cx, bufs),
        };
        let bytes = match &result {
            Poll::Ready(Ok(bytes)) => *bytes,
            _ => 0,
        };
        this.finish(Operation::WriteVectored, &result, bytes, false);
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let waker = this.start(2, None, cx);
        let result = match &waker {
            Some(waker) => Pin::new(&mut this.inner).poll_flush(&mut Context::from_waker(waker)),
            None => Pin::new(&mut this.inner).poll_flush(cx),
        };
        this.finish(Operation::Flush, &result, 0, false);
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl<I> Drop for SocketIo<I> {
    fn drop(&mut self) {
        if let Some(observer) = &self.observer {
            observer.socket_drop();
        }
        for wake in self.wakes.iter().flatten() {
            let _ = wake
                .target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
    }
}

/// Samples established rustls demand without processing packets or exposing bytes.
#[cfg(feature = "tls")]
pub struct TlsIo<I> {
    inner: tokio_rustls::TlsStream<I>,
    observer: Option<Arc<dyn IoObserver>>,
}

#[cfg(feature = "tls")]
impl<I> TlsIo<I> {
    /// Attach only after TLS establishment; no handshake or peer material is observed.
    pub fn optional(
        inner: tokio_rustls::TlsStream<I>,
        observer: Option<Arc<dyn IoObserver>>,
    ) -> Self {
        let this = Self { inner, observer };
        this.sample();
        this
    }

    /// Inspect the established fixture TLS stream without re-establishing it.
    pub fn inner_mut(&mut self) -> &mut tokio_rustls::TlsStream<I> {
        &mut self.inner
    }

    fn sample(&self) {
        if let Some(observer) = &self.observer {
            let session = self.inner.get_ref().1;
            observer.tls_state([
                session.wants_read(),
                session.wants_write(),
                session.is_handshaking(),
            ]);
        }
    }
}

#[cfg(feature = "tls")]
impl<I: AsyncRead + AsyncWrite + Unpin> AsyncRead for TlsIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        this.sample();
        result
    }
}

#[cfg(feature = "tls")]
impl<I: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TlsIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.sample();
        result
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.sample();
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        this.sample();
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.sample();
        result
    }
}

/// Plaintext boundary between Alloy's Transport and established TCP/TLS I/O.
pub struct PlaintextIo<I> {
    inner: I,
    observer: Option<Arc<dyn IoObserver>>,
}

impl<I> PlaintextIo<I> {
    /// Explicit fixture construction; no buffering, gating or extra polling.
    pub fn new(inner: I, observer: Arc<impl IoObserver + 'static>) -> Self {
        Self::optional(inner, Some(observer))
    }

    /// No callbacks are invoked when the runtime observer is absent.
    pub fn optional(inner: I, observer: Option<Arc<dyn IoObserver>>) -> Self {
        Self { inner, observer }
    }

    /// Inspect the delegated fixture transport.
    pub fn inner_mut(&mut self) -> &mut I {
        &mut self.inner
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for PlaintextIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let capacity = buf.remaining();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Some(observer) = &this.observer {
            let bytes = &buf.filled()[before..];
            if !bytes.is_empty() {
                observer.prefix(Operation::Read, bytes);
            }
            let eof = matches!(&result, Poll::Ready(Ok(()))) && bytes.is_empty() && capacity > 0;
            observer.outcome(Operation::Read, Outcome::of(&result), eof);
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for PlaintextIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Some(observer) = &this.observer {
            if let Poll::Ready(Ok(count)) = &result {
                observer.prefix(Operation::Write, &buf[..*count]);
            }
            observer.outcome(Operation::Write, Outcome::of(&result), false);
        }
        result
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Some(observer) = &this.observer {
            if let Poll::Ready(Ok(count)) = &result {
                let mut remaining = *count;
                for buf in bufs {
                    let count = remaining.min(buf.len());
                    if count > 0 {
                        observer.prefix(Operation::WriteVectored, &buf[..count]);
                    }
                    remaining -= count;
                    if remaining == 0 {
                        break;
                    }
                }
            }
            observer.outcome(Operation::WriteVectored, Outcome::of(&result), false);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        if let Some(observer) = &this.observer {
            observer.outcome(Operation::Flush, Outcome::of(&result), false);
        }
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        if let Some(observer) = &this.observer {
            observer.outcome(Operation::Shutdown, Outcome::of(&result), false);
        }
        result
    }
}
