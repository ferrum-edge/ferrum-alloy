# ADR 0009: Authenticated Edge diagnostic-reference lookup

**Status:** Accepted (2026-10-04), issue #115. Extends ADR 0005.

## Context

Edge v0.9.9 implements `GET /diagnostics/v1/refs/{ref}`. A reference header
alone is spoofable. A service's `diagnose --url` report does not observe the
client response and cannot supply its reference, status, or gateway token.
Serialized `verified` or `authenticated` claims cannot establish provenance.

## Decision

The operator explicitly selects `--edge-admin-url` and supplies a separate
`--edge-observation` JSON capture from a trusted client or trusted recording
system. The capture names `reference`, `namespace`, `status`, `gateway_error`
(null explicitly means no header), `protocol`, `request_started_at`, and
`response_received_at`. It contains no trust switch. Its integrity and the
association of the chosen admin listener with that gateway are operator trust
assumptions. Service reports and their observation flags cannot substitute for
this input. Optional accompanying reports remain unverified and are not linked
to this response merely by being analyzed alongside it.

The CLI reads only `FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN` for the Edge credential.
Edge verifies the JWT, `diagnostics:read` scope, and namespace (`ns`) claim;
Alloy never trusts a locally decoded JWT. The explicit expected namespace must
also equal the returned namespace. An admin role alone grants no lookup right.

Before building the lookup target, validate the `fd1_`/`fd2_` grammar and URL.
Reject userinfo, queries, fragments, unsupported schemes, and plaintext DNS
names (including localhost). Use verified HTTPS with platform roots, or HTTP
directly to a canonical literal loopback IP. Loopback trusts the local host and
the operator-selected process; it is not cryptographic server identity. Disable
environment proxies, follow no redirects, make one GET with at most two seconds
to connect and five seconds overall (or the shorter requested timeout), and
read at most 64 KiB plus one overflow byte. Refusals and failures disclose no
response bodies, credentials, URL, or arbitrary server error text. Do not route
automatically using replica hints or retry a redirect.

The serde-only diagnostics crate checks the released schema's interpreted
fields and deterministically binds the record to the exact reference, embedded
replica (`fd2_` requires its matching `replica_id`; `fd1_` permits none), namespace,
HTTP status, token including absence, protocol, and creation time inside the
explicit request window. The window is at most 300 seconds, with no implicit
clock-skew expansion; the expiry must not precede its end. RFC 3339 offsets and
up to nanosecond fractions are supported; leap seconds and unknown local offsets
fail closed. These times are identity checks, never latency measurements.

Pure binding returns a non-deserializable `BoundRecord` whose finding is always
`likely`. Only the CLI's private `AuthenticatedLookup`, constructed by a
successful GET on the allowed channel followed by binding, can promote the new
`alloy.r008` finding to `confirmed`. It confirms that this selected Edge admin
listener recorded the supplied response facts and cited classifications. It
does not authenticate any other report or raise any existing rule's confidence.

Unknown schema versions, schema enums (including public tokens), invalid shapes,
and mismatches are refused. Unknown granular classes remain readable but cap
the record finding at `likely`. Rejection labels and TLS reasons are extensible
strings in the shared schema: this reader conservatively caps records containing
them at `likely`. Additive fields are ignored, including forged authentication
flags. Export only response facts and recognized classification tokens; omit
raw records, operator proxy/backend configuration, extension fields, plugin
labels, and TLS reasons. Scrub the credential from rendered and saved output.
Offline parsing always drops supplied findings and downgrades provenance.

## Consequences

This path proves the gateway's recorded view under the above trust assumptions.
It does not prove capture integrity, a service crash, backend health, packet
loss, independent causal truth of an error class, timing attribution, or that
earlier retry attempts never reached a backend. A duration bucket is not a
precise duration. A record with no terminal detail proves only matching response
metadata. A `404` does not distinguish expiration, eviction, disabled retention,
wrong namespace, wrong replica, or an unknown reference. There is no automatic
data-plane probing or new Edge API. Network I/O stays in the CLI, and the
diagnostics crate gains no dependencies.

The diagnostic-ref schema and ten valid/invalid fixtures were first vendored byte for
byte from `contracts-edge-0.9.9-r2` (commit
`591c73a3f965fdab440c3a76b2707accdf491ba5`), with SHA-256 values in PIN. Hosted
CI runs recording HTTP fixtures and schema/vocabulary drift tests. This path did
not change the Edge support policy or MSRV. The 2026-10-04 adoption candidate now
pins the identical schema/fixture bytes from `contracts-edge-0.9.11` at
`390edbd5b2485af0988e02f7827fde778d76ae0a`; the latest-plus-previous Edge window
rolls to v0.9.11/v0.9.10 and requires fresh hosted pairing and HTTP fixture checks.
