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

/// The order repetition `rep` (from 1) runs the cells in: every cell once,
/// rotated by one position per repetition so that no cell always runs first
/// or last.
pub(crate) fn order(cells: &[Cell], rep: u32) -> Vec<Cell> {
    let mut ordered = cells.to_vec();
    if !ordered.is_empty() {
        let shift = rep.saturating_sub(1) as usize % ordered.len();
        ordered.rotate_left(shift);
    }
    ordered
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
    for rep in 1..=reps {
        for cell in order(cells, rep) {
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
}

/// Runs every repetition of every cell as `alloy-bench run` subprocesses,
/// appending each result line to `out` (or stdout). Failed runs are
/// reported and counted; the command fails if any run failed.
pub(crate) fn execute(
    cells: &[Cell],
    reps: u32,
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
    for rep in 1..=reps {
        for cell in order(cells, rep) {
            done += 1;
            eprintln!(
                "[{done}/{total}] rep {rep}: {} {} {}",
                cell.scenario.name(),
                cell.workload.name(),
                cell.transport.name()
            );
            let output = Command::new(&exe)
                .args(cell_args(cell, rep))
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

    #[test]
    fn repetitions_rotate_and_keep_every_cell() {
        let scenarios = [Scenario::Plain, Scenario::Alloy];
        let transports = [Transport::H1, Transport::H2c];
        let cells = plan(&scenarios, &[Workload::Small], &transports);
        assert_eq!(order(&cells, 1), cells);
        let second = order(&cells, 2);
        assert_eq!(second[0], cells[1]);
        assert_eq!(second.len(), cells.len());
        for cell in &cells {
            assert!(second.contains(cell));
        }
        assert_eq!(order(&cells, 5), order(&cells, 1));
        assert!(order(&[], 3).is_empty());
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
