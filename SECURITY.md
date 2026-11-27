# Security policy

## Reporting a vulnerability

Please do not report a security problem in a public issue, discussion, or pull
request, and don't disclose it publicly before a fix is available.

Report it privately:

1. Through GitHub private vulnerability reporting: on this repository's
   **Security** tab, choose **Report a vulnerability**
   (<https://github.com/ferrum-edge/ferrum-alloy/security/advisories/new>),
   where the button is shown.
2. Otherwise, by email to **contact@ferrumedge.com** with "Security" in the
   subject line.

Include the Alloy version or commit, the crates and features you enable, the
relevant configuration (trust, TLS, JWT, diagnostics), steps to reproduce, and
the impact you believe the issue has. Don't include live credentials, private
keys, or tokens.

We aim to acknowledge a report within **5 business days** and keep you
informed through triage and the fix. Fixes are published as GitHub Security
Advisories on this repository. We credit reporters unless you ask us not to.

## Supported versions

Alloy is released from Git tags (the first is `v0.1.0`) and is not published
to crates.io or any other registry. Before 1.0.0, security fixes land on
`main` and ship in the next tagged release; earlier 0.x tags receive no
backports.

From 1.0.0 onward, the [stability policy](docs/release.md#stability-policy)
sets the support window:

- The latest 1.x release is supported. A security fix ships in a new 1.x
  release, a patch release when the fix changes no covered surface. Earlier
  1.x releases receive no backports, because upgrading within 1.x is
  compatible.
- After 2.0.0 is released, the last 1.x minor line receives security fixes
  for six months from the 2.0.0 release date, as patch releases.
- 0.x tags receive no fixes once 1.0.0 is released.
- The [security exception](docs/release.md#security-exception) takes
  precedence over all of the above and over the rest of the stability policy:
  a fix that cannot be made without a breaking change (including an upgrade of
  a public dependency across an incompatible version) ships in a new 1.x minor
  release, also during the six months after 2.0.0, when that release becomes
  the last 1.x minor line for the rest of the same window. Its advisory and
  release notes state what breaks and how to migrate.

## Security model

What Alloy trusts, and what it deliberately does not protect against, is
documented in [docs/security.md](docs/security.md). A report that a
documented non-goal can be exploited is still welcome. Please say which
property you expected to hold.
