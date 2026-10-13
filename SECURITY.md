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

Alloy is pre-release and not yet published to crates.io. Security fixes land
on `main`; there is no release line to backport to.

## Security model

What Alloy trusts, and what it deliberately does not protect against, is
documented in [docs/security.md](docs/security.md). A report that a
documented non-goal can be exploited is still welcome. Please say which
property you expected to hold.
