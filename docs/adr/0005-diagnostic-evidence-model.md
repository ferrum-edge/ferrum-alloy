# ADR 0005: Versioned, language-neutral evidence with deterministic rules

**Status:** Accepted (2026-09-26)

## Context

Ferrum Anvil already defines a `DiagnosticFinding` shape: confidence `confirmed`/`likely`/`unknown`/`conflicting_evidence`, plus scope, owner, `does_not_prove`, and `confirm_with`. Its audited Edge v0.9.7 catalog shows that `X-Gateway-Error` is spoofable on some paths. Alloy must explain Edge-plus-service timing without converting missing or unverifiable evidence into confident claims.

## Decision

- Schema `ferrum.diagnostic_report` v1 (JSON, with JSON Schema in `contracts/diagnostics/`) separates **observations** (collected facts) from **findings** (derived).
- Each observation records: id, producer (kind, name, version, instance), kind, catalog name, availability, value and unit, boundaries, clock domain, same-producer wall-clock interval, scope (leg, service, gateway, attempt), span linkage, bounded attributes, trust, and a redacted evidence reference.
- Findings are a strict superset of Anvil's `DiagnosticFinding`, adding `supporting_observations` and `missing_evidence`. Two evidence sources, `gateway_telemetry` and `service_telemetry`, are proposed additions for Anvil.
- Unknown enum values are preserved as unrecognized, never mapped onto known values. Unknown fields are preserved and reported. Unsupported major versions are rejected.
- File input is untrusted and bounded. A `verified` claim in a file is downgraded, so offline input never yields `confirmed`.
- Rules are deterministic Rust with no network access and no AI service. Each is versioned (`alloy.r00N`), cites observation ids, and lists what it does not prove. Tests assert that forbidden explanations never appear: packet loss from a missing span, DNS failure from `connection_failure`, a slow handler from a long response, a crash from a reset.

## Consequences

- Anvil integration is PROPOSED until Anvil's importer is tested with these reports.
- `confirmed` gateway attribution needs authenticated gateway detail (Anvil G01 / ferrum-edge#5767), which does not exist yet.
