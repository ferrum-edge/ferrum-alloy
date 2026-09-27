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
    fn counts_allocations_by_role_once_enabled() {
        enable();
        assert!(enabled());
        let delta = std::thread::spawn(|| {
            set_role(Role::Collector);
            let before = snapshot();
            let data = std::hint::black_box(vec![0_u8; 4096]);
            let after = snapshot();
            drop(data);
            after[Role::Collector as usize].since(before[Role::Collector as usize])
        })
        .join()
        .unwrap();
        assert!(delta.calls >= 1, "{delta:?}");
        assert!(delta.bytes >= 4096, "{delta:?}");
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
