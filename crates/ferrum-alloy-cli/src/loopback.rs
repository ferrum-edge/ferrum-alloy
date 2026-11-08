//! The plain-http rule for credentials, shared by `diagnose --url` (the
//! service report) and `diagnose --edge-admin-url` (the Edge lookup).

use std::net::IpAddr;

use reqwest::Url;

/// Whether `url`, parsed from `original`, names this host by a canonical
/// literal loopback IP: a dotted-quad address in `127.0.0.0/8`, or `[::1]`.
///
/// DNS names, `localhost` included, never qualify: the resolver decides
/// where they connect, and a credential sent over plain http must not leave
/// this host. The URL parser also normalizes shorthand, octal, hex and
/// integer IPv4 hosts (`127.1`, `0177.0.0.1`, `0x7f.1`, `2130706433`) and
/// long IPv6 forms (`[0:0:0:0:0:0:0:1]`) to a loopback address. Those are
/// refused too: the original authority must spell the canonical literal
/// itself, so what the operator typed is what the check approved.
pub(crate) fn is_canonical_loopback(original: &str, url: &Url) -> bool {
    // `host_str` is the parser's canonical form: dotted-quad IPv4, or
    // bracketed compressed IPv6. A domain never parses as an address.
    let Some(host) = url.host_str() else {
        return false;
    };
    let address = host.trim_start_matches('[').trim_end_matches(']');
    if !address.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return false;
    }
    let authority = original
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let authority = authority.split('/').next().unwrap_or_default();
    authority
        .strip_prefix(host)
        .is_some_and(|port| port.is_empty() || port.starts_with(':'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::is_canonical_loopback;

    fn check(original: &str) -> bool {
        is_canonical_loopback(original, &reqwest::Url::parse(original).unwrap())
    }

    #[test]
    fn only_canonical_literal_loopback_addresses_qualify() {
        for accepted in [
            "http://127.0.0.1",
            "http://127.0.0.1:9090",
            "http://127.0.0.1:9090/base/",
            "http://127.0.0.2:9090",
            "http://[::1]",
            "http://[::1]:9090/",
        ] {
            assert!(check(accepted), "{accepted}");
        }
        for refused in [
            "http://localhost:9090",
            "http://LocalHost:9090",
            "http://api.localhost:9090",
            "http://192.0.2.1:9090",
            "http://10.0.0.8:9090",
            "http://127.1:9090",
            "http://127.0.1:9090",
            "http://0177.0.0.1:9090",
            "http://0x7f.0.0.1:9090",
            "http://0x7f000001:9090",
            "http://2130706433:9090",
            "http://[0:0:0:0:0:0:0:1]:9090",
            "http://[::ffff:127.0.0.1]:9090",
            "http:127.0.0.1:9090",
        ] {
            assert!(!check(refused), "{refused}");
        }
    }
}
