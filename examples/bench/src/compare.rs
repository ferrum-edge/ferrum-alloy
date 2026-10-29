//! The `compare` command: every cell's throughput and latency as a ratio to
//! `plain` in the same cell, repetition, and invocation, optionally checked
//! against per-cell budgets.
//!
//! Pairing follows `docs/benchmarks.md`: only lines with `errors == 0` are
//! used, lines are grouped by `run_id` (and commit and allocation mode)
//! before a cell is paired with `plain` of the same repetition, and counting
//! and non-counting runs are never compared. No budget is defined yet; the
//! budget file format exists so that one can be checked once a dedicated
//! host has measured a baseline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::Failure;
use crate::dims::{Dimension, Scenario, Transport, Workload};
use crate::run::SCHEMA;

/// Version of the lines `compare` prints.
pub(crate) const COMPARE_SCHEMA: &str = "alloy-bench-compare/1";

/// Version of the budget file `--budget` reads.
pub(crate) const BUDGET_SCHEMA: &str = "alloy-bench-budget/1";

/// The reference every other scenario is compared with.
const PLAIN: &str = "plain";

/// The keys a budget may have.
const BUDGET_KEYS: &[&str] = &[
    "scenario",
    "workload",
    "transport",
    "alloc_counting",
    "min_throughput_ratio",
    "max_latency_p50_ratio",
    "max_latency_p99_ratio",
];

/// One result line, reduced to what comparison needs.
#[derive(Debug, Clone)]
struct Line {
    run_id: String,
    commit: Option<String>,
    alloc_counting: bool,
    rep: Option<u64>,
    scenario: String,
    workload: String,
    transport: String,
    errors: u64,
    requests_per_second: f64,
    p50: Option<u64>,
    p99: Option<u64>,
}

/// Lines that may be paired: one invocation, commit, allocation mode,
/// workload, and transport.
type GroupKey = (String, Option<String>, bool, String, String);

/// A group's lines by repetition and scenario.
type Cells<'a> = BTreeMap<(Option<u64>, String), &'a Line>;

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value.get(key).ok_or_else(|| format!("missing {key}"))
}

fn string(value: &Value, key: &str) -> Result<String, String> {
    field(value, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} is not a string"))
}

/// A nonnegative integer, or `null` (unknown).
fn optional_count(value: &Value, key: &str) -> Result<Option<u64>, String> {
    match field(value, key)? {
        Value::Null => Ok(None),
        other => other
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} is not a count")),
    }
}

impl Line {
    fn parse(value: &Value) -> Result<Self, String> {
        let schema = field(value, "schema")?;
        if schema.as_str() != Some(SCHEMA) {
            return Err(format!("schema {schema} is not {SCHEMA:?}"));
        }
        let commit = match field(value, "commit")? {
            Value::Null => None,
            other => Some(
                other
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("commit is not a string")?,
            ),
        };
        let requests_per_second = field(value, "requests_per_second")?
            .as_f64()
            .filter(|rate| rate.is_finite() && *rate >= 0.0)
            .ok_or("requests_per_second is not a nonnegative number")?;
        let latency = field(value, "latency_us")?;
        let scenario = string(value, "scenario")?;
        Scenario::parse(&scenario).map_err(|error| error.to_string())?;
        let workload = string(value, "workload")?;
        Workload::parse(&workload).map_err(|error| error.to_string())?;
        let transport = string(value, "transport")?;
        Transport::parse(&transport).map_err(|error| error.to_string())?;
        Ok(Self {
            run_id: string(value, "run_id")?,
            commit,
            alloc_counting: field(value, "alloc_counting")?
                .as_bool()
                .ok_or("alloc_counting is not a boolean")?,
            rep: optional_count(value, "rep")?,
            scenario,
            workload,
            transport,
            errors: field(value, "errors")?
                .as_u64()
                .ok_or("errors is not a count")?,
            requests_per_second,
            p50: optional_count(latency, "p50")?,
            p99: optional_count(latency, "p99")?,
        })
    }

    fn group(&self) -> GroupKey {
        (
            self.run_id.clone(),
            self.commit.clone(),
            self.alloc_counting,
            self.workload.clone(),
            self.transport.clone(),
        )
    }
}

/// Parses result lines, skipping blank ones.
fn parse_lines(text: &str) -> Result<Vec<Line>, String> {
    let mut lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        let value: Value =
            serde_json::from_str(line).map_err(|error| format!("line {number}: {error}"))?;
        lines.push(Line::parse(&value).map_err(|error| format!("line {number}: {error}"))?);
    }
    Ok(lines)
}

/// Median, minimum, and maximum of some values.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Distribution {
    n: usize,
    median: f64,
    min: f64,
    max: f64,
}

impl Distribution {
    /// `None` for no values: unknown, not zero.
    fn of(values: &[f64]) -> Option<Self> {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        let middle = n / 2;
        let median = if n.is_multiple_of(2) {
            // The midpoint without adding, which overflows near f64::MAX.
            let (low, high) = (sorted.get(middle.checked_sub(1)?)?, sorted.get(middle)?);
            low + (high - low) / 2.0
        } else {
            *sorted.get(middle)?
        };
        Some(Self {
            n,
            median,
            min: *sorted.first()?,
            max: *sorted.last()?,
        })
    }

    fn to_json(self) -> Value {
        json!({ "n": self.n, "median": self.median, "min": self.min, "max": self.max })
    }
}

/// Ratio of a latency to `plain`'s, when both are known and `plain`'s is
/// not zero.
fn latency_ratio(value: Option<u64>, plain: Option<u64>) -> Option<f64> {
    let plain = plain.filter(|plain| *plain > 0)?;
    Some(value? as f64 / plain as f64)
}

/// One scenario of one group, paired with `plain` in each repetition.
#[derive(Debug)]
struct Comparison {
    group: GroupKey,
    scenario: String,
    reps: Vec<Option<u64>>,
    throughput: Vec<f64>,
    p50: Vec<f64>,
    p99: Vec<f64>,
    plain_requests_per_second: Vec<f64>,
}

impl Comparison {
    fn new(group: GroupKey, scenario: String) -> Self {
        Self {
            group,
            scenario,
            reps: Vec::new(),
            throughput: Vec::new(),
            p50: Vec::new(),
            p99: Vec::new(),
            plain_requests_per_second: Vec::new(),
        }
    }

    /// Fails when the throughput ratio is not finite: finite rates can
    /// still overflow it, and an infinite ratio would pass a minimum.
    fn add(&mut self, line: &Line, plain: &Line) -> Result<(), String> {
        let throughput = line.requests_per_second / plain.requests_per_second;
        if !throughput.is_finite() {
            return Err(format!(
                "{} throughput ratio to plain is not finite in run_id {:?}",
                line.scenario, line.run_id
            ));
        }
        self.reps.push(line.rep);
        self.throughput.push(throughput);
        self.plain_requests_per_second
            .push(plain.requests_per_second);
        if let Some(ratio) = latency_ratio(line.p50, plain.p50) {
            self.p50.push(ratio);
        }
        if let Some(ratio) = latency_ratio(line.p99, plain.p99) {
            self.p99.push(ratio);
        }
        Ok(())
    }

    fn matches(&self, budget: &Budget) -> bool {
        let (_, _, alloc_counting, workload, transport) = &self.group;
        self.scenario == budget.scenario
            && *workload == budget.workload
            && *transport == budget.transport
            && *alloc_counting == budget.alloc_counting
    }

    /// The limits of `budget` this comparison exceeds, by name: a median
    /// throughput ratio below its minimum, or a median latency ratio above
    /// its maximum. A limit on an unknown ratio is exceeded, since unknown
    /// is not within it.
    fn exceeded(&self, budget: &Budget) -> Vec<&'static str> {
        let median = |values: &[f64]| Distribution::of(values).map(|d| d.median);
        let mut exceeded = Vec::new();
        let throughput = median(&self.throughput);
        if budget
            .min_throughput_ratio
            .is_some_and(|min| throughput.is_none_or(|ratio| ratio < min))
        {
            exceeded.push("min_throughput_ratio");
        }
        let p50 = median(&self.p50);
        if budget
            .max_latency_p50_ratio
            .is_some_and(|max| p50.is_none_or(|ratio| ratio > max))
        {
            exceeded.push("max_latency_p50_ratio");
        }
        let p99 = median(&self.p99);
        if budget
            .max_latency_p99_ratio
            .is_some_and(|max| p99.is_none_or(|ratio| ratio > max))
        {
            exceeded.push("max_latency_p99_ratio");
        }
        exceeded
    }

    /// The printed line. `exceeded` is `None` when no budget covers the cell.
    fn to_json(&self, exceeded: Option<&[&'static str]>) -> Value {
        let (run_id, commit, alloc_counting, workload, transport) = &self.group;
        let summary = |values: &[f64]| Distribution::of(values).map(Distribution::to_json);
        json!({
            "schema": COMPARE_SCHEMA,
            "run_id": run_id,
            "commit": commit,
            "alloc_counting": alloc_counting,
            "scenario": self.scenario,
            "workload": workload,
            "transport": transport,
            "pairs": self.throughput.len(),
            "reps": self.reps,
            "throughput_ratio": summary(&self.throughput),
            "latency_p50_ratio": summary(&self.p50),
            "latency_p99_ratio": summary(&self.p99),
            "plain_requests_per_second": summary(&self.plain_requests_per_second),
            "budget": exceeded.map(|exceeded| json!({ "exceeded": exceeded })),
        })
    }
}

/// The comparisons, and how many lines could not be used.
#[derive(Debug, Default)]
struct Report {
    comparisons: Vec<Comparison>,
    /// Lines with client errors, `plain` included.
    skipped_errors: usize,
    /// Error-free lines whose repetition has no usable `plain` line.
    unpaired: usize,
}

/// Pairs every error-free line with the error-free `plain` line of the same
/// group and repetition.
fn compare(lines: &[Line]) -> Result<Report, String> {
    let mut groups: BTreeMap<GroupKey, Cells<'_>> = BTreeMap::new();
    for line in lines {
        let cells = groups.entry(line.group()).or_default();
        if cells
            .insert((line.rep, line.scenario.clone()), line)
            .is_some()
        {
            return Err(format!(
                "two result lines for run_id {} rep {:?} {} {} {} (alloc_counting {})",
                line.run_id,
                line.rep,
                line.scenario,
                line.workload,
                line.transport,
                line.alloc_counting
            ));
        }
    }
    let mut report = Report::default();
    for (group, cells) in &groups {
        let mut by_scenario: BTreeMap<&str, Comparison> = BTreeMap::new();
        for ((rep, scenario), line) in cells {
            if line.errors > 0 {
                report.skipped_errors += 1;
                continue;
            }
            if scenario == PLAIN {
                continue;
            }
            let plain = cells
                .get(&(*rep, PLAIN.to_owned()))
                .filter(|plain| plain.errors == 0 && plain.requests_per_second > 0.0);
            let Some(plain) = plain else {
                report.unpaired += 1;
                continue;
            };
            by_scenario
                .entry(scenario.as_str())
                .or_insert_with(|| Comparison::new(group.clone(), scenario.clone()))
                .add(line, plain)?;
        }
        report.comparisons.extend(by_scenario.into_values());
    }
    if report.comparisons.is_empty() {
        return Err(
            "no error-free result line could be paired with an error-free plain \
                    line of the same run_id, repetition, workload, transport, and \
                    allocation mode"
                .into(),
        );
    }
    Ok(report)
}

/// Limits for one cell, as ratios to `plain`.
#[derive(Debug, Clone, PartialEq)]
struct Budget {
    scenario: String,
    workload: String,
    transport: String,
    alloc_counting: bool,
    min_throughput_ratio: Option<f64>,
    max_latency_p50_ratio: Option<f64>,
    max_latency_p99_ratio: Option<f64>,
}

impl Budget {
    fn parse(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("a budget is a JSON object")?;
        if let Some(key) = object
            .keys()
            .find(|key| !BUDGET_KEYS.contains(&key.as_str()))
        {
            return Err(format!("unknown key {key:?}"));
        }
        let name = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must be a string"))
        };
        let scenario = name("scenario")?;
        Scenario::parse(&scenario).map_err(|error| error.to_string())?;
        if scenario == PLAIN {
            return Err("plain is the reference, so it has no budget".into());
        }
        let workload = name("workload")?;
        Workload::parse(&workload).map_err(|error| error.to_string())?;
        let transport = name("transport")?;
        Transport::parse(&transport).map_err(|error| error.to_string())?;
        let alloc_counting = match object.get("alloc_counting") {
            None => false,
            Some(value) => value.as_bool().ok_or("alloc_counting must be a boolean")?,
        };
        let limit = |key: &str| match object.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_f64()
                .filter(|limit| limit.is_finite() && *limit > 0.0)
                .map(Some)
                .ok_or_else(|| format!("{key} must be a positive number")),
        };
        let budget = Self {
            scenario,
            workload,
            transport,
            alloc_counting,
            min_throughput_ratio: limit("min_throughput_ratio")?,
            max_latency_p50_ratio: limit("max_latency_p50_ratio")?,
            max_latency_p99_ratio: limit("max_latency_p99_ratio")?,
        };
        if budget.min_throughput_ratio.is_none()
            && budget.max_latency_p50_ratio.is_none()
            && budget.max_latency_p99_ratio.is_none()
        {
            return Err("a budget sets at least one limit".into());
        }
        Ok(budget)
    }

    fn same_cell(&self, other: &Self) -> bool {
        self.scenario == other.scenario
            && self.workload == other.workload
            && self.transport == other.transport
            && self.alloc_counting == other.alloc_counting
    }

    fn describe(&self) -> String {
        format!(
            "{} {} {} (alloc_counting {})",
            self.scenario, self.workload, self.transport, self.alloc_counting
        )
    }
}

/// Parses a budget file (format in `docs/benchmarks.md`).
fn parse_budgets(text: &str) -> Result<Vec<Budget>, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let object = value.as_object().ok_or("a budget file is a JSON object")?;
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "schema" | "budgets"))
    {
        return Err(format!("unknown key {key:?}"));
    }
    if object.get("schema").and_then(Value::as_str) != Some(BUDGET_SCHEMA) {
        return Err(format!("schema must be {BUDGET_SCHEMA:?}"));
    }
    let entries = object
        .get("budgets")
        .and_then(Value::as_array)
        .ok_or("budgets must be an array")?;
    let mut budgets: Vec<Budget> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let budget = Budget::parse(entry).map_err(|error| format!("budgets[{index}]: {error}"))?;
        if budgets.iter().any(|other| other.same_cell(&budget)) {
            let cell = budget.describe();
            return Err(format!("budgets[{index}]: a second budget for {cell}"));
        }
        budgets.push(budget);
    }
    Ok(budgets)
}

/// What `compare` prints, and why it fails, if it does.
#[derive(Debug)]
struct Outcome {
    /// One JSON line per comparison, for stdout.
    lines: Vec<Value>,
    /// Notes for stderr.
    notes: Vec<String>,
    /// Set when a budget is exceeded or covers a cell with no paired lines.
    failure: Option<String>,
}

/// Compares `lines` and checks `budgets`, if given.
fn evaluate(lines: &[Line], budgets: Option<&[Budget]>) -> Result<Outcome, String> {
    let report = compare(lines)?;
    let mut notes = Vec::new();
    if report.skipped_errors > 0 {
        notes.push(format!(
            "skipped {} result lines with client errors",
            report.skipped_errors
        ));
    }
    if report.unpaired > 0 {
        notes.push(format!(
            "{} error-free result lines had no error-free plain line to pair with",
            report.unpaired
        ));
    }
    let budgets = budgets.unwrap_or_default();
    let mut checked = vec![false; budgets.len()];
    let mut exceeded_cells = 0;
    let mut output = Vec::new();
    for comparison in &report.comparisons {
        let budget = budgets.iter().position(|budget| comparison.matches(budget));
        let exceeded = budget.and_then(|index| {
            let budget = budgets.get(index)?;
            if let Some(checked) = checked.get_mut(index) {
                *checked = true;
            }
            Some(comparison.exceeded(budget))
        });
        if let Some(exceeded) = exceeded.as_deref().filter(|names| !names.is_empty()) {
            exceeded_cells += 1;
            let (run_id, ..) = &comparison.group;
            let cell = budget
                .and_then(|index| budgets.get(index))
                .map(Budget::describe)
                .unwrap_or_default();
            notes.push(format!(
                "budget exceeded: {cell}, run_id {run_id}: {}",
                exceeded.join(", ")
            ));
        }
        output.push(comparison.to_json(exceeded.as_deref()));
    }
    let mut unchecked = 0;
    for (budget, checked) in budgets.iter().zip(&checked) {
        if !checked {
            unchecked += 1;
            notes.push(format!(
                "budget unchecked: no paired result lines for {}",
                budget.describe()
            ));
        }
    }
    let failure = match (exceeded_cells, unchecked) {
        (0, 0) => None,
        (exceeded, unchecked) => Some(format!(
            "{exceeded} cells exceeded their budget, {unchecked} budgeted cells had no \
             paired result lines"
        )),
    };
    Ok(Outcome {
        lines: output,
        notes,
        failure,
    })
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Reads the result files and the budget file, prints one line per
/// comparison, and fails when a budget is exceeded or unchecked.
pub(crate) fn execute(files: &[PathBuf], budget: Option<&Path>) -> Result<(), Failure> {
    let budgets = match budget {
        Some(path) => {
            let text = read(path)?;
            Some(parse_budgets(&text).map_err(|error| format!("{}: {error}", path.display()))?)
        }
        None => None,
    };
    let mut lines = Vec::new();
    for path in files {
        let text = read(path)?;
        let parsed = parse_lines(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        lines.extend(parsed);
    }
    let outcome = evaluate(&lines, budgets.as_deref())?;
    for line in &outcome.lines {
        println!("{line}");
    }
    for note in &outcome.notes {
        eprintln!("{note}");
    }
    match outcome.failure {
        Some(failure) => Err(failure.into()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

    use super::*;

    /// A result line with the fields `compare` reads.
    fn line(run_id: &str, rep: u64, scenario: &str, rate: f64, p50: u64) -> Value {
        json!({
            "schema": SCHEMA,
            "run_id": run_id,
            "commit": "abc",
            "rep": rep,
            "scenario": scenario,
            "workload": "small",
            "transport": "h1",
            "alloc_counting": false,
            "errors": 0,
            "requests_per_second": rate,
            "latency_us": {
                "p50": p50, "p90": p50, "p99": p50 * 2, "p999": p50 * 3, "max": p50 * 4,
            },
        })
    }

    fn parse(values: &[Value]) -> Vec<Line> {
        let text: Vec<String> = values.iter().map(Value::to_string).collect();
        parse_lines(&text.join("\n")).unwrap()
    }

    /// Five repetitions of `plain` at 100, 110, ... and `alloy` at half.
    fn five_reps() -> Vec<Value> {
        let mut values = Vec::new();
        for rep in 1..=5 {
            let plain = 100.0 + 10.0 * (rep - 1) as f64;
            values.push(line("run", rep, "plain", plain, 100));
            values.push(line("run", rep, "alloy", plain / 2.0, 100 + 10 * rep));
        }
        values
    }

    fn budgets(entries: Value) -> Vec<Budget> {
        let file = json!({ "schema": BUDGET_SCHEMA, "budgets": entries });
        parse_budgets(&file.to_string()).unwrap()
    }

    #[test]
    fn distributions_report_median_and_range() {
        assert_eq!(Distribution::of(&[]), None);
        let odd = Distribution::of(&[3.0, 1.0, 2.0]).unwrap();
        assert_eq!((odd.n, odd.median, odd.min, odd.max), (3, 2.0, 1.0, 3.0));
        let even = Distribution::of(&[4.0, 1.0, 3.0, 2.0]).unwrap();
        assert_eq!(
            (even.n, even.median, even.min, even.max),
            (4, 2.5, 1.0, 4.0)
        );
        // Adding the two middle values would overflow to infinity.
        let large = Distribution::of(&[1e308, 1e308]).unwrap();
        assert_eq!(large.median, 1e308);
    }

    /// Finite rates can overflow the ratio; an infinite ratio would pass any
    /// minimum, so it is refused instead.
    #[test]
    fn a_ratio_that_overflows_is_refused() {
        let lines = parse(&[
            line("run", 1, "plain", 0.1, 100),
            line("run", 1, "alloy", 1e308, 100),
        ]);
        let error = compare(&lines).unwrap_err();
        assert!(error.contains("not finite"), "{error}");
    }

    #[test]
    fn ratios_pair_plain_of_the_same_repetition() {
        let report = compare(&parse(&five_reps())).unwrap();
        assert_eq!(report.comparisons.len(), 1);
        let alloy = &report.comparisons[0];
        assert_eq!(alloy.scenario, "alloy");
        assert_eq!(alloy.throughput, [0.5; 5]);
        let p50 = Distribution::of(&alloy.p50).unwrap();
        assert_eq!((p50.min, p50.median, p50.max), (1.1, 1.3, 1.5));
        let json = alloy.to_json(None);
        assert_eq!(json["schema"], COMPARE_SCHEMA);
        assert_eq!(json["pairs"], 5);
        assert_eq!(json["throughput_ratio"]["median"], 0.5);
        assert_eq!(json["plain_requests_per_second"]["min"], 100.0);
        assert_eq!(json["plain_requests_per_second"]["max"], 140.0);
        assert_eq!(json["budget"], Value::Null);
    }

    #[test]
    fn lines_with_errors_are_skipped() {
        let mut values = five_reps();
        // An errored alloy line is dropped; an errored plain line leaves its
        // repetition's alloy line unpaired.
        values[1]["errors"] = json!(3);
        values[1]["requests_per_second"] = json!(1_000_000.0);
        values[2]["errors"] = json!(1);
        let report = compare(&parse(&values)).unwrap();
        assert_eq!(report.skipped_errors, 2);
        assert_eq!(report.unpaired, 1);
        let alloy = &report.comparisons[0];
        assert_eq!(alloy.reps, [Some(3), Some(4), Some(5)]);
        assert_eq!(alloy.throughput, [0.5; 3]);
    }

    #[test]
    fn invocations_are_never_paired_with_each_other() {
        let mut values = five_reps();
        // The same repetitions of another invocation, with plain only: its
        // plain lines must not pair with the first invocation's alloy lines,
        // nor the first invocation's plain lines with its own.
        for rep in 1..=5 {
            values.push(line("other", rep, "plain", 1.0, 1));
        }
        values.push(line("other", 1, "alloy", 4.0, 1));
        let report = compare(&parse(&values)).unwrap();
        assert_eq!(report.comparisons.len(), 2);
        let other = &report.comparisons[0];
        assert_eq!(other.group.0, "other");
        assert_eq!(other.throughput, [4.0]);
        let run = &report.comparisons[1];
        assert_eq!(run.group.0, "run");
        assert_eq!(run.throughput, [0.5; 5]);
        // An invocation without plain pairs nothing.
        let lonely = [line("lonely", 1, "alloy", 1.0, 1)];
        assert!(compare(&parse(&lonely)).is_err());
    }

    #[test]
    fn allocation_modes_are_never_mixed() {
        let mut values = five_reps();
        for value in &mut values {
            if value["scenario"] == "alloy" {
                value["alloc_counting"] = json!(true);
            }
        }
        // Counting alloy lines have only non-counting plain lines.
        assert!(compare(&parse(&values)).is_err());
        let mut counting = five_reps();
        for value in &mut counting {
            value["alloc_counting"] = json!(true);
            value["requests_per_second"] = json!(1.0);
        }
        values.extend(counting.into_iter().filter(|v| v["scenario"] == "plain"));
        let report = compare(&parse(&values)).unwrap();
        assert_eq!(report.comparisons.len(), 1);
        let alloy = &report.comparisons[0];
        assert!(alloy.group.2);
        assert_eq!(alloy.throughput[0], 50.0);
    }

    #[test]
    fn duplicate_and_malformed_lines_are_rejected() {
        let mut values = five_reps();
        values.push(values[0].clone());
        assert!(compare(&parse(&values)).is_err());
        let mut other_schema = line("run", 1, "plain", 1.0, 1);
        other_schema["schema"] = json!("alloy-bench/2");
        let mut missing = line("run", 1, "plain", 1.0, 1);
        missing.as_object_mut().unwrap().remove("errors");
        let unknown_scenario = line("run", 1, "aloy", 1.0, 1);
        let mut unknown_workload = line("run", 1, "plain", 1.0, 1);
        unknown_workload["workload"] = json!("tiny");
        let mut unknown_transport = line("run", 1, "plain", 1.0, 1);
        unknown_transport["transport"] = json!("h4");
        for value in [
            other_schema,
            missing,
            unknown_scenario,
            unknown_workload,
            unknown_transport,
        ] {
            assert!(parse_lines(&value.to_string()).is_err(), "{value}");
        }
        assert!(parse_lines("not json").is_err());
        assert!(parse_lines("\n\n").unwrap().is_empty());
    }

    #[test]
    fn unknown_latencies_stay_unknown() {
        let mut values = five_reps();
        for value in &mut values {
            value["latency_us"]["p99"] = Value::Null;
        }
        let report = compare(&parse(&values)).unwrap();
        let json = report.comparisons[0].to_json(None);
        assert_eq!(json["latency_p99_ratio"], Value::Null);
        assert_eq!(json["latency_p50_ratio"]["n"], 5);
    }

    #[test]
    fn budgets_pass_within_their_limits() {
        let budgets = budgets(json!([{
            "scenario": "alloy", "workload": "small", "transport": "h1",
            "min_throughput_ratio": 0.5, "max_latency_p50_ratio": 1.3,
        }]));
        let outcome = evaluate(&parse(&five_reps()), Some(budgets.as_slice())).unwrap();
        assert_eq!(outcome.failure, None, "{:?}", outcome.notes);
        assert_eq!(outcome.lines[0]["budget"]["exceeded"], json!([]));
        let unbudgeted = evaluate(&parse(&five_reps()), None).unwrap();
        assert_eq!(unbudgeted.failure, None);
        assert_eq!(unbudgeted.lines[0]["budget"], Value::Null);
    }

    #[test]
    fn budgets_fail_when_exceeded_unknown_or_unchecked() {
        let exceeded = budgets(json!([{
            "scenario": "alloy", "workload": "small", "transport": "h1",
            "min_throughput_ratio": 0.6, "max_latency_p50_ratio": 1.2,
        }]));
        let outcome = evaluate(&parse(&five_reps()), Some(exceeded.as_slice())).unwrap();
        assert!(outcome.failure.is_some());
        assert_eq!(
            outcome.lines[0]["budget"]["exceeded"],
            json!(["min_throughput_ratio", "max_latency_p50_ratio"])
        );
        let mut values = five_reps();
        for value in &mut values {
            value["latency_us"]["p99"] = Value::Null;
        }
        let unknown = budgets(json!([{
            "scenario": "alloy", "workload": "small", "transport": "h1",
            "max_latency_p99_ratio": 100.0,
        }]));
        let outcome = evaluate(&parse(&values), Some(unknown.as_slice())).unwrap();
        assert!(outcome.failure.is_some());
        let unchecked = budgets(json!([{
            "scenario": "alloy", "workload": "large", "transport": "h1",
            "min_throughput_ratio": 0.1,
        }]));
        let outcome = evaluate(&parse(&five_reps()), Some(unchecked.as_slice())).unwrap();
        assert!(outcome.failure.is_some());
        assert_eq!(outcome.lines[0]["budget"], Value::Null);
        // A counting budget does not cover a non-counting cell.
        let counting = budgets(json!([{
            "scenario": "alloy", "workload": "small", "transport": "h1",
            "alloc_counting": true, "min_throughput_ratio": 0.1,
        }]));
        let outcome = evaluate(&parse(&five_reps()), Some(counting.as_slice())).unwrap();
        assert!(outcome.failure.is_some());
    }

    #[test]
    fn budget_files_are_strict() {
        let cell = json!({ "scenario": "alloy", "workload": "small", "transport": "h1" });
        let with = |key: &str, value: Value| {
            let mut entry = cell.clone();
            entry[key] = value;
            json!({ "schema": BUDGET_SCHEMA, "budgets": [entry] }).to_string()
        };
        assert!(parse_budgets(&with("min_throughput_ratio", json!(0.5))).is_ok());
        let empty = json!({ "schema": BUDGET_SCHEMA, "budgets": [] }).to_string();
        assert!(parse_budgets(&empty).unwrap().is_empty());
        let invalid = [
            with("min_throughput_ratio", json!(0)),
            with("min_throughput_ratio", json!(-1.0)),
            with("min_throughput_ratio", json!("0.5")),
            with("max_latency_p90_ratio", json!(1.5)),
            with("scenario", json!("plain")),
            with("scenario", json!("nginx")),
            with("transport", json!("h3")),
            with("alloc_counting", json!("yes")),
            json!({ "schema": BUDGET_SCHEMA, "budgets": [cell] }).to_string(),
            json!({ "schema": "other", "budgets": [] }).to_string(),
            json!({ "schema": BUDGET_SCHEMA, "budgets": [], "extra": 1 }).to_string(),
            json!({ "schema": BUDGET_SCHEMA }).to_string(),
            "[]".to_owned(),
        ];
        for text in invalid {
            assert!(parse_budgets(&text).is_err(), "{text}");
        }
        let entry = json!({ "scenario": "alloy", "workload": "small", "transport": "h1",
                            "min_throughput_ratio": 0.5 });
        let twice = json!({ "schema": BUDGET_SCHEMA, "budgets": [entry, entry] });
        assert!(parse_budgets(&twice.to_string()).is_err());
    }
}
