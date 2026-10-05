//! Internal, opt-in benchmark observation seam; not a supported application API.
//!
//! No observer is installed by default. Callbacks borrow accepted plaintext
//! prefixes synchronously and must retain only bounded numerical summaries,
//! never payloads, headers, TLS material or error text. No global hook exists.

use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

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
}

/// Selects a bounded observation for a real accepted socket, before serving it.
pub trait ConnectionObserver: Send + Sync + std::fmt::Debug {
    /// `local`/`remote` are actual socket endpoints; TLS bytes remain plaintext.
    fn accepted(
        &self,
        local: Option<SocketAddr>,
        remote: SocketAddr,
        tls: bool,
    ) -> Option<Arc<dyn IoObserver>>;
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
            observer.outcome(
                Operation::Read,
                Outcome::of(&result),
                eof,
            );
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
            observer.outcome(
                Operation::Write,
                Outcome::of(&result),
                false,
            );
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
            observer.outcome(
                Operation::WriteVectored,
                Outcome::of(&result),
                false,
            );
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
            observer.outcome(
                Operation::Flush,
                Outcome::of(&result),
                false,
            );
        }
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        if let Some(observer) = &this.observer {
            observer.outcome(
                Operation::Shutdown,
                Outcome::of(&result),
                false,
            );
        }
        result
    }
}
