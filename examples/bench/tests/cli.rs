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
    assert!(scenarios.iter().any(|name| name == "plain"), "{scenarios:?}");
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
            "0",
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
