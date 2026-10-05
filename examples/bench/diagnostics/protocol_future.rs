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
