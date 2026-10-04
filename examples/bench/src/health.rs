//! Bounded, instance-owned test observations. No protocol contents are retained.

use std::fmt::Write;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

const TASK_SLOTS: usize = 160;
const REQUEST_SLOTS: usize = 72;

#[derive(Clone, Default)]
pub(crate) struct PollState {
    pub(crate) polls: u64,
    pub(crate) inner_polls: u64,
    pub(crate) pending: u64,
    pub(crate) ready: u64,
    pub(crate) wakes: u64,
    pub(crate) dropped: bool,
    first: Option<Instant>,
    last: Option<Instant>,
    last_wake: Option<Instant>,
    max_sync: Option<Duration>,
}

#[derive(Default)]
pub(crate) struct PollObservation {
    state: Mutex<PollState>,
    changed: tokio::sync::Notify,
}

impl PollObservation {
    pub(crate) fn snapshot(&self) -> PollState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) async fn pending(&self) {
        while self.snapshot().pending == 0 {
            self.changed.notified().await;
        }
    }

    pub(crate) async fn ready(&self) {
        while self.snapshot().ready == 0 {
            self.changed.notified().await;
        }
    }

    fn begin(&self) -> Instant {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.polls = state.polls.saturating_add(1);
        let _ = state.first.get_or_insert(now);
        state.last = Some(now);
        now
    }

    fn finish(&self, start: Instant, ready: bool, inner: bool) {
        let duration = start.elapsed();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.inner_polls = state.inner_polls.saturating_add(u64::from(inner));
        if ready {
            state.ready = state.ready.saturating_add(1);
        } else {
            state.pending = state.pending.saturating_add(1);
        }
        let previous = state.max_sync.unwrap_or(duration);
        state.max_sync = Some(previous.max(duration));
        drop(state);
        self.changed.notify_one();
    }

    fn dropped(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).dropped = true;
    }

    fn write(&self, text: &mut impl Write, now: Instant) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let age = |at: Option<Instant>| at.map(|at| now.saturating_duration_since(at).as_micros());
        let _ = write!(
            text,
            "polls={} inner={} pending={} ready={} drop={} wakes={} \
             first_age_us={:?} last_age_us={:?} wake_age_us={:?} max_sync_us={:?}",
            state.polls,
            state.inner_polls,
            state.pending,
            state.ready,
            state.dropped,
            state.wakes,
            age(state.first),
            age(state.last),
            age(state.last_wake),
            state.max_sync.map(|duration| duration.as_micros()),
        );
    }
}

struct ForwardWake {
    observation: Arc<PollObservation>,
    target: Mutex<Option<Waker>>,
}

impl Wake for ForwardWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        {
            let mut state = self
                .observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.wakes = state.wakes.saturating_add(1);
            state.last_wake = Some(Instant::now());
        }
        let target = self.target.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Never forward a wake while holding an observation or target lock.
        if let Some(target) = target {
            target.wake();
        }
    }
}

pub(crate) struct Observed<F> {
    inner: Pin<Box<F>>,
    wake: Option<Arc<ForwardWake>>,
    gate: Option<Arc<Gate>>,
}

impl<F> Observed<F> {
    pub(crate) fn new(future: F, observation: Option<Arc<PollObservation>>) -> Self {
        Self {
            inner: Box::pin(future),
            wake: observation.map(|observation| {
                Arc::new(ForwardWake {
                    observation,
                    target: Mutex::new(None),
                })
            }),
            gate: None,
        }
    }

    pub(crate) fn gated(mut self, gate: Option<Arc<Gate>>) -> Self {
        self.gate = gate;
        self
    }
}

impl<F: Future> Future for Observed<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(wake) = &this.wake else {
            return this.inner.as_mut().poll(cx);
        };
        *wake.target.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        let waker = Waker::from(Arc::clone(wake));
        let mut context = Context::from_waker(&waker);
        let start = wake.observation.begin();
        if let Some(gate) = &this.gate
            && gate.poll(&mut context).is_pending()
        {
            wake.observation.finish(start, false, false);
            return Poll::Pending;
        }
        let result = this.inner.as_mut().poll(&mut context);
        wake.observation.finish(start, result.is_ready(), true);
        result
    }
}

impl<F> Drop for Observed<F> {
    fn drop(&mut self) {
        if let Some(wake) = &self.wake {
            wake.observation.dropped();
            let _ = wake.target.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

// Only controlled tests install a gate. Registration and release are local to
// one health instance; a blocked wrapper is explicitly not an inner wire poll.
#[derive(Default)]
pub(crate) struct Gate {
    released: AtomicBool,
    reached: AtomicBool,
    changed: tokio::sync::Notify,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        self.reached.store(true, Ordering::SeqCst);
        self.changed.notify_one();
        if self.released.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub(crate) async fn reached(&self) {
        while !self.reached.load(Ordering::SeqCst) {
            self.changed.notified().await;
        }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        let waker = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct Task {
    kind: &'static str,
    owner: usize,
    generation: u64,
    socket: Option<SocketAddr>,
    ordinal: usize,
    observation: Arc<PollObservation>,
}

pub(crate) struct RequestObservation {
    pub(crate) socket: Option<SocketAddr>,
    pub(crate) ordinal: usize,
    entered: Instant,
    pub(crate) response: Mutex<Option<Instant>>,
    pub(crate) handler: Arc<PollObservation>,
    pub(crate) body: Arc<PollObservation>,
    pub(crate) frames: AtomicU64,
}

struct Slots {
    tasks: [Option<Task>; TASK_SLOTS],
    requests: [Option<Arc<RequestObservation>>; REQUEST_SLOTS],
    tasks_omitted: u64,
    requests_omitted: u64,
}

pub(crate) struct Observer {
    slots: Mutex<Slots>,
    pub(crate) wire_gate: Mutex<Option<Arc<Gate>>>,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            slots: Mutex::new(Slots {
                tasks: std::array::from_fn(|_| None),
                requests: std::array::from_fn(|_| None),
                tasks_omitted: 0,
                requests_omitted: 0,
            }),
            wire_gate: Mutex::new(None),
        }
    }
}

impl Observer {
    pub(crate) fn tasks(&self, kind: &'static str) -> Vec<Arc<PollObservation>> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .tasks
            .iter()
            .flatten()
            .filter(|task| task.kind == kind)
            .map(|task| Arc::clone(&task.observation))
            .collect()
    }

    pub(crate) fn requests(&self) -> Vec<Arc<RequestObservation>> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.requests.iter().flatten().map(Arc::clone).collect()
    }

    pub(crate) fn task(
        &self,
        kind: &'static str,
        owner: usize,
        generation: u64,
        socket: Option<SocketAddr>,
        ordinal: usize,
    ) -> Option<Arc<PollObservation>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.tasks.iter_mut().find(|slot| slot.is_none()) else {
            slots.tasks_omitted = slots.tasks_omitted.saturating_add(1);
            return None;
        };
        let observation = Arc::new(PollObservation::default());
        *slot = Some(Task {
            kind,
            owner,
            generation,
            socket,
            ordinal,
            observation: Arc::clone(&observation),
        });
        Some(observation)
    }

    pub(crate) fn request(&self, socket: Option<SocketAddr>) -> Option<Arc<RequestObservation>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let ordinal = slots
            .requests
            .iter()
            .flatten()
            .filter(|request| request.socket == socket)
            .count()
            + 1;
        let Some(slot) = slots.requests.iter_mut().find(|slot| slot.is_none()) else {
            slots.requests_omitted = slots.requests_omitted.saturating_add(1);
            return None;
        };
        let request = Arc::new(RequestObservation {
            socket,
            ordinal,
            entered: Instant::now(),
            response: Mutex::new(None),
            handler: Arc::new(PollObservation::default()),
            body: Arc::new(PollObservation::default()),
            frames: AtomicU64::new(0),
        });
        *slot = Some(Arc::clone(&request));
        Some(request)
    }

    pub(crate) fn write(&self, text: &mut impl Write, now: Instant) {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(
            text,
            "observer slots(task,request)=({TASK_SLOTS},{REQUEST_SLOTS}) omitted=({},{}) \
             child_ready=unit-completion-inner-wire-result-unknown socket_ordinal_is_not_stream_id",
            slots.tasks_omitted, slots.requests_omitted,
        );
        for (id, task) in slots.tasks.iter().enumerate() {
            if let Some(task) = task {
                let _ = write!(
                    text,
                    "task={id} kind={} owner={} gen={} socket={:?} ordinal={} ",
                    task.kind, task.owner, task.generation, task.socket, task.ordinal,
                );
                task.observation.write(text, now);
                let _ = writeln!(text);
            }
        }
        for request in slots.requests.iter().flatten() {
            let response = *request.response.lock().unwrap_or_else(|e| e.into_inner());
            let _ = write!(
                text,
                "server socket={:?} ordinal={} entry_age_us={} response_age_us={:?} \
                 frames={} router_future(",
                request.socket,
                request.ordinal,
                now.saturating_duration_since(request.entered).as_micros(),
                response.map(|at| now.saturating_duration_since(at).as_micros()),
                request.frames.load(Ordering::Relaxed),
            );
            request.handler.write(text, now);
            let _ = write!(text, ") body(");
            request.body.write(text, now);
            let _ = writeln!(text, ")");
        }
    }
}

pub(crate) async fn response(
    future: impl Future<Output = axum::response::Response>,
    observation: Option<Arc<RequestObservation>>,
) -> axum::response::Response {
    response_with_gates(future, observation, None, None).await
}

pub(crate) async fn response_with_gates(
    future: impl Future<Output = axum::response::Response>,
    observation: Option<Arc<RequestObservation>>,
    headers: Option<Arc<Gate>>,
    data: Option<Arc<Gate>>,
) -> axum::response::Response {
    let Some(observation) = observation else {
        return future.await;
    };
    let response = Observed::new(future, Some(Arc::clone(&observation.handler)))
        .gated(headers)
        .await;
    *observation.response.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    response.map(|body| {
        axum::body::Body::new(ObservedBody {
            inner: Box::pin(body),
            observation,
            gate: data,
        })
    })
}

struct ObservedBody<B> {
    inner: Pin<Box<B>>,
    observation: Arc<RequestObservation>,
    gate: Option<Arc<Gate>>,
}

impl<B: hyper::body::Body> hyper::body::Body for ObservedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let start = this.observation.body.begin();
        if let Some(gate) = &this.gate && gate.poll(cx).is_pending() {
            this.observation.body.finish(start, false, false);
            return Poll::Pending;
        }
        let result = this.inner.as_mut().poll_frame(cx);
        this.observation.body.finish(start, result.is_ready(), true);
        if matches!(&result, Poll::Ready(Some(Ok(_)))) {
            this.observation.frames.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for ObservedBody<B> {
    fn drop(&mut self) {
        self.observation.body.dropped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_bound_tasks_requests_and_socket_ordinals_per_instance() {
        let observer = Observer::default();
        let other = Observer::default();
        let socket = Some(SocketAddr::from(([127, 0, 0, 1], 12345)));
        for ordinal in 0..TASK_SLOTS + 5 {
            let task = observer.task("test", 0, 1, socket, ordinal);
            assert_eq!(task.is_some(), ordinal < TASK_SLOTS);
        }
        for ordinal in 1..=REQUEST_SLOTS + 5 {
            let request = observer.request(socket);
            assert_eq!(request.is_some(), ordinal <= REQUEST_SLOTS);
            if let Some(request) = request {
                assert_eq!(request.ordinal, ordinal);
                assert_eq!(request.socket, socket);
            }
        }
        let slots = observer.slots.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(slots.tasks_omitted, 5);
        assert_eq!(slots.requests_omitted, 5);
        assert!(other.tasks("test").is_empty());
        assert!(other.requests().is_empty());
    }
}
