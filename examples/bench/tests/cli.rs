//! The `alloy-bench` binary end to end: every scenario runs in its own
//! process, as `matrix` runs it, and prints one valid result line.

#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use std::process::{Command, Output};

use serde_json::Value;

/// Keys every result line carries.
const KEYS: &[&str] = &[
    "schema",
    "run_id",
    "commit",
    "rep",
    "scenario",
    "workload",
    "transport",
    "alloc_counting",
    "requests",
    "errors",
    "error_samples",
    "connects",
    "requests_per_second",
    "latency_us",
    "cpu",
    "memory",
    "allocations",
    "otel",
    "environment",
];

fn bench(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_alloy-bench"));
    command.args(args);
    command
}

fn describe(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{}\nstdout:\n{stdout}\nstderr:\n{stderr}", output.status)
}

/// The non-empty stdout lines of a successful command, parsed as JSON.
fn result_lines(output: &Output) -> Vec<Value> {
    assert!(output.status.success(), "{}", describe(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("{error}: {line:?}\n{}", describe(output));
            })
        })
        .collect()
}

fn assert_valid(result: &Value) {
    for key in KEYS {
        assert!(result.get(key).is_some(), "missing {key}: {result}");
    }
    assert_eq!(result["schema"], "alloy-bench/1", "{result}");
    assert!(result["requests"].as_u64().unwrap() > 0, "{result}");
    assert_eq!(result["errors"], 0, "{result}");
    assert_eq!(result["error_samples"], serde_json::json!([]), "{result}");
    assert_eq!(result["alloc_counting"], false, "{result}");
    assert_eq!(result["run_id"].as_str().unwrap().len(), 36, "{result}");
}

/// The scenario names the usage text lists, so that a new scenario is
/// covered as soon as it is documented (a unit test checks it is).
fn scenarios() -> Vec<String> {
    let output = bench(&["help"]).output().unwrap();
    assert!(output.status.success(), "{}", describe(&output));
    let usage = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, rest) = usage.split_once("scenarios:").unwrap();
    let (list, _) = rest.split_once("workloads:").unwrap();
    list.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_scenario_runs_in_its_own_process() {
    let scenarios = scenarios();
    assert!(scenarios.len() >= 7, "{scenarios:?}");
    assert!(
        scenarios.iter().any(|name| name == "plain"),
        "{scenarios:?}"
    );
    for scenario in &scenarios {
        let output = bench(&[
            "run",
            "--scenario",
            scenario.as_str(),
            "--workload",
            "small",
            "--transport",
            "h1",
            "--seconds",
            "0.2",
            "--warmup",
            "0.1",
        ])
        .env_remove("GITHUB_SHA")
        .output()
        .unwrap();
        let lines = result_lines(&output);
        assert_eq!(lines.len(), 1, "{scenario}: {}", describe(&output));
        let result = &lines[0];
        assert_valid(result);
        assert_eq!(result["scenario"], scenario.as_str(), "{result}");
        assert_eq!(result["workload"], "small", "{result}");
        assert_eq!(result["transport"], "h1", "{result}");
        assert_eq!(result["rep"], Value::Null, "{result}");
        assert_eq!(result["commit"], Value::Null, "{result}");
    }
}

#[test]
fn matrix_writes_one_result_line_per_run() {
    let output = bench(&[
        "matrix",
        "--reps",
        "1",
        "--scenarios",
        "plain",
        "--workloads",
        "small",
        "--transports",
        "h1",
        "--seconds",
        "0.2",
        "--warmup",
        "0",
        "--run-id",
        "matrix-test",
    ])
    .env("GITHUB_SHA", "0123456789abcdef")
    .output()
    .unwrap();
    let lines = result_lines(&output);
    assert_eq!(lines.len(), 1, "{}", describe(&output));
    let result = &lines[0];
    for key in KEYS {
        assert!(result.get(key).is_some(), "missing {key}: {result}");
    }
    assert_eq!(result["schema"], "alloy-bench/1", "{result}");
    assert!(result["requests"].as_u64().unwrap() > 0, "{result}");
    assert_eq!(result["errors"], 0, "{result}");
    assert_eq!(result["scenario"], "plain", "{result}");
    assert_eq!(result["rep"], 1, "{result}");
    assert_eq!(result["run_id"], "matrix-test", "{result}");
    assert_eq!(result["commit"], "0123456789abcdef", "{result}");
}

/// The cells of each repetition of a `--dry-run` plan, in order.
fn planned(output: &Output) -> Vec<Vec<String>> {
    assert!(output.status.success(), "{}", describe(output));
    let mut reps: Vec<Vec<String>> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let (rep, cell) = line.split_once('\t').unwrap();
        let rep: usize = rep.parse().unwrap();
        if reps.len() < rep {
            reps.resize(rep, Vec::new());
        }
        reps[rep - 1].push(cell.to_owned());
    }
    reps
}

#[test]
fn matrix_dry_run_shuffles_reproducibly() {
    let args = [
        "matrix",
        "--dry-run",
        "--reps",
        "3",
        "--scenarios",
        "plain,alloy,alloy-logs",
        "--workloads",
        "small,large",
        "--transports",
        "h1",
    ];
    let first = bench(&args).output().unwrap();
    let second = bench(&args).output().unwrap();
    let reps = planned(&first);
    assert_eq!(reps, planned(&second));
    assert_eq!(reps.len(), 3, "{}", describe(&first));
    for cells in &reps {
        let mut unique = cells.clone();
        unique.sort();
        unique.dedup();
        assert_eq!((cells.len(), unique.len()), (6, 6), "{cells:?}");
    }
    assert_ne!(reps[0], reps[1]);
    assert_ne!(reps[1], reps[2]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(stderr.contains("order: splitmix64"), "{stderr}");
}

/// A result line with the fields `compare` reads.
fn result_line(rep: u64, scenario: &str, rate: f64) -> String {
    serde_json::json!({
        "schema": "alloy-bench/1",
        "run_id": "compare-test",
        "commit": null,
        "rep": rep,
        "scenario": scenario,
        "workload": "small",
        "transport": "h1",
        "alloc_counting": false,
        "errors": 0,
        "requests_per_second": rate,
        "latency_us": { "p50": 100, "p90": 150, "p99": 200, "p999": 300, "max": 400 },
    })
    .to_string()
}

#[test]
fn compare_reports_ratios_and_enforces_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let results = dir.path().join("results.jsonl");
    let lines: Vec<String> = (1..=3)
        .flat_map(|rep| {
            [
                result_line(rep, "plain", 100.0),
                result_line(rep, "alloy", 75.0),
            ]
        })
        .collect();
    std::fs::write(&results, lines.join("\n") + "\n").unwrap();
    let results = results.to_str().unwrap();

    let output = bench(&["compare", results]).output().unwrap();
    let compared = result_lines(&output);
    assert_eq!(compared.len(), 1, "{}", describe(&output));
    let alloy = &compared[0];
    assert_eq!(alloy["schema"], "alloy-bench-compare/1", "{alloy}");
    assert_eq!(alloy["scenario"], "alloy", "{alloy}");
    assert_eq!(alloy["pairs"], 3, "{alloy}");
    assert_eq!(alloy["throughput_ratio"]["median"], 0.75, "{alloy}");
    assert_eq!(alloy["latency_p99_ratio"]["median"], 1.0, "{alloy}");
    assert_eq!(alloy["budget"], Value::Null, "{alloy}");

    let budget = dir.path().join("budget.json");
    let write_budget = |min: f64| {
        let file = serde_json::json!({
            "schema": "alloy-bench-budget/1",
            "budgets": [{
                "scenario": "alloy",
                "workload": "small",
                "transport": "h1",
                "min_throughput_ratio": min,
            }],
        });
        std::fs::write(&budget, file.to_string()).unwrap();
    };
    let budget_arg = budget.to_str().unwrap();
    write_budget(0.7);
    let output = bench(&["compare", "--budget", budget_arg, results])
        .output()
        .unwrap();
    let compared = result_lines(&output);
    assert_eq!(compared[0]["budget"]["exceeded"], serde_json::json!([]));
    write_budget(0.8);
    let output = bench(&["compare", "--budget", budget_arg, results])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{}", describe(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let printed: Value = serde_json::from_str(stdout.trim()).unwrap();
    let exceeded = serde_json::json!(["min_throughput_ratio"]);
    assert_eq!(printed["budget"]["exceeded"], exceeded, "{printed}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("budget exceeded"), "{stderr}");
}
