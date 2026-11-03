//! Overhead benchmark: plain hyper versus Ferrum Alloy configurations,
//! across workloads and transports.
//!
//! ```text
//! alloy-bench run --scenario alloy --workload small --transport h1 [options]
//! alloy-bench matrix --reps 5 --out results.jsonl [options]
//! alloy-bench compare [--budget budget.json] results.jsonl
//! ```
//!
//! Server and client run in one process on separate multi-threaded Tokio
//! runtimes over loopback, and `matrix` runs every cell in its own process.
//! Each run prints one JSON line (schema [`run::SCHEMA`]) with throughput,
//! latency percentiles, CPU time and allocations per thread role, peak RSS,
//! OpenTelemetry export counters, and the environment. Results are same-host
//! measurements with the noise that implies; see `docs/benchmarks.md` for how
//! to run and read them. `compare` reports each cell's ratios to `plain`
//! from result lines and can check them against per-cell budgets.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "benchmark driver"
)]

mod alloc;
mod client;
mod compare;
mod dims;
mod matrix;
mod pki;
mod probe;
mod run;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::client::Load;
use crate::dims::{Cell, Dimension, Scenario, Transport, Workload, parse_list};
use crate::run::{RunOptions, seconds};

type Failure = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "\
usage:
  alloy-bench run [--scenario S] [--workload W] [--transport T] [--rep N] [options]
  alloy-bench matrix [--scenarios L] [--workloads L] [--transports L] [--reps N]
                     [--out FILE] [--dry-run] [options]
  alloy-bench compare [--budget FILE] RESULTS...

run measures one cell (default: alloy, small, h1) and prints one JSON line.
matrix runs every combination of the lists (default: all), --reps times
(default 5), in a seeded shuffle per repetition, each in its own process, and
appends the JSON lines to --out (default: stdout). L is `all` or a
comma-separated list.
compare reads result files and prints one JSON line per cell with the median
and range of its throughput and latency ratios to plain in the same run_id,
repetition, workload, transport and allocation mode; lines with errors are
skipped. --budget checks those medians against per-cell limits (a JSON file,
see docs/benchmarks.md) and fails when one is exceeded or has no data.

options:
  --seconds N               measurement window (default 5)
  --warmup N                warm-up before the window (default 1)
  --concurrency N           requests in flight (default 32)
  --streams N               streams per HTTP/2 connection (default 8)
  --alloc-counting          count allocations; slows every run, so compare
                            only with other counting runs
  --label TEXT              recorded as environment.label (host, commit, ...)
  --run-id ID               recorded as run_id (default: random); every run
                            of one matrix records the same id
  --collector-endpoint URL  otel-collector exports to this OTLP/HTTP traces
                            URL instead of the in-process stub
  --inject-service-ns N     busy-wait N ns per request in every scenario but
                            plain: a synthetic regression for checking the
                            regression gate, never a measurement (default 0)

scenarios:  plain, alloy, alloy-logs, alloy-logs-fmt, alloy-diagnostics,
            otel-sampled, otel-unsampled, otel-unreachable, otel-collector
workloads:  small, large, stream, cancel
transports: h1, h2c, h1-tls, h2-tls, h1-mtls, h2-mtls
";

/// Flags that `matrix` forwards to every `run`.
const FORWARDED: &[&str] = &[
    "--seconds",
    "--warmup",
    "--concurrency",
    "--streams",
    "--label",
    "--collector-endpoint",
    "--inject-service-ns",
];

#[derive(Debug)]
enum Command {
    Help,
    Run {
        cell: Cell,
        options: RunOptions,
    },
    Matrix {
        cells: Vec<Cell>,
        reps: u32,
        options: RunOptions,
        forward: Vec<String>,
        out: Option<PathBuf>,
        dry_run: bool,
    },
    Compare {
        files: Vec<PathBuf>,
        budget: Option<PathBuf>,
    },
}

fn parse_compare(args: &[String]) -> Result<Command, Failure> {
    let mut files = Vec::new();
    let mut budget = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--budget" => {
                let value = iter.next().ok_or("--budget needs a value")?;
                if budget.replace(PathBuf::from(value)).is_some() {
                    return Err("--budget given twice".into());
                }
            }
            flag if flag.starts_with('-') => {
                let message = format!("unknown flag {flag} for compare\n\n{USAGE}");
                return Err(message.into());
            }
            file => files.push(PathBuf::from(file)),
        }
    }
    if files.is_empty() {
        return Err(format!("compare needs at least one result file\n\n{USAGE}").into());
    }
    Ok(Command::Compare { files, budget })
}

fn parse(args: &[String]) -> Result<Command, Failure> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    let matrix = match command.as_str() {
        "run" => false,
        "matrix" => true,
        "compare" => return parse_compare(rest),
        "help" | "-h" | "--help" => return Ok(Command::Help),
        other => return Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    };
    let mut options = RunOptions {
        load: Load {
            concurrency: 32,
            streams: 8,
            warmup: Duration::from_secs(1),
            duration: Duration::from_secs(5),
        },
        alloc_counting: false,
        label: None,
        run_id: String::new(),
        collector_endpoint: None,
        inject_service_ns: 0,
        rep: None,
    };
    let mut cell = Cell {
        scenario: Scenario::Alloy,
        workload: Workload::Small,
        transport: Transport::H1,
    };
    let mut scenarios = Scenario::ALL.to_vec();
    let mut workloads = Workload::ALL.to_vec();
    let mut transports = Transport::ALL.to_vec();
    let mut reps = 5;
    let mut out = None;
    let mut dry_run = false;
    let mut forward = Vec::new();

    let mut iter = rest.iter();
    while let Some(flag) = iter.next() {
        let flag = flag.as_str();
        match (matrix, flag) {
            (_, "--alloc-counting") => {
                options.alloc_counting = true;
                forward.push(flag.to_owned());
                continue;
            }
            (true, "--dry-run") => {
                dry_run = true;
                continue;
            }
            _ => {}
        }
        let value = iter.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match (matrix, flag) {
            (_, "--seconds") => options.load.duration = seconds(value)?,
            (_, "--warmup") => options.load.warmup = seconds(value)?,
            (_, "--concurrency") => options.load.concurrency = value.parse()?,
            (_, "--streams") => options.load.streams = value.parse()?,
            (_, "--label") => options.label = Some(value.clone()),
            (_, "--run-id") => options.run_id = value.clone(),
            (_, "--collector-endpoint") => options.collector_endpoint = Some(value.clone()),
            (_, "--inject-service-ns") => options.inject_service_ns = value.parse()?,
            (false, "--scenario") => cell.scenario = Scenario::parse(value)?,
            (false, "--workload") => cell.workload = Workload::parse(value)?,
            (false, "--transport") => cell.transport = Transport::parse(value)?,
            (false, "--rep") => options.rep = Some(value.parse()?),
            (true, "--scenarios") => scenarios = parse_list(value)?,
            (true, "--workloads") => workloads = parse_list(value)?,
            (true, "--transports") => transports = parse_list(value)?,
            (true, "--reps") => reps = value.parse()?,
            (true, "--out") => out = Some(PathBuf::from(value)),
            _ => {
                let message = format!("unknown flag {flag} for {command}\n\n{USAGE}");
                return Err(message.into());
            }
        }
        if FORWARDED.contains(&flag) {
            forward.extend([flag.to_owned(), value.clone()]);
        }
    }

    if options.run_id.is_empty() {
        options.run_id = run::new_run_id()?;
    }
    if !matrix {
        options.load.validate(cell.transport)?;
        return Ok(Command::Run { cell, options });
    }
    if reps == 0 {
        return Err("--reps must be at least 1".into());
    }
    // Reject an invalid load before hours of runs start, not in each of them.
    for transport in &transports {
        options.load.validate(*transport)?;
    }
    Ok(Command::Matrix {
        cells: matrix::plan(&scenarios, &workloads, &transports),
        reps,
        options,
        forward,
        out,
        dry_run,
    })
}

fn execute(command: Command) -> Result<(), Failure> {
    match command {
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Command::Run { cell, options } => {
            let result = run::run_cell(cell, &options)?;
            println!("{result}");
            Ok(())
        }
        Command::Matrix {
            cells,
            reps,
            options,
            forward,
            out,
            dry_run,
        } => {
            if dry_run {
                let per_run = options.load.warmup + options.load.duration;
                matrix::dry_run(&cells, reps, per_run);
                return Ok(());
            }
            matrix::execute(&cells, reps, &options.run_id, &forward, out.as_deref())
        }
        Command::Compare { files, budget } => compare::execute(&files, budget.as_deref()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args).and_then(execute) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("alloy-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn run_defaults_and_overrides() {
        let Command::Run { cell, options } = parse(&args("run")).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(cell.scenario, Scenario::Alloy);
        assert_eq!(options.load.concurrency, 32);
        let line = "run --scenario otel-collector --workload cancel --transport h2-mtls \
                    --seconds 0.5 --concurrency 16 --streams 4 --rep 2 --alloc-counting";
        let Command::Run { cell, options } = parse(&args(line)).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(cell.scenario, Scenario::OtelCollector);
        assert_eq!(cell.workload, Workload::Cancel);
        assert_eq!(cell.transport, Transport::H2Mtls);
        assert_eq!(options.load.duration, Duration::from_millis(500));
        assert_eq!(options.load.concurrency, 16);
        assert_eq!(options.rep, Some(2));
        assert!(options.alloc_counting);
        assert_eq!(options.run_id.len(), 36);
        let Command::Run { options, .. } = parse(&args("run --run-id r1")).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.run_id, "r1");
    }

    #[test]
    fn matrix_forwards_common_flags_only() {
        let line = "matrix --scenarios plain,alloy --transports h1,h2c --reps 3 \
                    --seconds 2 --label ci --alloc-counting --out r.jsonl --dry-run \
                    --inject-service-ns 5000";
        let Command::Matrix {
            cells,
            reps,
            forward,
            out,
            dry_run,
            ..
        } = parse(&args(line)).unwrap()
        else {
            panic!("expected matrix");
        };
        assert_eq!(cells.len(), 2 * Workload::ALL.len() * 2);
        assert_eq!(reps, 3);
        assert_eq!(
            forward.join(" "),
            "--seconds 2 --label ci --alloc-counting --inject-service-ns 5000"
        );
        assert_eq!(out, Some(PathBuf::from("r.jsonl")));
        assert!(dry_run);
    }

    #[test]
    fn rejects_bad_input() {
        for line in [
            "bench",
            "run --scenarios plain",
            "run --reps 2",
            "matrix --scenario plain",
            "matrix --reps 0",
            "run --seconds",
            "run --transport h3",
            "matrix --concurrency 30 --streams 8",
            "run --transport h2c --concurrency 30 --streams 8",
            "compare",
            "compare --budget b.json",
            "compare r.jsonl --budget",
            "compare r.jsonl --budget a.json --budget b.json",
            "compare r.jsonl --reps 3",
        ] {
            assert!(parse(&args(line)).is_err(), "{line}");
        }
        let uneven_http1 = args("run --transport h1 --concurrency 30 --streams 8");
        assert!(parse(&uneven_http1).is_ok());
    }

    #[test]
    fn compare_takes_result_files_and_an_optional_budget() {
        let line = "compare a.jsonl --budget budget.json b.jsonl";
        let Command::Compare { files, budget } = parse(&args(line)).unwrap() else {
            panic!("expected compare");
        };
        let expected = [PathBuf::from("a.jsonl"), PathBuf::from("b.jsonl")];
        assert_eq!(files, expected);
        assert_eq!(budget, Some(PathBuf::from("budget.json")));
        let Command::Compare { files, budget } = parse(&args("compare r.jsonl")).unwrap() else {
            panic!("expected compare");
        };
        assert_eq!(files, [PathBuf::from("r.jsonl")]);
        assert_eq!(budget, None);
        assert!(USAGE.contains("alloy-bench compare [--budget FILE]"));
    }

    #[test]
    fn usage_names_every_dimension_value() {
        let names = Scenario::ALL
            .iter()
            .map(|v| v.name())
            .chain(Workload::ALL.iter().map(|v| v.name()))
            .chain(Transport::ALL.iter().map(|v| v.name()));
        for name in names {
            assert!(USAGE.contains(name), "{name}");
        }
    }
}
