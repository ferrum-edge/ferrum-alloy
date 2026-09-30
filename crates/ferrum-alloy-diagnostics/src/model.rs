//! Diagnostic report data model, schema `ferrum.diagnostic_report` version 1.
//!
//! The JSON representation is the contract; these Rust types are one reader and
//! writer of it. The language-neutral definition lives in
//! `contracts/diagnostics/diagnostic-report.v1.schema.json`.
//!
//! Findings are a strict superset of Ferrum Anvil's `DiagnosticFinding`
//! (`contracts/schemas/DiagnosticFinding.schema.json` in ferrum-anvil): every
//! Anvil field is present with the same meaning and values, and Alloy adds
//! `supporting_observations` and `missing_evidence` as additional properties.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::open_enum::open_enum;

/// The schema identifier every report must carry in `schema`.
pub const SCHEMA_NAME: &str = "ferrum.diagnostic_report";
/// The only major version this reader accepts.
pub const SCHEMA_MAJOR: u32 = 1;
/// The newest minor version this reader was written against.
pub const SCHEMA_MINOR: u32 = 0;

/// A complete diagnostic report: collected observations plus derived findings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticReport {
    /// Always [`SCHEMA_NAME`].
    pub schema: String,
    /// `"<major>.<minor>"`.
    pub schema_version: String,
    /// Optional producer-assigned identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_id: Option<String>,
    /// RFC 3339 generation time. Informational only; never used by rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    /// How the report was assembled and whether its provenance was verified.
    pub collection: Collection,
    /// What the report is about.
    #[serde(default)]
    pub subject: Subject,
    /// Collected facts. Never derived conclusions.
    #[serde(default)]
    pub observations: Vec<Observation>,
    /// Derived conclusions. Readers recompute these; supplied findings are
    /// preserved but never used as evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// Namespaced producer extensions (keys should start with `x-`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
    /// Fields this reader does not recognize (for example from a newer minor
    /// version). Preserved and reported, never interpreted.
    #[serde(flatten)]
    pub unrecognized: BTreeMap<String, Value>,
}

impl DiagnosticReport {
    /// Creates an empty report for the current schema version.
    pub fn new(collection: Collection) -> Self {
        Self {
            schema: SCHEMA_NAME.to_owned(),
            schema_version: format!("{SCHEMA_MAJOR}.{SCHEMA_MINOR}"),
            report_id: None,
            generated_at: None,
            collection,
            subject: Subject::default(),
            observations: Vec::new(),
            findings: Vec::new(),
            extensions: BTreeMap::new(),
            unrecognized: BTreeMap::new(),
        }
    }

    /// Looks up an observation by id.
    pub fn observation(&self, id: &str) -> Option<&Observation> {
        self.observations.iter().find(|o| o.id == id)
    }
}

/// Collection provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    /// The component that assembled this report.
    pub collector: Producer,
    /// How the observations were gathered.
    pub method: CollectionMethod,
    /// Whether the collector authenticated the producers of the observations.
    /// A value read from a file is a claim, not proof; offline readers
    /// downgrade it (see [`crate::parse`]).
    pub verification: Verification,
    /// Free-form, bounded notes from the collector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The component that produced an observation or assembled a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    /// Producer family.
    pub kind: ProducerKind,
    /// Product or component name, e.g. `ferrum-edge`, `ferrum-alloy-telemetry`.
    pub name: String,
    /// Producer version when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Process or host instance, used to decide whether two wall-clock
    /// intervals share a clock. Absent means "unknown instance".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

/// What the report is about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// W3C trace id (32 lowercase hex characters) when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Correlation / request id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Logical service name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Route template (never a raw path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// One collected fact: a measurement or an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Unique within the report: `[A-Za-z0-9._:-]{1,64}`.
    pub id: String,
    /// Who measured or emitted it.
    pub producer: Producer,
    /// Measurement or event.
    pub kind: ObservationKind,
    /// Catalog name, e.g. `alloy.server.time_to_headers`. See [`crate::catalog`].
    pub name: String,
    /// Whether a value exists and, if not, why.
    pub availability: Availability,
    /// Numeric value for measurements. Absent unless `availability` is `measured`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Unit of `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
    /// Lifecycle boundaries of the measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<Boundaries>,
    /// Clock the value was measured with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockDomain>,
    /// Wall-clock interval, only comparable with intervals from the same
    /// producer instance. Never subtract intervals from different hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    /// Where on the request path the observation applies.
    pub scope: Scope,
    /// Trace linkage when the observation came from a span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SpanRef>,
    /// Bounded, redacted string attributes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
    /// Provenance of this observation.
    pub trust: Trust,
    /// Redacted pointer to the raw evidence, e.g. `otlp:span/<span_id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    /// Fields this reader does not recognize.
    #[serde(flatten)]
    pub unrecognized: BTreeMap<String, Value>,
}

impl Observation {
    /// The measured value converted to milliseconds, when this observation is
    /// usable duration evidence: its kind is `measurement`, its availability
    /// is `measured`, its unit is a time unit, and its value is finite and not
    /// negative both before and after conversion.
    ///
    /// Anything else yields `None`, so no timing rule uses it: an event, an
    /// unrecognized kind, unit, or availability, a negative value, or a value
    /// that overflows when converted. Invalid values stay in the report, where
    /// the contradiction rules still report them.
    pub fn duration_ms(&self) -> Option<f64> {
        if self.kind != ObservationKind::Measurement || self.availability != Availability::Measured
        {
            return None;
        }
        let value = self
            .value
            .filter(|value| value.is_finite() && *value >= 0.0)?;
        let milliseconds = match self.unit.as_ref()? {
            Unit::Microseconds => value / 1_000.0,
            Unit::Milliseconds => value,
            Unit::Seconds => value * 1_000.0,
            _ => return None,
        };
        // Adding zero turns a negative zero into zero.
        milliseconds.is_finite().then_some(milliseconds + 0.0)
    }

    /// Reads a string attribute.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

/// Measurement boundaries, named from the catalog (e.g. `alloy.middleware_entry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundaries {
    /// Where the measurement starts.
    pub start: String,
    /// Where the measurement ends.
    pub end: String,
}

/// Wall-clock interval in Unix nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    /// Start, Unix epoch nanoseconds.
    pub start_unix_nano: u64,
    /// End, Unix epoch nanoseconds.
    pub end_unix_nano: u64,
}

impl Interval {
    /// Returns `true` when `self` lies within `outer`, allowing `slack_nanos`
    /// of clock-reading jitter at either edge.
    pub fn within(&self, outer: &Interval, slack_nanos: u64) -> bool {
        self.start_unix_nano.saturating_add(slack_nanos) >= outer.start_unix_nano
            && self.end_unix_nano <= outer.end_unix_nano.saturating_add(slack_nanos)
            && self.start_unix_nano <= self.end_unix_nano
    }
}

/// Position on the request path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Connection leg.
    pub leg: Leg,
    /// Service name when the observation belongs to a service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Gateway name when the observation belongs to a gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// Upstream attempt index (1-based) when the producer recorded one.
    /// Absent means the attempt is unknown, not "first attempt".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Trace linkage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanRef {
    /// 32 lowercase hex characters.
    pub trace_id: String,
    /// 16 lowercase hex characters.
    pub span_id: String,
    /// Parent span id, when the span has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

/// A derived conclusion (Anvil `DiagnosticFinding` superset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable finding code, e.g. `alloy.service.operation_dominates`.
    pub code: String,
    /// Rule identifier.
    pub rule_id: String,
    /// Rule version; incremented whenever rule logic or wording semantics change.
    pub rule_version: u32,
    /// Short title.
    pub title: String,
    /// Explanation of what the evidence shows.
    pub explanation: String,
    /// Which leg the claim is about.
    pub scope: SourceScope,
    /// Strength of the evidence (not a probability).
    pub confidence: Confidence,
    /// Severity.
    pub severity: Severity,
    /// Evidence cited, in Anvil's `{source, key, value}` form.
    pub evidence: Vec<Evidence>,
    /// Other explanations consistent with the same evidence.
    pub alternatives: Vec<String>,
    /// What this evidence does not establish.
    pub does_not_prove: Vec<String>,
    /// Suggested actions, each with an owner.
    pub remediation: Vec<Remediation>,
    /// Who can act.
    pub owner: Owner,
    /// Safe next checks that would confirm or refute the claim.
    pub confirm_with: Vec<String>,
    /// Alloy extension: ids of observations supporting the finding.
    #[serde(default)]
    pub supporting_observations: Vec<String>,
    /// Alloy extension: evidence that would be needed for a stronger claim.
    #[serde(default)]
    pub missing_evidence: Vec<String>,
}

/// One cited piece of evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Evidence source family.
    pub source: EvidenceSource,
    /// Machine key.
    pub key: String,
    /// Redacted value as observed.
    pub value: String,
    /// Attempt index when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// A suggested action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remediation {
    /// What to do. Plain text; never an executable command.
    pub text: String,
    /// Who can do it.
    pub owner: Owner,
}

open_enum! {
    /// How observations were gathered.
    pub enum CollectionMethod {
        /// Exported live by the producing process.
        LiveExport => "live_export",
        /// Converted from an OTLP/JSON trace export file.
        OtlpFileImport => "otlp_file_import",
        /// Assembled offline from supplied data.
        OfflineImport => "offline_import",
        /// A test fixture.
        Fixture => "fixture",
    }
}

open_enum! {
    /// Provenance verification state.
    pub enum Verification {
        /// The collector authenticated each producer.
        Verified => "verified",
        /// Producer identity was not checked.
        Unverified => "unverified",
    }
}

open_enum! {
    /// Producer family.
    pub enum ProducerKind {
        /// Ferrum Edge gateway.
        Edge => "edge",
        /// A Ferrum Alloy service.
        Alloy => "alloy",
        /// An API client such as Ferrum Anvil.
        Client => "client",
        /// A telemetry collector.
        Collector => "collector",
        /// Supplied by a person.
        User => "user",
    }
}

open_enum! {
    /// Observation kind.
    pub enum ObservationKind {
        /// A numeric measurement with boundaries.
        Measurement => "measurement",
        /// A discrete event with attributes.
        Event => "event",
    }
}

open_enum! {
    /// Whether a value exists and why not. Unknown is not zero.
    pub enum Availability {
        /// A value was measured.
        Measured => "measured",
        /// The producer supports the measurement but has no value for this request.
        Unavailable => "unavailable",
        /// The producer version cannot measure this.
        Unsupported => "unsupported",
        /// The measurement does not apply (e.g. connection setup on a reused connection).
        NotApplicable => "not_applicable",
        /// The request was not sampled.
        NotSampled => "not_sampled",
        /// Export has not completed yet.
        ExportPending => "export_pending",
        /// Telemetry was dropped (queue full, export failure).
        Dropped => "dropped",
        /// Nothing is known about this measurement.
        Unknown => "unknown",
    }
}

open_enum! {
    /// Unit of a measurement value.
    pub enum Unit {
        /// Microseconds.
        Microseconds => "us",
        /// Milliseconds.
        Milliseconds => "ms",
        /// Seconds.
        Seconds => "s",
        /// Bytes.
        Bytes => "bytes",
        /// A count.
        Count => "count",
    }
}

open_enum! {
    /// Clock used to measure a value.
    pub enum ClockDomain {
        /// A local monotonic clock. Durations are reliable; instants are not
        /// comparable across processes.
        MonotonicLocal => "monotonic_local",
        /// Wall-clock time. Subject to adjustment; never subtract across hosts.
        WallClock => "wall_clock",
    }
}

open_enum! {
    /// Connection leg.
    pub enum Leg {
        /// Client to gateway.
        ClientToGateway => "client_to_gateway",
        /// Inside the gateway.
        Gateway => "gateway",
        /// Gateway to service (the upstream exchange as a whole).
        GatewayToService => "gateway_to_service",
        /// Inside the service.
        Service => "service",
        /// Service to a dependency (database, outbound HTTP).
        ServiceToDependency => "service_to_dependency",
        /// Unknown leg.
        Unknown => "unknown",
    }
}

open_enum! {
    /// Observation provenance.
    pub enum Trust {
        /// Collected from an authenticated producer over an authenticated path.
        Verified => "verified",
        /// Attributed to a producer, but the attribution was not checked.
        Unverified => "unverified",
        /// Supplied by a person.
        UserAsserted => "user_asserted",
    }
}

open_enum! {
    /// Finding scope (Anvil `SourceScope`).
    pub enum SourceScope {
        /// The client itself.
        LocalClient => "local_client",
        /// Client to the peer it connected to.
        ClientToPeer => "client_to_peer",
        /// The gateway's own admission and policy decisions.
        GatewayAdmission => "gateway_admission",
        /// Gateway to its upstream service.
        GatewayToUpstream => "gateway_to_upstream",
        /// Delivery of an already-started response.
        ResponseDelivery => "response_delivery",
        /// A forward proxy.
        ForwardProxy => "forward_proxy",
        /// The upstream application.
        UpstreamApplication => "upstream_application",
        /// Unknown.
        Unknown => "unknown",
    }
}

open_enum! {
    /// Evidence strength (Anvil `Confidence`). Never a probability.
    pub enum Confidence {
        /// Directly observed by an authenticated producer.
        Confirmed => "confirmed",
        /// Strong but indirect, unverified, or spoofable evidence.
        Likely => "likely",
        /// The evidence cannot separate the candidate causes.
        Unknown => "unknown",
        /// The evidence contradicts itself.
        ConflictingEvidence => "conflicting_evidence",
    }
}

open_enum! {
    /// Finding severity (Anvil `Severity`).
    pub enum Severity {
        /// Informational.
        Info => "info",
        /// Worth attention.
        Warning => "warning",
        /// A failure.
        Error => "error",
    }
}

open_enum! {
    /// Who can act (Anvil `Owner`).
    pub enum Owner {
        /// The caller.
        Caller => "caller",
        /// The gateway operator.
        GatewayOperator => "gateway_operator",
        /// The API/service owner.
        ApiOwner => "api_owner",
        /// The network administrator.
        NetworkAdministrator => "network_administrator",
        /// The identity provider.
        IdentityProvider => "identity_provider",
        /// Unknown.
        Unknown => "unknown",
    }
}

open_enum! {
    /// Evidence source (Anvil `EvidenceSource` plus two proposed values).
    pub enum EvidenceSource {
        /// Local validation.
        LocalValidation => "local_validation",
        /// Native transport measurement.
        NativeTransport => "native_transport",
        /// TLS verifier result.
        TlsVerifier => "tls_verifier",
        /// HTTP status code.
        HttpStatus => "http_status",
        /// HTTP header.
        HttpHeader => "http_header",
        /// HTTP trailer.
        HttpTrailer => "http_trailer",
        /// gRPC status.
        GrpcStatus => "grpc_status",
        /// WebSocket close.
        WebSocketClose => "web_socket_close",
        /// Response body completion state.
        BodyCompletion => "body_completion",
        /// Parsed body content (weak).
        BodyContent => "body_content",
        /// Configuration.
        Configuration => "configuration",
        /// A user assertion.
        Assertion => "assertion",
        /// Ferrum marker from a trusted destination.
        FerrumMarkerTrusted => "ferrum_marker_trusted",
        /// Ferrum-like marker from an unverified destination.
        FerrumMarkerUnverified => "ferrum_marker_unverified",
        /// Authenticated gateway diagnostic detail (versioned contract).
        GatewayDetail => "gateway_detail",
        /// PROPOSED: gateway-produced telemetry (spans, access logs).
        GatewayTelemetry => "gateway_telemetry",
        /// PROPOSED: service-produced telemetry (Alloy spans and measurements).
        ServiceTelemetry => "service_telemetry",
    }
}
