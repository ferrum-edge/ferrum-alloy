//! Response body lifecycle accounting.
//!
//! A service future returning a response only proves that response *headers*
//! exist. [`InstrumentedBody`] keeps request accounting open until the body
//! reaches its end, fails, or is dropped, and finalizes exactly once.
//!
//! "Completed" means the final frame was handed to Hyper. It does not prove
//! the remote client received the bytes.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Buf;
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;

use crate::layer::{BodyOutcome, Finalizer};

pin_project! {
    /// A response body that finalizes request telemetry when it ends.
    pub struct InstrumentedBody<B> {
        #[pin]
        inner: B,
        // `None` once finalized, or for pass-through (duplicate layer).
        finalizer: Option<Finalizer>,
        // Entered while polling so the span's end time tracks the last body
        // activity. Dropped at finalization.
        span: Option<tracing::Span>,
    }
}

impl<B> InstrumentedBody<B> {
    pub(crate) fn new(inner: B, finalizer: Option<Finalizer>) -> Self
    where
        B: Body,
    {
        let span = finalizer.as_ref().map(|f| f.span().clone());
        let mut body = Self {
            inner,
            finalizer,
            span,
        };
        // Hyper never polls a body that is already at its end, and discards
        // bodies the protocol forbids (HEAD, 1xx, 204, 304), so those are
        // finalized now rather than being misreported as cancelled on drop.
        let protocol_outcome = body
            .finalizer
            .as_ref()
            .and_then(Finalizer::protocol_body_outcome);
        if let Some(outcome) = protocol_outcome {
            body.finish(outcome);
        } else if body.inner.is_end_stream() {
            body.finish(BodyOutcome::Completed);
        }
        body
    }

    /// A body without accounting (the request is counted by an outer layer).
    pub(crate) fn passthrough(inner: B) -> Self {
        Self {
            inner,
            finalizer: None,
            span: None,
        }
    }

    fn finish(&mut self, outcome: BodyOutcome) {
        if let Some(mut finalizer) = self.finalizer.take() {
            finalizer.finish(outcome);
        }
        self.span = None;
    }
}

impl<B> Body for InstrumentedBody<B>
where
    B: Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        // A synchronous guard scoped to this poll: never held across an await.
        let _entered = this.span.as_ref().map(tracing::Span::enter);
        let mut inner = this.inner;
        let result = inner.as_mut().poll_frame(cx);
        let finalize = |finalizer: &mut Option<Finalizer>, outcome| {
            if let Some(mut f) = finalizer.take() {
                f.finish(outcome);
            }
        };
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(finalizer) = this.finalizer.as_mut() {
                    if let Some(data) = frame.data_ref() {
                        finalizer.add_body_bytes(data.remaining() as u64);
                    } else if frame.is_trailers() {
                        finalizer.saw_trailers();
                    }
                }
                if inner.is_end_stream() {
                    finalize(&mut *this.finalizer, BodyOutcome::Completed);
                }
            }
            Poll::Ready(Some(Err(_))) => finalize(&mut *this.finalizer, BodyOutcome::Error),
            Poll::Ready(None) => finalize(&mut *this.finalizer, BodyOutcome::Completed),
            Poll::Pending => {}
        }
        if this.finalizer.is_none() {
            drop(_entered);
            *this.span = None;
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> std::fmt::Debug for InstrumentedBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstrumentedBody")
            .field("finalized", &self.finalizer.is_none())
            .finish_non_exhaustive()
    }
}
