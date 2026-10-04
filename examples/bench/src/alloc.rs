//! A counting global allocator, installed in the benchmark binary only.
//!
//! Counting is off until [`enable`] is called: a shared counter bumped on
//! every allocation changes the throughput it measures, so counting runs are
//! reported separately and never compared with non-counting ones. Allocations
//! are attributed to the [`Role`] of the allocating thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Who a thread works for. CPU time and allocations are attributed by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The service under test, including its exporter threads and anything
    /// else not marked otherwise.
    Service = 0,
    /// The load generator.
    Client = 1,
    /// The in-process OTLP collector stub.
    Collector = 2,
}

impl Role {
    pub(crate) const ALL: [Self; 3] = [Self::Service, Self::Client, Self::Collector];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Client => "client",
            Self::Collector => "collector",
        }
    }
}

thread_local! {
    static ROLE: Cell<u8> = const { Cell::new(Role::Service as u8) };
}

/// Marks the current thread. Call it from runtime thread-start hooks.
pub(crate) fn set_role(role: Role) {
    let _ = ROLE.try_with(|current| current.set(role as u8));
}

/// Counters for one role, padded to their own cache lines.
#[repr(align(128))]
struct Slot {
    calls: AtomicU64,
    bytes: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static SLOTS: [Slot; Role::ALL.len()] = [Slot::new(), Slot::new(), Slot::new()];

/// Starts counting. There is no way to stop; take deltas between snapshots.
pub(crate) fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Allocation calls (including reallocations) and bytes requested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) calls: u64,
    pub(crate) bytes: u64,
}

impl Counts {
    pub(crate) fn since(self, earlier: Self) -> Self {
        Self {
            calls: self.calls.saturating_sub(earlier.calls),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// The counters of every role, indexed like [`Role::ALL`].
pub(crate) fn snapshot() -> [Counts; Role::ALL.len()] {
    SLOTS.each_ref().map(|slot| Counts {
        calls: slot.calls.load(Ordering::Relaxed),
        bytes: slot.bytes.load(Ordering::Relaxed),
    })
}

struct Counting;

#[global_allocator]
static GLOBAL: Counting = Counting;

#[inline]
fn record(size: usize) {
    #[cfg(test)]
    observation::record(size);
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // `ROLE` is const-initialized and has no destructor, so reading it never
    // allocates and never fails; `try_with` keeps that true regardless.
    let role = ROLE.try_with(Cell::get).unwrap_or(Role::Service as u8);
    if let Some(slot) = SLOTS.get(usize::from(role)) {
        slot.calls.fetch_add(1, Ordering::Relaxed);
        slot.bytes.fetch_add(size as u64, Ordering::Relaxed);
    }
}

// Test observation never enables or updates production counters. Active
// counters are owned by the scope and temporarily stored by value in TLS.
// These const-initialized Copy cells have no destructor or allocation path.
#[cfg(test)]
mod observation {
    use std::marker::PhantomData;
    use std::rc::Rc;

    use super::*;

    thread_local! {
        static OBSERVER: Cell<Option<[Counts; Role::ALL.len()]>> = const { Cell::new(None) };
    }

    pub(super) struct Scope {
        previous: Option<[Counts; Role::ALL.len()]>,
        counters: Option<[Counts; Role::ALL.len()]>,
        role: u8,
        // A scope cannot move to a different allocating thread.
        _thread: PhantomData<Rc<()>>,
    }

    impl Scope {
        pub(super) fn enter() -> Self {
            let previous = OBSERVER
                .with(|observer| observer.replace(Some([Counts::default(); Role::ALL.len()])));
            Self {
                previous,
                counters: None,
                role: ROLE.with(Cell::get),
                _thread: PhantomData,
            }
        }

        pub(super) fn snapshot(&self) -> [Counts; Role::ALL.len()] {
            self.counters
                .or_else(|| OBSERVER.with(Cell::get))
                .unwrap_or_default()
        }

        pub(super) fn close(&mut self) {
            if self.counters.is_none() {
                self.counters = OBSERVER
                    .try_with(|observer| observer.replace(self.previous.take()))
                    .ok()
                    .flatten();
                let _ = ROLE.try_with(|role| role.set(self.role));
            }
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            self.close();
        }
    }

    pub(super) fn active() -> bool {
        OBSERVER.with(|observer| observer.get().is_some())
    }

    pub(super) fn record(size: usize) {
        let _ = OBSERVER.try_with(|observer| {
            if let Some(mut counters) = observer.get() {
                let role = ROLE.try_with(Cell::get).unwrap_or(Role::Service as u8);
                if let Some(slot) = counters.get_mut(usize::from(role)) {
                    slot.calls = slot.calls.saturating_add(1);
                    slot.bytes = slot.bytes.saturating_add(size as u64);
                }
                observer.set(Some(counters));
            }
        });
    }
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract; counting touches only atomics and a
// const-initialized thread-local, and never allocates.
#[expect(
    unsafe_code,
    reason = "GlobalAlloc is an unsafe trait; every method forwards to System unchanged"
)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract, and
        // `ptr` came from `System` through this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract, and
        // `ptr` came from `System` through this allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn scoped_allocations_isolate_concurrent_threads_and_restore_on_exit() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let counted_barrier = std::sync::Arc::clone(&barrier);
        let before_global = snapshot();
        let counted = std::thread::spawn(move || {
            // Warm the barrier's platform primitives before observing allocations;
            // pthread-backed synchronization can allocate lazily on its first wait.
            counted_barrier.wait();
            let original_role = ROLE.with(Cell::get);
            let mut scope = observation::Scope::enter();
            let active = observation::active();
            let globally_enabled = enabled();
            counted_barrier.wait();
            set_role(Role::Collector);
            let data = std::hint::black_box(vec![0_u8; 4096]);
            set_role(Role::Client);
            let client = std::hint::black_box(vec![0_u8; 2048]);
            counted_barrier.wait();
            let after = scope.snapshot();
            // All assertions follow the final rendezvous, so a failed isolation
            // assertion cannot strand the other thread at a barrier.
            assert!(active);
            assert!(!globally_enabled);
            assert_eq!(after[Role::Service as usize], Counts::default());
            assert!(after[Role::Collector as usize].calls >= 1);
            assert!(after[Role::Collector as usize].bytes >= 4096);
            assert!(after[Role::Client as usize].calls >= 1);
            assert!(after[Role::Client as usize].bytes >= 2048);
            scope.close();
            assert!(!observation::active());
            assert_eq!(ROLE.with(Cell::get), original_role);
            let unobserved = std::hint::black_box(vec![0_u8; 8192]);
            assert_eq!(scope.snapshot(), after);
            assert!(!enabled());
            drop(scope);
            drop(unobserved);
            drop(client);
            drop(data);
        });
        let uncounted = std::thread::spawn(move || {
            barrier.wait();
            barrier.wait();
            let active = observation::active();
            let globally_enabled = enabled();
            let data = std::hint::black_box(vec![0_u8; 16_384]);
            barrier.wait();
            assert!(!active);
            assert!(!globally_enabled);
            assert!(!observation::active());
            drop(data);
        });
        counted.join().unwrap();
        uncounted.join().unwrap();
        assert_eq!(snapshot(), before_global);
        assert!(!enabled());
    }

    #[test]
    fn dropping_nested_scope_restores_outer_counters_and_role() {
        let original_role = ROLE.with(Cell::get);
        let outer = observation::Scope::enter();
        set_role(Role::Collector);
        let data = std::hint::black_box(vec![0_u8; 4096]);
        let before = outer.snapshot();
        {
            let inner = observation::Scope::enter();
            set_role(Role::Client);
            let data = std::hint::black_box(vec![0_u8; 2048]);
            assert!(inner.snapshot()[Role::Client as usize].bytes >= 2048);
            assert_eq!(
                inner.snapshot()[Role::Collector as usize],
                Counts::default()
            );
            drop(data);
        }
        assert!(observation::active());
        assert_eq!(ROLE.with(Cell::get), Role::Collector as u8);
        assert_eq!(outer.snapshot(), before);
        drop(outer);
        assert!(!observation::active());
        assert_eq!(ROLE.with(Cell::get), original_role);
        assert!(!enabled());
        drop(data);
    }

    /// The manifest restates `[workspace.lints]` with `unsafe_code` lowered
    /// from `forbid` to `deny`, so that this module alone can expect it. Any
    /// other difference is drift.
    #[test]
    fn lints_match_the_workspace_except_unsafe_code() {
        let manifest = |path: &str| {
            let text = std::fs::read_to_string(path).unwrap();
            toml::from_str::<toml::Table>(&text).unwrap()
        };
        let root = manifest(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"));
        let bench = manifest(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        let mut expected = root["workspace"]["lints"].clone();
        let rust = expected
            .get_mut("rust")
            .and_then(toml::Value::as_table_mut)
            .unwrap();
        let workspace_level = rust.insert("unsafe_code".into(), "deny".into());
        assert_eq!(workspace_level, Some("forbid".into()));
        assert_eq!(bench["lints"], expected);
    }

    #[test]
    fn since_saturates() {
        let later = Counts { calls: 1, bytes: 1 };
        let earlier = Counts { calls: 5, bytes: 5 };
        assert_eq!(later.since(earlier), Counts::default());
    }
}
