//! Deterministic diagnosis rules.
//!
//! Every rule reads only observations, never supplied findings, and produces
//! findings whose confidence reflects evidence strength:
//!
//! * `confirmed` requires evidence from a verified producer over a verified
//!   collection path. Offline files never qualify (see [`crate::parse`]).
//! * `likely` covers unverified or indirect evidence.
//! * `unknown` is the correct answer when evidence cannot separate causes.
//! * `conflicting_evidence` preserves contradictions instead of hiding them.
//!
//! Rules never sum overlapping durations, never clamp negative residuals to
//! zero, and never convert missing telemetry into a networking diagnosis.

use std::collections::{BTreeMap, BTreeSet};

use crate::catalog::{self, EDGE_GATEWAY_ERROR_TOKENS, EDGE_PRE_UPSTREAM_PHASES};
use crate::model::{
    Availability, Confidence, DiagnosticReport, Evidence, EvidenceSource, Finding, Observation,
    Owner, ProducerKind, Remediation, Severity, SourceScope, SpanRef, Trust, Verification,
};

/// Tunable thresholds. Defaults are documented in `docs/measurement-semantics.md`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Minimum share of time-to-headers a single operation must take to be
    /// reported as dominating.
    pub dominance_fraction: f64,
    /// Ignore services faster than this (milliseconds) for dominance findings.
    pub dominance_min_ms: f64,
    /// Minimum unattributed residual (milliseconds) worth reporting.
    pub residual_min_ms: f64,
    /// Minimum unattributed residual as a share of the gateway measurement.
    pub residual_min_fraction: f64,
    /// Minimum body duration (milliseconds) for a streaming finding.
    pub streaming_min_ms: f64,
    /// Clock-reading slack allowed when checking interval nesting (nanoseconds).
    pub nesting_slack_nanos: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            dominance_fraction: 0.5,
            dominance_min_ms: 5.0,
            residual_min_ms: 50.0,
            residual_min_fraction: 0.2,
            streaming_min_ms: 100.0,
            nesting_slack_nanos: 1_000_000,
        }
    }
}

/// Most parent hops followed when linking a span to an ancestor. The walk is
/// bounded, so a parent cycle in hostile input cannot loop forever.
const MAX_ANCESTOR_HOPS: usize = 64;

/// Most unsampled, dropped, or unexported observations one finding cites.
pub const MAX_DEGRADED_CITATIONS_PER_FINDING: usize = 32;

/// Most unsampled, dropped, or unexported observations cited across one
/// [`analyze`] run. A finding that reaches either cap says how many it left out.
pub const MAX_DEGRADED_CITATIONS_PER_RUN: usize = 512;

/// Runs every rule and returns findings in deterministic order.
pub fn analyze(report: &DiagnosticReport, thresholds: &Thresholds) -> Vec<Finding> {
    let index = Index::build(report);
    let mut findings = Vec::new();
    rule_edge_rejection(&index, &mut findings);
    rule_gateway_error(&index, &mut findings);
    rule_operation_dominates(&index, thresholds, &mut findings);
    rule_unattributed_interval(&index, thresholds, &mut findings);
    rule_streaming(&index, thresholds, &mut findings);
    rule_body_incomplete(&index, &mut findings);
    rule_negative_values(&index, &mut findings);
    rule_incomplete(&index, &mut findings);
    sort_findings(&mut findings);
    findings
}

/// Orders by severity (most severe first), then rule id, code, and evidence ids.
pub fn sort_findings(findings: &mut [Finding]) {
    fn rank(severity: &Severity) -> u8 {
        match severity {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
            Severity::Unrecognized(_) => 3,
        }
    }
    findings.sort_by(|a, b| {
        rank(&a.severity)
            .cmp(&rank(&b.severity))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.supporting_observations.cmp(&b.supporting_observations))
    });
}

/// A gateway or service request reconstructed from observations sharing a span.
#[derive(Debug, Default)]
struct RequestView<'a> {
    span: Option<&'a SpanRef>,
    observations: Vec<&'a Observation>,
}

impl<'a> RequestView<'a> {
    fn named(&self, name: &str) -> Option<&'a Observation> {
        self.observations.iter().copied().find(|o| o.name == name)
    }

    fn span_id(&self) -> Option<&'a str> {
        self.span.map(|s| s.span_id.as_str())
    }
}

struct Index<'a> {
    report: &'a DiagnosticReport,
    verified_collection: bool,
    /// Edge requests keyed by span id (`""` when no span is known).
    edge: BTreeMap<&'a str, RequestView<'a>>,
    /// Alloy server requests keyed by span id.
    service: BTreeMap<&'a str, RequestView<'a>>,
    operations: Vec<&'a Observation>,
    /// Alloy span id -> parent span id, for every Alloy observation with a span.
    alloy_parents: BTreeMap<&'a str, Option<&'a str>>,
}

const SERVICE_NAMES: &[&str] = &[
    catalog::ALLOY_TIME_TO_HEADERS,
    catalog::ALLOY_BODY_DURATION,
    catalog::ALLOY_SERVER_DURATION,
    catalog::ALLOY_RESPONSE,
    catalog::ALLOY_ADMISSION_WAIT,
];

impl<'a> Index<'a> {
    fn build(report: &'a DiagnosticReport) -> Self {
        let mut edge: BTreeMap<&str, RequestView<'_>> = BTreeMap::new();
        let mut service: BTreeMap<&str, RequestView<'_>> = BTreeMap::new();
        let mut operations = Vec::new();
        let mut alloy_parents = BTreeMap::new();
        for observation in &report.observations {
            let key = observation.span.as_ref().map_or("", |s| s.span_id.as_str());
            match observation.producer.kind {
                ProducerKind::Edge => {
                    let view = edge.entry(key).or_default();
                    view.span = view.span.or(observation.span.as_ref());
                    view.observations.push(observation);
                }
                ProducerKind::Alloy => {
                    if let Some(span) = &observation.span {
                        alloy_parents
                            .entry(span.span_id.as_str())
                            .or_insert(span.parent_span_id.as_deref());
                    }
                    if observation.name == catalog::ALLOY_OPERATION_DURATION {
                        operations.push(observation);
                    } else if SERVICE_NAMES.contains(&observation.name.as_str()) {
                        let view = service.entry(key).or_default();
                        view.span = view.span.or(observation.span.as_ref());
                        view.observations.push(observation);
                    }
                }
                _ => {}
            }
        }
        Self {
            report,
            verified_collection: report.collection.verification == Verification::Verified,
            edge,
            service,
            operations,
            alloy_parents,
        }
    }

    fn verified(&self, observation: &Observation) -> bool {
        self.verified_collection && observation.trust == Trust::Verified
    }

    /// Service requests whose parent span is the given gateway span.
    fn services_under(&self, edge_span_id: &str) -> Vec<&RequestView<'a>> {
        self.service
            .values()
            .filter(|view| {
                view.span
                    .and_then(|s| s.parent_span_id.as_deref())
                    .is_some_and(|parent| parent == edge_span_id)
            })
            .collect()
    }

    /// Returns `true` when `span_id` descends from `ancestor` through Alloy spans.
    fn descends_from(&self, span_id: &str, ancestor: &str) -> bool {
        let mut current = span_id;
        for _ in 0..MAX_ANCESTOR_HOPS {
            match self.alloy_parents.get(current).copied().flatten() {
                Some(parent) if parent == ancestor => return true,
                Some(parent) => current = parent,
                None => return false,
            }
        }
        false
    }

    /// Returns the nearest id in `span`'s lineage that `accept` matches: the
    /// span itself, its own parent, then its Alloy ancestors. The walk stops
    /// after [`MAX_ANCESTOR_HOPS`] parents, so a parent cycle cannot loop.
    fn nearest_in_lineage<'s>(
        &'s self,
        span: &'s SpanRef,
        accept: impl Fn(&str) -> bool,
    ) -> Option<&'s str> {
        if accept(span.span_id.as_str()) {
            return Some(span.span_id.as_str());
        }
        let parent = span.parent_span_id.as_deref();
        if let Some(parent) = parent.filter(|&parent| accept(parent)) {
            return Some(parent);
        }
        let mut current: &'s str = span.span_id.as_str();
        for _ in 0..MAX_ANCESTOR_HOPS {
            let parent = self.alloy_parents.get(current).copied().flatten()?;
            if accept(parent) {
                return Some(parent);
            }
            current = parent;
        }
        None
    }
}

fn evidence(source: EvidenceSource, key: &str, value: impl Into<String>) -> Evidence {
    Evidence {
        source,
        key: key.to_owned(),
        value: value.into(),
        attempt: None,
    }
}

fn source_for(observation: &Observation) -> EvidenceSource {
    match observation.producer.kind {
        ProducerKind::Edge => EvidenceSource::GatewayTelemetry,
        ProducerKind::Alloy => EvidenceSource::ServiceTelemetry,
        ProducerKind::User => EvidenceSource::Assertion,
        _ => EvidenceSource::HttpHeader,
    }
}

fn ms(value: f64) -> String {
    format!("{value:.1} ms")
}

struct FindingBuilder(Finding);

impl FindingBuilder {
    fn new(code: &str, rule_id: &str, rule_version: u32, title: &str) -> Self {
        Self(Finding {
            code: code.to_owned(),
            rule_id: rule_id.to_owned(),
            rule_version,
            title: title.to_owned(),
            explanation: String::new(),
            scope: SourceScope::Unknown,
            confidence: Confidence::Unknown,
            severity: Severity::Info,
            evidence: Vec::new(),
            alternatives: Vec::new(),
            does_not_prove: Vec::new(),
            remediation: Vec::new(),
            owner: Owner::Unknown,
            confirm_with: Vec::new(),
            supporting_observations: Vec::new(),
            missing_evidence: Vec::new(),
        })
    }
    fn explanation(mut self, text: String) -> Self {
        self.0.explanation = text;
        self
    }
    fn scope(mut self, scope: SourceScope) -> Self {
        self.0.scope = scope;
        self
    }
    fn confidence(mut self, confidence: Confidence) -> Self {
        self.0.confidence = confidence;
        self
    }
    fn severity(mut self, severity: Severity) -> Self {
        self.0.severity = severity;
        self
    }
    fn owner(mut self, owner: Owner) -> Self {
        self.0.owner = owner;
        self
    }
    fn cite(mut self, observation: &Observation, key: &str, value: impl Into<String>) -> Self {
        self.0.evidence.push(Evidence {
            attempt: observation.scope.attempt,
            ..evidence(source_for(observation), key, value)
        });
        if !self.0.supporting_observations.contains(&observation.id) {
            self.0.supporting_observations.push(observation.id.clone());
        }
        self
    }
    /// Cites degraded observations up to [`MAX_DEGRADED_CITATIONS_PER_FINDING`]
    /// and the run's remaining `budget`, then says in the explanation how many
    /// were left out. Call it after [`Self::explanation`].
    fn cite_degraded(mut self, observations: &[&Observation], budget: &mut usize) -> Self {
        let cited = observations
            .len()
            .min(MAX_DEGRADED_CITATIONS_PER_FINDING)
            .min(*budget);
        for observation in observations.iter().take(cited) {
            self = self.cite(
                observation,
                &observation.name,
                observation.availability.as_str(),
            );
        }
        *budget -= cited;
        let omitted = observations.len() - cited;
        if omitted > 0 {
            let noun = if omitted == 1 {
                "observation is"
            } else {
                "observations are"
            };
            let note = format!(
                " {omitted} more degraded {noun} not cited; the citation limit was reached."
            );
            self.0.explanation.push_str(&note);
        }
        self
    }
    fn alternatives(mut self, items: &[&str]) -> Self {
        self.0
            .alternatives
            .extend(items.iter().map(|s| (*s).to_owned()));
        self
    }
    fn does_not_prove(mut self, items: &[&str]) -> Self {
        self.0
            .does_not_prove
            .extend(items.iter().map(|s| (*s).to_owned()));
        self
    }
    fn confirm_with(mut self, items: &[&str]) -> Self {
        self.0
            .confirm_with
            .extend(items.iter().map(|s| (*s).to_owned()));
        self
    }
    fn missing(mut self, items: &[&str]) -> Self {
        self.0
            .missing_evidence
            .extend(items.iter().map(|s| (*s).to_owned()));
        self
    }
    fn remediation(mut self, text: &str, owner: Owner) -> Self {
        self.0.remediation.push(Remediation {
            text: text.to_owned(),
            owner,
        });
        self
    }
    fn build(mut self) -> Finding {
        self.0.supporting_observations.sort();
        self.0
    }
}

/// R001: Edge rejected the request before any upstream attempt.
fn rule_edge_rejection(index: &Index<'_>, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r001";
    for view in index.edge.values() {
        let services = view
            .span_id()
            .map(|id| index.services_under(id))
            .unwrap_or_default();
        if let Some(rejected) = view.named(catalog::EDGE_REQUEST_REJECTED) {
            let phase = rejected.attr("phase");
            let pre_upstream = phase.is_some_and(|p| EDGE_PRE_UPSTREAM_PHASES.contains(&p));
            if phase.is_some() && !pre_upstream {
                continue;
            }
            let mut builder = FindingBuilder::new(
                "alloy.edge.rejected_before_upstream",
                RULE,
                1,
                "Gateway rejected the request before contacting the service",
            )
            .scope(SourceScope::GatewayAdmission)
            .severity(Severity::Warning)
            .owner(if phase == Some("authenticate") {
                Owner::Caller
            } else {
                Owner::GatewayOperator
            })
            .cite(
                rejected,
                "edge.rejection_phase",
                phase.unwrap_or("<unknown>"),
            )
            .does_not_prove(&[
                "that the gateway policy is misconfigured",
                "which plugin rejected the request (Ferrum Edge v0.9.7 and v0.9.8 record only the phase)",
            ])
            .confirm_with(&[
                "the gateway transaction log entry for this request (metadata.rejection_phase)",
            ]);
            if let Some(status) = rejected.attr("status") {
                builder = builder.cite(rejected, "http.response.status_code", status);
            }
            if let Some(service) = services.first() {
                let conflicting = service.observations.first().copied();
                builder = builder
                    .confidence(Confidence::ConflictingEvidence)
                    .explanation(
                        "The gateway reports a rejection before any upstream attempt, but service telemetry is linked to the same gateway span. At least one of the two records is wrong or they describe different requests.".into(),
                    );
                if let Some(conflicting) = conflicting {
                    builder = builder.cite(conflicting, "alloy.linked_server_span", "present");
                }
            } else if phase.is_none() {
                builder = builder
                    .confidence(Confidence::Unknown)
                    .explanation(
                        "The gateway reported a rejection without a phase, so it is unknown whether an upstream attempt occurred.".into(),
                    )
                    .missing(&["edge.rejection_phase"]);
            } else {
                let confidence = if index.verified(rejected) {
                    Confidence::Confirmed
                } else {
                    Confidence::Likely
                };
                builder = builder.confidence(confidence).explanation(format!(
                    "Ferrum Edge rejected the request in its {:?} phase, which runs before any upstream attempt. No service telemetry is linked to this gateway request.",
                    phase.unwrap_or_default()
                ));
                if !index.verified(rejected) {
                    builder =
                        builder.missing(&["verified gateway provenance for the rejection event"]);
                }
            }
            out.push(builder.build());
            continue;
        }

        // No explicit rejection event: a 4xx with no backend response timing
        // recorded by the gateway is consistent with, but not proof of, a
        // gateway-side rejection.
        let (Some(response), Some(ttfb)) = (
            view.named(catalog::EDGE_RESPONSE),
            view.named(catalog::EDGE_BACKEND_TIME_TO_HEADERS),
        ) else {
            continue;
        };
        let status = response.attr("status").and_then(|s| s.parse::<u16>().ok());
        let is_4xx = status.is_some_and(|s| (400..500).contains(&s));
        if !is_4xx
            || ttfb.availability != Availability::Unavailable
            || view.named(catalog::EDGE_GATEWAY_ERROR).is_some()
        {
            continue;
        }
        let mut builder = FindingBuilder::new(
            "alloy.edge.no_backend_response_recorded",
            RULE,
            1,
            "Gateway recorded no backend response for a client error",
        )
        .scope(SourceScope::GatewayAdmission)
        .severity(Severity::Warning)
        .owner(Owner::GatewayOperator)
        .cite(response, "http.response.status_code", status.map(|s| s.to_string()).unwrap_or_default())
        .cite(ttfb, "gateway.latency.backend_ttfb_ms", "unavailable (gateway sentinel)")
        .alternatives(&[
            "the gateway rejected the request in a pre-upstream phase (authentication, authorization, rate limiting, request policy)",
            "the backend timing was not recorded for another reason",
        ])
        .does_not_prove(&[
            "that the service was never reached",
            "which gateway phase or plugin produced the response",
        ])
        .confirm_with(&[
            "the gateway transaction log entry for this request (metadata.rejection_phase, latency_backend_ttfb_ms)",
        ])
        .missing(&["edge.request.rejected event with phase"]);
        if let Some(service) = services
            .first()
            .and_then(|s| s.observations.first().copied())
        {
            builder = builder
                .confidence(Confidence::ConflictingEvidence)
                .cite(service, "alloy.linked_server_span", "present")
                .explanation(
                    "The gateway recorded no backend response time, yet service telemetry is linked to the same gateway span.".into(),
                );
        } else {
            builder = builder.confidence(Confidence::Likely).explanation(
                "Ferrum Edge answered with a client error and exported its 'unknown' sentinel for backend time-to-headers, and no service telemetry is linked to this gateway request. That pattern is consistent with a gateway-side rejection before any upstream attempt.".into(),
            );
        }
        out.push(builder.build());
    }
}

/// R007: gateway error classification (X-Gateway-Error token or Edge error class).
fn rule_gateway_error(index: &Index<'_>, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r007";
    let header_events = index.report.observations.iter().filter(|o| {
        o.name == catalog::CLIENT_RESPONSE_HEADER
            && o.attr("header")
                .is_some_and(|h| h.eq_ignore_ascii_case("x-gateway-error"))
    });
    for observation in header_events {
        let token = observation.attr("value").unwrap_or_default();
        let known = EDGE_GATEWAY_ERROR_TOKENS.iter().find(|(t, _)| *t == token);
        let mut builder = FindingBuilder::new(
            "alloy.edge.gateway_error_token",
            RULE,
            1,
            "Response carried a gateway error token",
        )
        .scope(SourceScope::GatewayToUpstream)
        .severity(Severity::Error)
        .owner(Owner::GatewayOperator)
        .cite(observation, "header.x-gateway-error", token)
        .does_not_prove(does_not_prove_for_token(token))
        .confirm_with(&[
            "the gateway transaction log error_class for this request",
            "gateway span attribute gateway.error.class",
        ]);
        builder = match known {
            Some((_, meaning)) => builder
                // Ferrum Edge v0.9.8 strips a backend-supplied X-Gateway-Error,
                // but v0.9.5/v0.9.7 do not on every path, and the header names no
                // Edge version or authenticated sender, so it caps at likely.
                .confidence(Confidence::Likely)
                .explanation(format!(
                    "X-Gateway-Error: {token}. {meaning} Ferrum Edge before v0.9.8 lets a backend inject the header on some paths, and the header names no gateway version, so it is not authenticated gateway evidence."
                ))
                .missing(&["authenticated gateway diagnostic record"]),
            None => builder.confidence(Confidence::Unknown).explanation(format!(
                "X-Gateway-Error carried an unrecognized token {token:?}; no meaning is inferred."
            )),
        };
        out.push(builder.build());
    }

    for view in index.edge.values() {
        let Some(error) = view.named(catalog::EDGE_GATEWAY_ERROR) else {
            continue;
        };
        let class = error.attr("error_class").unwrap_or("<unknown>");
        let confidence = if index.verified(error) {
            Confidence::Confirmed
        } else {
            Confidence::Likely
        };
        let builder = FindingBuilder::new(
            "alloy.edge.gateway_error_class",
            RULE,
            1,
            "Gateway classified an upstream failure",
        )
        .scope(SourceScope::GatewayToUpstream)
        .severity(Severity::Error)
        .owner(Owner::GatewayOperator)
        .confidence(confidence)
        .cite(error, "gateway.error.class", class)
        .explanation(format!(
            "Ferrum Edge classified this request's failure as {class:?} in its own telemetry. The class is the gateway's typed verdict about the gateway-to-service exchange."
        ))
        .does_not_prove(&[
            "that the service process crashed or is down",
            "that a network device dropped packets",
        ])
        .confirm_with(&["the gateway transaction log entry for this request"]);
        out.push(builder.build());
    }
}

fn does_not_prove_for_token(token: &str) -> &'static [&'static str] {
    match token {
        "connection_failure" => &[
            "that DNS resolution failed",
            "that TLS failed",
            "that the service is down",
        ],
        "backend_timeout" => &[
            "that the service received the request",
            "that the service is slow rather than unreachable",
        ],
        "backend_error" => &["that the service itself returned this error"],
        "circuit_breaker_open" => &["that the service is down right now"],
        "overload" => &["that the gateway host is CPU-bound"],
        "request_timeout" => &[
            "that the service received the request",
            "which gateway phase used up the deadline",
        ],
        _ => &["any specific root cause"],
    }
}

/// Where an operation ran relative to the header phase of its server request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Same-instance intervals place the operation inside the header phase.
    Inside,
    /// Same-instance intervals place the operation after response headers were
    /// produced, for example while the response body was streamed.
    AfterHeaders,
    /// Same-instance intervals show the operation crossing a header-phase
    /// boundary, so only part of its duration belongs to the header phase.
    Straddles,
    /// No same-instance intervals place the operation.
    Unknown,
}

/// Places `operation` relative to the header-phase interval of `server`
/// (`alloy.server.time_to_headers`). Intervals from different or unnamed
/// producer instances are never compared.
fn placement(operation: &Observation, server: &Observation, slack_nanos: u64) -> Placement {
    let (Some(inner), Some(outer)) = (operation.interval, server.interval) else {
        return Placement::Unknown;
    };
    if !same_instance(operation, server) || inner.start_unix_nano > inner.end_unix_nano {
        return Placement::Unknown;
    }
    if inner.within(&outer, slack_nanos) {
        Placement::Inside
    } else if inner.start_unix_nano.saturating_add(slack_nanos) >= outer.end_unix_nano {
        Placement::AfterHeaders
    } else {
        Placement::Straddles
    }
}

/// R002: one explicitly instrumented operation dominated time-to-headers.
///
/// Only operations placed inside the header phase, or whose placement is
/// unknown, are compared. An operation the evidence places after headers, or
/// across the headers boundary, cannot explain the time to headers with its
/// whole duration, so it is never compared. Unknown placement yields at most
/// `unknown` confidence.
fn rule_operation_dominates(index: &Index<'_>, thresholds: &Thresholds, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r002";
    for view in index.service.values() {
        let Some(server) = view.named(catalog::ALLOY_TIME_TO_HEADERS) else {
            continue;
        };
        let Some(server_ms) = server.duration_ms() else {
            continue;
        };
        if server_ms < thresholds.dominance_min_ms {
            continue;
        }
        let Some(server_span) = view.span_id() else {
            continue;
        };
        // Candidate operations: descendants of this server span. An operation
        // placed inside the header phase outranks one whose placement is
        // unknown; otherwise the longest wins.
        let mut best: Option<(&Observation, f64, bool)> = None;
        for operation in &index.operations {
            let Some(op_ms) = operation.duration_ms() else {
                continue;
            };
            let Some(op_span) = operation.span.as_ref() else {
                continue;
            };
            if !index.descends_from(&op_span.span_id, server_span) {
                continue;
            }
            let nested = match placement(operation, server, thresholds.nesting_slack_nanos) {
                Placement::Inside => true,
                Placement::Unknown => false,
                Placement::AfterHeaders | Placement::Straddles => continue,
            };
            let rank = (nested, op_ms);
            if best.is_none_or(|(_, best_ms, best_nested)| rank > (best_nested, best_ms)) {
                best = Some((operation, op_ms, nested));
            }
        }
        let Some((operation, op_ms, nested)) = best else {
            continue;
        };
        if op_ms > server_ms {
            // Handled by the inconsistency rule when nesting is claimed;
            // otherwise the operation outlived the response head (for example
            // it continued in the body or in background work).
            if nested {
                out.push(
                    FindingBuilder::new(
                        "alloy.evidence.operation_exceeds_enclosing",
                        "alloy.r006",
                        2,
                        "Nested operation is longer than its enclosing measurement",
                    )
                    .scope(SourceScope::UpstreamApplication)
                    .severity(Severity::Warning)
                    .confidence(Confidence::ConflictingEvidence)
                    .owner(Owner::ApiOwner)
                    .cite(server, "alloy.server.time_to_headers", ms(server_ms))
                    .cite(operation, "alloy.operation.duration", ms(op_ms))
                    .explanation(
                        "An operation recorded inside the request's time-to-headers interval reports a longer duration than that interval. The measurements are inconsistent; no dominance claim is made.".into(),
                    )
                    .does_not_prove(&[
                        "which of the service time-to-headers measurement or operation duration is inaccurate",
                        "whether clock skew or a misattributed parent span explains the apparent nesting",
                        "that time was not double-counted in either measurement",
                    ])
                    .build(),
                );
            }
            continue;
        }
        let fraction = op_ms / server_ms;
        if fraction < thresholds.dominance_fraction {
            continue;
        }
        let op_name = operation
            .attr("operation.name")
            .unwrap_or("<unnamed operation>");
        let is_db = operation.attr("operation.kind") == Some("db");
        let confidence = match (nested, index.verified(operation) && index.verified(server)) {
            (true, true) => Confidence::Confirmed,
            (true, false) => Confidence::Likely,
            (false, _) => Confidence::Unknown,
        };
        let explanation = if nested {
            format!(
                "The application-observed operation {op_name:?} took {} of the {} the service spent before producing response headers ({:.0}%). Only the single largest operation is compared; overlapping operations are never added together.",
                ms(op_ms),
                ms(server_ms),
                fraction * 100.0
            )
        } else {
            format!(
                "The application-observed operation {op_name:?} took {}, {:.0}% of the {} the service spent before producing response headers, but no same-instance intervals place it inside that phase. It may have run while the response body was produced, so it is not attributed to the time to headers. Only the single largest operation is compared; overlapping operations are never added together.",
                ms(op_ms),
                fraction * 100.0,
                ms(server_ms)
            )
        };
        let mut builder = FindingBuilder::new(
            "alloy.service.operation_dominates",
            RULE,
            2,
            "An instrumented operation dominated the service's time to response headers",
        )
        .scope(SourceScope::UpstreamApplication)
        .severity(if nested {
            Severity::Warning
        } else {
            Severity::Info
        })
        .owner(Owner::ApiOwner)
        .confidence(confidence)
        .cite(server, "alloy.server.time_to_headers", ms(server_ms))
        .cite(operation, "alloy.operation.duration", format!("{} ({op_name})", ms(op_ms)))
        .explanation(explanation)
        .alternatives(&[
            "the dependency was slow",
            "the operation waited on a contended resource (pool, lock, or rate limit) that is inside the measured interval",
        ])
        .confirm_with(&[
            "the dependency's own server-side measurements for the same time window",
            "pool wait measurements (alloy.db.pool_wait) for the same request",
        ]);
        if is_db {
            builder = builder.does_not_prove(&[
                "database server execution time (this is application-observed call time, including pool wait, network transfer, and driver work)",
                "that the query plan is inefficient",
            ]);
        } else {
            builder = builder.does_not_prove(&[
                "which part of the operation was slow",
                "that removing the operation would reduce end-user latency by the same amount",
            ]);
        }
        if !nested {
            builder = builder
                .does_not_prove(&[
                    "that the operation ran before response headers were produced (no same-instance intervals place it in the header phase; it may have run during the response body)",
                ])
                .missing(&[
                    "same-instance intervals placing the operation inside the header phase",
                ]);
        }
        out.push(builder.build());
    }
}

fn same_instance(a: &Observation, b: &Observation) -> bool {
    match (&a.producer.instance, &b.producer.instance) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// R003: gateway backend time minus service time, when comparable.
fn rule_unattributed_interval(index: &Index<'_>, thresholds: &Thresholds, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r003";
    for view in index.edge.values() {
        let (Some(edge_span), Some(ttfb)) = (
            view.span_id(),
            view.named(catalog::EDGE_BACKEND_TIME_TO_HEADERS),
        ) else {
            continue;
        };
        let Some(edge_ms) = ttfb.duration_ms() else {
            continue;
        };
        let services = index.services_under(edge_span);
        if services.is_empty() {
            continue;
        }
        if services.len() > 1 {
            let mut builder = FindingBuilder::new(
                "alloy.gateway.multiple_service_attempts",
                RULE,
                1,
                "Several service requests share one gateway request",
            )
            .scope(SourceScope::GatewayToUpstream)
            .severity(Severity::Info)
            .owner(Owner::GatewayOperator)
            .confidence(Confidence::Likely)
            .cite(ttfb, "gateway.latency.backend_ttfb_ms", ms(edge_ms))
            .explanation(format!(
                "{} service server spans have the same gateway span as parent. Ferrum Edge v0.9.7 and v0.9.8 reuse one traceparent for every retry attempt and record no attempt identity, so these are probably separate attempts; gateway and service timings are not compared.",
                services.len()
            ))
            .does_not_prove(&["which attempt produced the final response"])
            .missing(&["per-attempt gateway spans or attempt identifiers"]);
            for service in &services {
                if let Some(first) = service.observations.first() {
                    builder = builder.cite(first, "alloy.server_span", "linked");
                }
            }
            out.push(builder.build());
            continue;
        }
        let Some(service) = services.first() else {
            continue;
        };
        let streamed = ttfb.attr("edge.response.streamed");
        let (service_obs, comparison) = match streamed {
            Some("true") => (
                service.named(catalog::ALLOY_TIME_TO_HEADERS),
                "service time-to-headers (streamed response)",
            ),
            Some("false") => (
                service.named(catalog::ALLOY_SERVER_DURATION),
                "service total duration (buffered response: the gateway measurement includes the body)",
            ),
            _ => (None, ""),
        };
        let Some(service_obs) = service_obs else {
            let mut builder = FindingBuilder::new(
                "alloy.gateway.timings_not_comparable",
                RULE,
                1,
                "Gateway and service timings are not comparable",
            )
            .scope(SourceScope::GatewayToUpstream)
            .severity(Severity::Info)
            .owner(Owner::Unknown)
            .confidence(Confidence::Unknown)
            .cite(ttfb, "gateway.latency.backend_ttfb_ms", ms(edge_ms))
            .explanation(
                "The gateway's backend measurement boundaries could not be matched to a service measurement (unknown response buffering mode or missing service measurement), so no difference is computed.".into(),
            )
            .does_not_prove(&[
                "whether the gateway or service was fast or slow",
                "the size or cause of any gateway-to-service timing difference",
            ])
            .missing(&["gateway response buffering mode", "matching service measurement"]);
            if let Some(first) = service.observations.first() {
                builder = builder.cite(first, "alloy.server_span", "linked");
            }
            out.push(builder.build());
            continue;
        };
        let Some(service_ms) = service_obs.duration_ms() else {
            continue;
        };
        let residual = edge_ms - service_ms;
        if residual < 0.0 {
            out.push(
                FindingBuilder::new(
                    "alloy.evidence.service_exceeds_gateway",
                    "alloy.r006",
                    2,
                    "Service measured longer than the gateway's backend measurement",
                )
                .scope(SourceScope::GatewayToUpstream)
                .severity(Severity::Warning)
                .owner(Owner::Unknown)
                .confidence(Confidence::ConflictingEvidence)
                .cite(ttfb, "gateway.latency.backend_ttfb_ms", ms(edge_ms))
                .cite(service_obs, &service_obs.name, ms(service_ms))
                .explanation(format!(
                    "The service measured {} but the gateway measured only {} for the enclosing backend exchange. The difference ({}) is negative, which the declared boundaries do not allow; it is reported instead of being clamped to zero.",
                    ms(service_ms),
                    ms(edge_ms),
                    ms(residual)
                ))
                .alternatives(&[
                    "the measurements describe different requests or attempts",
                    "one producer's boundaries differ from its documentation",
                ])
                .does_not_prove(&[
                    "which measurement is inaccurate",
                    "whether clock skew or a misattributed parent span explains the difference",
                ])
                .build(),
            );
            continue;
        }
        if residual < thresholds.residual_min_ms
            || residual / edge_ms < thresholds.residual_min_fraction
        {
            continue;
        }
        let verified_attempt = ttfb.scope.attempt.is_some() && index.verified(ttfb);
        let mut builder = FindingBuilder::new(
            "alloy.gateway.unattributed_interval",
            RULE,
            1,
            "Large unattributed interval between gateway and service",
        )
        .scope(SourceScope::GatewayToUpstream)
        .severity(Severity::Warning)
        .owner(Owner::Unknown)
        .confidence(if verified_attempt {
            Confidence::Confirmed
        } else {
            Confidence::Likely
        })
        .cite(ttfb, "gateway.latency.backend_ttfb_ms", ms(edge_ms))
        .cite(service_obs, &service_obs.name, ms(service_ms))
        .explanation(format!(
            "The gateway measured {} from backend dispatch to response headers; the {comparison} was {}. {} ({:.0}%) is unattributed: neither producer measured it.",
            ms(edge_ms),
            ms(service_ms),
            ms(residual),
            residual / edge_ms * 100.0
        ))
        .alternatives(&[
            "connection establishment (DNS, TCP, TLS) by the gateway",
            "gateway retries and backoff for attempts that never reached the service",
            "request transfer and queuing before the request entered Alloy middleware (accept backlog, TLS, header parsing)",
            "an intermediary between gateway and service",
        ])
        .alternatives(&[if streamed == Some("true") {
            "transfer of the response headers back to the gateway"
        } else {
            "response body transfer and flow control back to the gateway"
        }])
        .does_not_prove(&[
            "network latency",
            "that the network is slow",
            "that the service was idle during the interval",
        ])
        .confirm_with(&[
            "gateway connection-pool reuse and connect timing (not recorded by Ferrum Edge v0.9.7 or v0.9.8)",
            "gateway retry logs (\"Retrying backend request\") for this request",
        ]);
        if !verified_attempt {
            builder = builder.missing(&[
                "gateway attempt identity",
                "gateway connection setup timing",
            ]);
        }
        out.push(builder.build());
    }
}

/// R005: response headers were fast but the body kept streaming.
fn rule_streaming(index: &Index<'_>, thresholds: &Thresholds, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r005";
    for view in index.service.values() {
        let (Some(head), Some(body)) = (
            view.named(catalog::ALLOY_TIME_TO_HEADERS),
            view.named(catalog::ALLOY_BODY_DURATION),
        ) else {
            continue;
        };
        let (Some(head_ms), Some(body_ms)) = (head.duration_ms(), body.duration_ms()) else {
            continue;
        };
        if body_ms < thresholds.streaming_min_ms || body_ms < head_ms * 2.0 {
            continue;
        }
        let outcome = body.attr("body.outcome").unwrap_or("unknown");
        out.push(
            FindingBuilder::new(
                "alloy.response.streaming_dominates",
                RULE,
                1,
                "Most of the request was spent streaming the response body",
            )
            .scope(SourceScope::ResponseDelivery)
            .severity(Severity::Info)
            .owner(Owner::ApiOwner)
            .confidence(if index.verified(head) && index.verified(body) {
                Confidence::Confirmed
            } else {
                Confidence::Likely
            })
            .cite(head, "alloy.server.time_to_headers", ms(head_ms))
            .cite(body, "alloy.server.body_duration", format!("{} (outcome {outcome})", ms(body_ms)))
            .explanation(format!(
                "The service produced response headers after {} and the body stream then lasted {} (outcome: {outcome}).",
                ms(head_ms),
                ms(body_ms)
            ))
            .does_not_prove(&[
                "when the client received the bytes (Alloy observes frames handed to Hyper, not delivery)",
                "that the handler was slow; long-lived streams (SSE, downloads) are expected to last",
            ])
            .build(),
        );
    }
}

/// R008: the response body did not complete from the service's view.
fn rule_body_incomplete(index: &Index<'_>, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r008";
    for view in index.service.values() {
        let Some(body) = view.named(catalog::ALLOY_BODY_DURATION) else {
            continue;
        };
        let outcome = body.attr("body.outcome").unwrap_or("unknown");
        if !matches!(outcome, "cancelled" | "error") {
            continue;
        }
        let when = body.duration_ms().map_or_else(
            || "at an unknown time".to_owned(),
            |v| format!("after {}", ms(v)),
        );
        out.push(
            FindingBuilder::new(
                "alloy.response.body_incomplete",
                RULE,
                1,
                "Response body did not complete",
            )
            .scope(SourceScope::ResponseDelivery)
            .severity(Severity::Warning)
            .owner(Owner::Unknown)
            .confidence(if index.verified(body) {
                Confidence::Confirmed
            } else {
                Confidence::Likely
            })
            .cite(body, "alloy.response.body.outcome", outcome)
            .explanation(format!(
                "The service's response body ended with outcome {outcome:?} {when}: it was {} before the final frame.",
                if outcome == "cancelled" { "dropped" } else { "terminated by an error" }
            ))
            .alternatives(&[
                "the client disconnected",
                "the gateway abandoned the upstream exchange (for example a read timeout)",
                "the connection was reset",
                "the service shut down while streaming",
                "the body stream itself failed",
            ])
            .does_not_prove(&[
                "which peer ended the exchange",
                "that the service crashed",
            ])
            .confirm_with(&[
                "gateway telemetry for the same request (gateway.client.disconnected, gateway.body.error_class)",
            ])
            .build(),
        );
    }
}

/// R006: negative measured values are inconsistent evidence.
fn rule_negative_values(index: &Index<'_>, out: &mut Vec<Finding>) {
    for observation in &index.report.observations {
        if observation.availability != Availability::Measured {
            continue;
        }
        let Some(value) = observation.value else {
            continue;
        };
        if value >= 0.0 || !value.is_finite() {
            continue;
        }
        out.push(
            FindingBuilder::new(
                "alloy.evidence.negative_measurement",
                "alloy.r006",
                2,
                "A measurement reported a negative value",
            )
            .scope(SourceScope::Unknown)
            .severity(Severity::Warning)
            .owner(Owner::Unknown)
            .confidence(Confidence::ConflictingEvidence)
            .cite(observation, &observation.name, format!("{value}"))
            .explanation(format!(
                "{} reported {value}, which a duration cannot be. The value is not clamped to zero and is excluded from comparisons.",
                observation.name
            ))
            .does_not_prove(&[
                "why the producer reported a negative value",
                "whether other measurements from the same producer are accurate",
            ])
            .build(),
        );
    }
}

/// R004: telemetry is too incomplete to localize the delay.
fn rule_incomplete(index: &Index<'_>, out: &mut Vec<Finding>) {
    const RULE: &str = "alloy.r004";
    const VERSION: u32 = 2;
    let degraded: Vec<&Observation> = index
        .report
        .observations
        .iter()
        .filter(|o| {
            matches!(
                o.availability,
                Availability::NotSampled | Availability::Dropped | Availability::ExportPending
            )
        })
        .collect();
    let mut budget = MAX_DEGRADED_CITATIONS_PER_RUN;

    // Gateway spans that some service request names as its parent.
    let served: BTreeSet<&str> = index
        .service
        .values()
        .filter_map(|view| view.span.and_then(|span| span.parent_span_id.as_deref()))
        .collect();
    // Gateway requests with a measured backend exchange and no service child,
    // each with its time-to-headers and the degraded observations linked to it.
    let mut missing: BTreeMap<&str, (&Observation, Vec<&Observation>)> = index
        .edge
        .values()
        .filter_map(|view| {
            let edge_span = view.span_id().filter(|span| !served.contains(span))?;
            let ttfb = view
                .named(catalog::EDGE_BACKEND_TIME_TO_HEADERS)
                .filter(|ttfb| ttfb.duration_ms().is_some())?;
            Some((edge_span, (ttfb, Vec::new())))
        })
        .collect();
    let requests = missing.len();

    // Each degraded observation belongs to the nearest gateway span in its
    // lineage, which is walked once, so the cost is linear in the report.
    let mut unlinked: Vec<&Observation> = Vec::new();
    if requests > 0 {
        let is_gateway = |id: &str| index.edge.contains_key(id);
        for observation in &degraded {
            let gateway = observation
                .span
                .as_ref()
                .and_then(|span| index.nearest_in_lineage(span, is_gateway));
            let cited = match gateway {
                // Evidence for a gateway request that has service telemetry
                // belongs to that request, not to this rule.
                Some(span) => missing.get_mut(span).map(|(_, cited)| cited),
                // With one missing request, unlinked evidence can describe only it.
                None if requests == 1 => missing.values_mut().next().map(|(_, cited)| cited),
                None => Some(&mut unlinked),
            };
            if let Some(cited) = cited {
                cited.push(*observation);
            }
        }
    }

    for (ttfb, cited) in missing.values() {
        let builder = FindingBuilder::new(
            "alloy.telemetry.service_span_missing",
            RULE,
            VERSION,
            "No service telemetry is linked to this gateway request",
        )
        .scope(SourceScope::Unknown)
        .severity(Severity::Info)
        .owner(Owner::Unknown)
        .confidence(Confidence::Unknown)
        .cite(ttfb, "gateway.latency.backend_ttfb_ms", ttfb.duration_ms().map(ms).unwrap_or_default())
        .explanation(
            "The gateway recorded a backend exchange, but no service span has the gateway span as its parent. The delay cannot be localized inside the service.".into(),
        )
        .alternatives(&[
            "the service is not instrumented or does not export traces",
            "the service did not sample or has not yet exported the span",
            "the service's trust policy re-rooted the trace",
            "a different service or intermediary answered",
        ])
        .does_not_prove(&[
            "that the service was never reached",
            "packet loss or any other network fault",
        ])
        .missing(&["alloy.server span whose parent is the gateway span"]);
        out.push(builder.cite_degraded(cited, &mut budget).build());
    }

    // With several missing requests, evidence that links to none of them is
    // cited once here rather than dropped or copied into every request.
    if !unlinked.is_empty() {
        let explanation = format!(
            "{requests} gateway requests have no linked service span. The unsampled, dropped, or \
             unexported observations cited here link to no gateway span, so they cannot be \
             attributed to one request."
        );
        let builder = FindingBuilder::new(
            "alloy.telemetry.degraded_evidence_unlinked",
            RULE,
            VERSION,
            "Degraded telemetry is not linked to a gateway request",
        )
        .scope(SourceScope::Unknown)
        .severity(Severity::Info)
        .owner(Owner::Unknown)
        .confidence(Confidence::Unknown)
        .explanation(explanation)
        .alternatives(&[
            "the observations carry no span",
            "the service's trust policy re-rooted the trace",
            "the observations describe a request that is not in this report",
        ])
        .does_not_prove(&[
            "which gateway request the observations describe",
            "that the service was never reached",
            "packet loss or any other network fault",
        ])
        .missing(&["a span linking each degraded observation to its gateway span"]);
        out.push(builder.cite_degraded(&unlinked, &mut budget).build());
    }

    let has_measurement = index
        .report
        .observations
        .iter()
        .any(|o| o.duration_ms().is_some());
    if !has_measurement {
        let builder = FindingBuilder::new(
            "alloy.telemetry.insufficient",
            RULE,
            VERSION,
            "Telemetry is insufficient to localize any delay",
        )
        .scope(SourceScope::Unknown)
        .severity(Severity::Info)
        .owner(Owner::Unknown)
        .confidence(Confidence::Unknown)
        .explanation(
            "The report contains no measured durations, so no timing conclusion is possible."
                .into(),
        )
        .does_not_prove(&["that any component was slow or fast"])
        .missing(&["gateway backend timing", "service time-to-headers"]);
        out.push(builder.cite_degraded(&degraded, &mut budget).build());
    } else if index.edge.is_empty() && !index.service.is_empty() {
        out.push(
            FindingBuilder::new(
                "alloy.telemetry.gateway_evidence_missing",
                RULE,
                VERSION,
                "No gateway telemetry is present",
            )
            .scope(SourceScope::Unknown)
            .severity(Severity::Info)
            .owner(Owner::Unknown)
            .confidence(Confidence::Unknown)
            .explanation(
                "Only service telemetry is present. Time spent before the request reached the service, or after it left, cannot be localized.".into(),
            )
            .does_not_prove(&["that the gateway or network added no latency"])
            .missing(&["gateway span for the same trace"])
            .remediation(
                "Export gateway spans for the same trace (Ferrum Edge otel_tracing plugin) and include them in the report.",
                Owner::GatewayOperator,
            )
            .build(),
        );
    }
}
