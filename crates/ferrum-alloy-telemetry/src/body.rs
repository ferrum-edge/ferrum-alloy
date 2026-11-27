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
        // `None` once the body ended or failed: it is dropped before the
        // request is finalized, so operations it still owned record their
        // cancellation in this request's evidence.
        #[pin]
        inner: Option<B>,
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
            inner: Some(inner),
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
        } else if body.inner.as_ref().is_some_and(Body::is_end_stream) {
            body.finish(BodyOutcome::Completed);
        }
        body
    }

    /// A body without accounting (the request is counted by an outer layer).
    pub(crate) fn passthrough(inner: B) -> Self {
        Self {
            inner: Some(inner),
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
        // Operations run while producing the body belong to this request.
        let _scope = this
            .finalizer
            .as_ref()
            .map(|finalizer| crate::evidence::RequestScope::enter(finalizer.collector()));
        let mut inner = this.inner;
        let Some(body) = inner.as_mut().as_pin_mut() else {
            return Poll::Ready(None);
        };
        let result = body.poll_frame(cx);
        let terminal = match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(finalizer) = this.finalizer.as_mut() {
                    if let Some(data) = frame.data_ref() {
                        finalizer.add_body_bytes(data.remaining() as u64);
                    } else if frame.is_trailers() {
                        finalizer.saw_trailers();
                    }
                }
                inner
                    .as_ref()
                    .get_ref()
                    .as_ref()
                    .is_some_and(Body::is_end_stream)
                    .then_some(BodyOutcome::Completed)
            }
            Poll::Ready(Some(Err(_))) => Some(BodyOutcome::Error),
            Poll::Ready(None) => Some(BodyOutcome::Completed),
            Poll::Pending => None,
        };
        if let Some(outcome) = terminal {
            // Drop the inner body first: an operation it still owns (a
            // stream stopped while awaiting one) records its cancellation
            // before the evidence is closed.
            inner.set(None);
            if let Some(mut finalizer) = this.finalizer.take() {
                finalizer.finish(outcome);
            }
        }
        if this.finalizer.is_none() {
            drop(_entered);
            *this.span = None;
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.as_ref().is_none_or(Body::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        self.inner
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), Body::size_hint)
    }
}

impl<B> std::fmt::Debug for InstrumentedBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstrumentedBody")
            .field("finalized", &self.finalizer.is_none())
            .finish_non_exhaustive()
    }
}
