//! The `matrix` command: every requested cell, repeated and interleaved,
//! each in a fresh process (a process installs one global subscriber, and a
//! fresh heap keeps runs independent).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::Failure;
use crate::dims::{Cell, Dimension, Scenario, Transport, Workload};
use crate::run::SCHEMA;

/// Every combination of the requested dimensions, in matrix order.
pub(crate) fn plan(
    scenarios: &[Scenario],
    workloads: &[Workload],
    transports: &[Transport],
) -> Vec<Cell> {
    let mut cells = Vec::new();
    for &transport in transports {
        for &workload in workloads {
            for &scenario in scenarios {
                cells.push(Cell {
                    scenario,
                    workload,
                    transport,
                });
            }
        }
    }
    cells
}

/// How [`orders`] orders each repetition, as `--dry-run` reports it.
pub(crate) const ORDERING: &str = "splitmix64 Fisher-Yates shuffle per repetition, \
                                   fewest repeated neighbors of 64";

/// The fixed seed of every repetition's shuffles, so that the same cells and
/// repetition count always run in the same order.
pub(crate) const ORDER_SEED: u64 = 0xA110_7BE4_C4ED_0197;

/// Shuffles tried per repetition (see [`orders`]).
const ORDER_ATTEMPTS: u32 = 64;

/// SplitMix64 (Steele, Lea and Flood, 2014): a small seedable generator,
/// enough to shuffle the matrix reproducibly. Not cryptographic.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform value in `0..bound`, rejecting the values that would bias
    /// the modulo. `bound` is at least 1.
    fn below(&mut self, bound: u64) -> u64 {
        let bound = bound.max(1);
        let zone = u64::MAX - u64::MAX % bound;
        loop {
            let value = self.next_u64();
            if value < zone {
                return value % bound;
            }
        }
    }
}

/// A Fisher-Yates shuffle of `0..len`, seeded by repetition and attempt.
fn shuffle(len: usize, rep: u32, attempt: u32) -> Vec<usize> {
    let mut random = SplitMix64(ORDER_SEED ^ (u64::from(rep) << 32) ^ u64::from(attempt));
    let mut permutation: Vec<usize> = (0..len).collect();
    for index in (1..len).rev() {
        let other = random.below(index as u64 + 1) as usize;
        permutation.swap(index, other);
    }
    permutation
}

/// The order of every repetition, from the first: each runs every cell
/// exactly once, in a seeded shuffle. A repetition tries up to 64 shuffles
/// and keeps the first that repeats the fewest ordered pairs of neighbors
/// (one cell running right after another) from the previous repetition,
/// then from all earlier ones, stopping at the first that repeats none. So
/// a slow cell does not keep slowing the same next cell, and no cell keeps
/// its position. The result depends only on the cells and `reps`.
pub(crate) fn orders(cells: &[Cell], reps: u32) -> Vec<Vec<Cell>> {
    let len = cells.len();
    // `used[a * len + b]` counts earlier repetitions that ran `b` right
    // after `a`; `successor[a]` is what the previous repetition ran after `a`.
    let mut used = vec![0_u64; len * len];
    let mut successor: Vec<Option<usize>> = vec![None; len];
    let mut orders: Vec<Vec<Cell>> = Vec::with_capacity(reps as usize);
    for rep in 1..=reps {
        let mut best: Option<((usize, u64), Vec<usize>)> = None;
        for attempt in 0..ORDER_ATTEMPTS {
            let candidate = shuffle(len, rep, attempt);
            let mut score = (0_usize, 0_u64);
            for pair in candidate.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if successor[a] == Some(b) {
                    score.0 += 1;
                }
                score.1 += used[a * len + b];
            }
            if best.as_ref().is_none_or(|(fewest, _)| score < *fewest) {
                best = Some((score, candidate));
            }
            if score == (0, 0) {
                break;
            }
        }
        let chosen = best.map_or_else(|| (0..len).collect(), |(_, candidate)| candidate);
        successor.fill(None);
        for pair in chosen.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            successor[a] = Some(b);
            used[a * len + b] += 1;
        }
        orders.push(chosen.iter().map(|&index| cells[index]).collect());
    }
    orders
}

/// Arguments that select one cell of one repetition.
fn cell_args(cell: Cell, rep: u32) -> Vec<String> {
    vec![
        "run".into(),
        "--scenario".into(),
        cell.scenario.name().into(),
        "--workload".into(),
        cell.workload.name().into(),
        "--transport".into(),
        cell.transport.name().into(),
        "--rep".into(),
        rep.to_string(),
    ]
}

/// Prints the plan without running it.
pub(crate) fn dry_run(cells: &[Cell], reps: u32, per_run: Duration) {
    for (rep, ordered) in (1..=reps).zip(orders(cells, reps)) {
        for cell in ordered {
            println!(
                "{rep}\t{}\t{}\t{}",
                cell.scenario.name(),
                cell.workload.name(),
                cell.transport.name()
            );
        }
    }
    let runs = cells.len() * reps as usize;
    let minutes = per_run.as_secs_f64() * runs as f64 / 60.0;
    eprintln!("{runs} runs, at least {minutes:.0} minutes of measurement and warm-up");
    eprintln!("order: {ORDERING}, seed {ORDER_SEED:#018x}");
}

/// Runs every repetition of every cell as `alloy-bench run` subprocesses,
/// all recording `run_id`, appending each result line to `out` (or stdout).
/// Failed runs are reported and counted; the command fails if any run failed.
pub(crate) fn execute(
    cells: &[Cell],
    reps: u32,
    run_id: &str,
    forward: &[String],
    out: Option<&Path>,
) -> Result<(), Failure> {
    let exe = std::env::current_exe()?;
    let mut sink: Box<dyn Write> = match out {
        Some(path) => Box::new(OpenOptions::new().create(true).append(true).open(path)?),
        None => Box::new(std::io::stdout()),
    };
    let total = cells.len() * reps as usize;
    let mut done = 0;
    let mut failures = 0;
    eprintln!("order: {ORDERING}, seed {ORDER_SEED:#018x}");
    for (rep, ordered) in (1..=reps).zip(orders(cells, reps)) {
        for cell in ordered {
            done += 1;
            eprintln!(
                "[{done}/{total}] rep {rep}: {} {} {}",
                cell.scenario.name(),
                cell.workload.name(),
                cell.transport.name()
            );
            let output = Command::new(&exe)
                .args(cell_args(cell, rep))
                .args(["--run-id", run_id])
                .args(forward)
                .stdin(Stdio::null())
                .stderr(Stdio::inherit())
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(line).ok();
            let valid = parsed.is_some_and(|value| value["schema"] == SCHEMA);
            if output.status.success() && valid {
                writeln!(sink, "{line}")?;
                sink.flush()?;
            } else {
                failures += 1;
                eprintln!("  failed ({})", output.status);
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} of {total} runs failed").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn plan_is_the_cartesian_product() {
        let cells = plan(Scenario::ALL, Workload::ALL, Transport::ALL);
        assert_eq!(
            cells.len(),
            Scenario::ALL.len() * Workload::ALL.len() * Transport::ALL.len()
        );
        for (index, cell) in cells.iter().enumerate() {
            assert!(!cells[..index].contains(cell), "{cell:?} planned twice");
        }
    }

    /// Three `small` / `h1` cells: `plain`, `alloy`, `alloy-logs`.
    fn three() -> Vec<Cell> {
        let scenarios = [Scenario::Plain, Scenario::Alloy, Scenario::AlloyLogs];
        plan(&scenarios, &[Workload::Small], &[Transport::H1])
    }

    /// The ordered pairs of neighbors (one cell right after another).
    fn neighbors(order: &[Cell]) -> Vec<(Cell, Cell)> {
        order.windows(2).map(|pair| (pair[0], pair[1])).collect()
    }

    #[test]
    fn splitmix64_matches_its_reference_output() {
        let mut random = SplitMix64(0);
        assert_eq!(random.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(random.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(random.next_u64(), 0x06C4_5D18_8009_454F);
        for bound in 1..=10 {
            assert!(random.below(bound) < bound);
        }
    }

    #[test]
    fn every_repetition_runs_every_cell_once() {
        let cells = plan(Scenario::ALL, Workload::ALL, Transport::ALL);
        let ordered = orders(&cells, 5);
        assert_eq!(ordered.len(), 5);
        for order in &ordered {
            assert_eq!(order.len(), cells.len());
            for cell in &cells {
                let runs = order.iter().filter(|other| *other == cell).count();
                assert_eq!(runs, 1, "{cell:?}");
            }
        }
        assert!(orders(&cells, 0).is_empty());
        let empty = orders(&[], 3);
        assert_eq!(empty.len(), 3);
        assert!(empty.iter().all(Vec::is_empty));
        let one = &cells[..1];
        assert!(orders(one, 3).iter().all(|order| order == one));
    }

    #[test]
    fn orders_are_reproducible() {
        let cells = plan(Scenario::ALL, Workload::ALL, Transport::ALL);
        let five = orders(&cells, 5);
        assert_eq!(orders(&cells, 5), five);
        assert_eq!(orders(&cells, 3)[..], five[..3]);
        // Pinned, so that `--dry-run` output stays the same across builds
        // and a change to the shuffle shows up here.
        let three = three();
        let indices: Vec<Vec<usize>> = orders(&three, 5)
            .iter()
            .map(|order| {
                let index = |cell: &Cell| three.iter().position(|c| c == cell).unwrap();
                order.iter().map(index).collect()
            })
            .collect();
        let expected: [[usize; 3]; 5] = [[0, 2, 1], [1, 2, 0], [2, 1, 0], [2, 0, 1], [1, 0, 2]];
        assert_eq!(indices, expected);
    }

    #[test]
    fn neighbors_and_positions_vary_between_repetitions() {
        let full = plan(Scenario::ALL, Workload::ALL, Transport::ALL);
        for cells in [full.clone(), three()] {
            let ordered = orders(&cells, 5);
            for (index, order) in ordered.iter().enumerate() {
                for earlier in &ordered[..index] {
                    assert_ne!(order, earlier, "repetition {} repeats", index + 1);
                }
            }
            for pair in ordered.windows(2) {
                let previous = neighbors(&pair[0]);
                let next = neighbors(&pair[1]);
                let repeated = next.iter().filter(|n| previous.contains(*n)).count();
                assert_eq!(repeated, 0, "{} cells", cells.len());
            }
            let first = ordered[0][0];
            assert!(ordered.iter().any(|order| order[0] != first));
        }
        // Across the full matrix, no cell follows the same cell twice in
        // five repetitions.
        let mut seen: Vec<(Cell, Cell)> = Vec::new();
        for order in orders(&full, 5) {
            for neighbor in neighbors(&order) {
                assert!(!seen.contains(&neighbor), "{neighbor:?} repeated");
                seen.push(neighbor);
            }
        }
        // With two cells, alternating is the only way to change neighbors.
        let three = three();
        let two = &three[..2];
        let ordered = orders(two, 4);
        for pair in ordered.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
    }

    #[test]
    fn cell_arguments_name_the_cell_and_repetition() {
        let cell = Cell {
            scenario: Scenario::OtelCollector,
            workload: Workload::Cancel,
            transport: Transport::H2Mtls,
        };
        let args = cell_args(cell, 3);
        assert_eq!(
            args.join(" "),
            "run --scenario otel-collector --workload cancel --transport h2-mtls --rep 3"
        );
    }
}
