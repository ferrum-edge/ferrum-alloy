//! Resource and environment probes. They read Linux `/proc` and `/sys`;
//! elsewhere they return `None`, which results report as `null` (unknown,
//! not zero).

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::alloc::Role;

/// Thread-name prefixes that mark client and collector threads. Linux keeps
/// the first 15 bytes of a thread name, so these stay short.
pub(crate) const CLIENT_THREAD: &str = "bench-client";
pub(crate) const COLLECTOR_THREAD: &str = "bench-collector";
pub(crate) const SERVER_THREAD: &str = "bench-server";

/// The role a thread is attributed to, from its name.
pub(crate) fn role_of_thread(name: &str) -> Role {
    if name.starts_with(CLIENT_THREAD) {
        Role::Client
    } else if name.starts_with(COLLECTOR_THREAD) {
        Role::Collector
    } else {
        Role::Service
    }
}

/// On-CPU nanoseconds of every live thread, keyed by thread id.
#[derive(Debug, Clone, Default)]
pub(crate) struct CpuSnapshot {
    threads: HashMap<String, (Role, u64)>,
}

impl CpuSnapshot {
    /// Reads `/proc/self/task/*/{comm,schedstat}`. `None` when unavailable.
    pub(crate) fn take() -> Option<Self> {
        let mut threads = HashMap::new();
        for entry in std::fs::read_dir("/proc/self/task").ok()? {
            let Ok(entry) = entry else { continue };
            let dir = entry.path();
            // A thread can exit between listing and reading; skip it.
            let Ok(comm) = std::fs::read_to_string(dir.join("comm")) else {
                continue;
            };
            let schedstat = read(&dir.join("schedstat"));
            let Some(ns) = schedstat.as_deref().and_then(parse_schedstat) else {
                continue;
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            threads.insert(id, (role_of_thread(comm.trim()), ns));
        }
        (!threads.is_empty()).then_some(Self { threads })
    }

    /// CPU nanoseconds per role between `earlier` and `self`, indexed like
    /// [`Role::ALL`]. Threads that exited in between are missing, so this is a
    /// lower bound; threads that started in between count in full.
    pub(crate) fn since(&self, earlier: &Self) -> [u64; Role::ALL.len()] {
        let mut by_role = [0_u64; Role::ALL.len()];
        for (id, (role, ns)) in &self.threads {
            let before = earlier.threads.get(id).map_or(0, |(_, ns)| *ns);
            if let Some(total) = by_role.get_mut(*role as usize) {
                *total += ns.saturating_sub(before);
            }
        }
        by_role
    }
}

/// The first field of `/proc/<pid>/task/<tid>/schedstat`: time spent on the
/// CPU, in nanoseconds.
pub(crate) fn parse_schedstat(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse().ok()
}

/// A `kB` value from `/proc/self/status`, in bytes.
pub(crate) fn parse_status_bytes(status: &str, key: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix(':')?;
        let kib: u64 = value.trim().strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    })
}

/// Process peak and current resident set size, in bytes.
pub(crate) fn memory() -> Option<Value> {
    let status = read(Path::new("/proc/self/status"))?;
    Some(json!({
        "scope": "process",
        "peak_rss_bytes": parse_status_bytes(&status, "VmHWM"),
        "rss_bytes": parse_status_bytes(&status, "VmRSS"),
    }))
}

/// The 1-minute load average.
pub(crate) fn load1() -> Option<f64> {
    read(Path::new("/proc/loadavg"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// What the result was measured on. Every field that cannot be read is
/// `null`.
pub(crate) fn environment(label: Option<&str>) -> Value {
    let cpu_model = read(Path::new("/proc/cpuinfo")).and_then(|info| model_name(&info));
    let governor = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor";
    json!({
        "label": label,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpus": std::thread::available_parallelism().ok().map(usize::from),
        "cpu_model": cpu_model,
        "cpu_governor": read(Path::new(governor)).map(|s| s.trim().to_owned()),
        "kernel": read(Path::new("/proc/sys/kernel/osrelease")).map(|s| s.trim().to_owned()),
        "load1_start": load1(),
        "debug_build": cfg!(debug_assertions),
        "bench_version": env!("CARGO_PKG_VERSION"),
        "started_unix_seconds": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs()),
    })
}

/// The first `model name` in `/proc/cpuinfo` (absent on some ARM hosts).
fn model_name(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "model name").then(|| value.trim().to_owned())
    })
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn parses_proc_formats() {
        assert_eq!(parse_schedstat("123456 789 10\n"), Some(123_456));
        assert_eq!(parse_schedstat(""), None);
        let status = "Name:\talloy-bench\nVmHWM:\t   2048 kB\nVmRSS:\t   1024 kB\n";
        assert_eq!(parse_status_bytes(status, "VmHWM"), Some(2048 * 1024));
        assert_eq!(parse_status_bytes(status, "VmRSS"), Some(1024 * 1024));
        assert_eq!(parse_status_bytes(status, "VmSwap"), None);
        let cpuinfo = "processor\t: 0\nmodel name\t: Example CPU @ 3.00GHz\n";
        let model = model_name(cpuinfo);
        assert_eq!(model.as_deref(), Some("Example CPU @ 3.00GHz"));
        assert_eq!(model_name("processor\t: 0\n"), None);
    }

    #[test]
    fn threads_are_attributed_by_name() {
        assert_eq!(role_of_thread("bench-client"), Role::Client);
        assert_eq!(role_of_thread("bench-collector"), Role::Collector);
        assert_eq!(role_of_thread("bench-server"), Role::Service);
        assert_eq!(role_of_thread("ferrum-alloy-ot"), Role::Service);
    }

    #[test]
    fn cpu_deltas_are_per_role_and_never_negative() {
        let snapshot = |threads: &[(&str, Role, u64)]| CpuSnapshot {
            threads: threads
                .iter()
                .map(|(id, role, ns)| ((*id).to_owned(), (*role, *ns)))
                .collect(),
        };
        let earlier = snapshot(&[("1", Role::Service, 100), ("2", Role::Client, 50)]);
        let later = snapshot(&[
            ("1", Role::Service, 250),
            ("2", Role::Client, 40),
            ("3", Role::Collector, 7),
        ]);
        assert_eq!(later.since(&earlier), [150, 0, 7]);
    }

    #[test]
    fn environment_always_has_its_keys() {
        let environment = environment(Some("ci"));
        for key in ["label", "os", "arch", "cpus", "load1_start", "debug_build"] {
            assert!(environment.get(key).is_some(), "{key}");
        }
        assert_eq!(environment["label"], "ci");
    }
}
