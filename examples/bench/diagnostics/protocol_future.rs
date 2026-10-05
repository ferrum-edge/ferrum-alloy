//! Temporary experiment adapter, included only by observer.patch.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::IoObserver;

struct Bridge(Arc<dyn IoObserver>);

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection-owned numerical observer")
    }
}

impl h2::alloy_diagnostics::Observer for Bridge {
    fn event(&self, event: h2::alloy_diagnostics::Event) {
        self.0.protocol_event(event);
    }

    fn runtime_flags(&self) -> Option<&std::sync::atomic::AtomicU64> {
        self.0.protocol_flags()
    }
}

pin_project_lite::pin_project! {
    /// Poll-scoped context for h2 construction on the original connection.
    /// h2 captures this observer and restores it on its own driver polls.
    pub struct ProtocolFuture<F> {
        #[pin]
        inner: F,
        observer: Option<Arc<dyn h2::alloy_diagnostics::Observer>>,
    }
}

impl<F> ProtocolFuture<F> {
    /// An absent runtime observer explicitly masks any enclosing context.
    pub fn new(inner: F, observer: Option<Arc<dyn IoObserver>>) -> Self {
        Self {
            inner,
            observer: observer.map(|observer| {
                Arc::new(Bridge(observer)) as Arc<dyn h2::alloy_diagnostics::Observer>
            }),
        }
    }
}

impl<F: Future> Future for ProtocolFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        h2::alloy_diagnostics::scope(this.observer.clone(), || this.inner.poll(cx))
    }
}

/// A labelled control gate above established TLS, below plaintext observation.
#[derive(Default)]
pub struct ReceiveGate {
    held: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
    target: std::sync::Mutex<Option<std::task::Waker>>,
}

impl ReceiveGate {
    /// Begins open; the exchange-completion seam arms it once.
    pub fn hold(&self) {
        self.held.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The control releases exactly once, forwarding the registered read waker.
    pub fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.held.store(false, std::sync::atomic::Ordering::SeqCst);
        let target = self.target.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(target) = target {
            target.wake();
        }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        use std::sync::atomic::Ordering;
        if self.held.load(Ordering::SeqCst) {
            *self.target.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
            if self.held.load(Ordering::SeqCst) && !self.released.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
        }
        Poll::Ready(())
    }
}

/// Writes, flush and shutdown pass through once; only client reads can be held.
pub struct ReadGate<I> {
    inner: I,
    gate: Option<Arc<ReceiveGate>>,
}

impl<I> ReadGate<I> {
    /// Wrap established I/O with this instance's optional receive gate.
    pub fn new(inner: I, gate: Option<Arc<ReceiveGate>>) -> Self {
        Self { inner, gate }
    }
}

impl<I: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for ReadGate<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Some(gate) = &this.gate
            && gate.poll(cx).is_pending()
        {
            return Poll::Pending;
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<I: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ReadGate<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bytes)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
