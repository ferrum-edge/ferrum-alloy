//! Configuration precedence, strictness, redaction, and validation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::path::PathBuf;

use ferrum_alloy::config::{
    AlloyConfig, ConfigError, ENV_VARS, EdgeMode, Overrides, Secret, load_from,
};

fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

fn write(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    path
}

const NO_FEATURES: &[&str] = &[];

#[test]
fn defaults_are_safe_and_valid() {
    let (config, sources) = load_from(None, env(&[]), &Overrides::default()).unwrap();
    assert!(
        config.server.bind.ip().is_loopback(),
        "listeners default to loopback"
    );
    assert!(config.management.bind.ip().is_loopback());
    assert!(!config.cors.enabled);
    assert!(!config.compression.enabled);
    assert!(!config.otlp.enabled);
    assert!(
        !config.openapi.public,
        "documentation is not public by default"
    );
    assert_eq!(config.edge.mode, EdgeMode::Standalone);
    assert!(sources.file.is_none());
    config.validate(NO_FEATURES).unwrap();
}

#[test]
fn precedence_is_override_then_env_then_file_then_default() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        &dir,
        "alloy.toml",
        r#"
        [server]
        bind = "127.0.0.1:7000"
        request_timeout_ms = 1111
        max_connections = 42

        [service]
        environment = "staging"
        "#,
    );
    let mut overrides = Overrides::default();
    overrides.set(
        &["server", "bind"],
        toml::Value::String("127.0.0.1:9999".into()),
    );
    let (config, sources) = load_from(
        Some(&file),
        env(&[
            ("FERRUM_ALLOY_BIND", "127.0.0.1:8888"),
            ("FERRUM_ALLOY_REQUEST_TIMEOUT_MS", "2222"),
            ("UNRELATED_VAR", "ignored"),
        ]),
        &overrides,
    )
    .unwrap();
    assert_eq!(
        config.server.bind.to_string(),
        "127.0.0.1:9999",
        "override beats env"
    );
    assert_eq!(config.server.request_timeout_ms, 2222, "env beats file");
    assert_eq!(config.server.max_connections, 42, "file beats default");
    assert_eq!(config.service.environment, "staging");
    assert_eq!(config.server.header_read_timeout_ms, 10_000, "default");
    assert_eq!(sources.file.as_deref(), Some(file.as_path()));
    assert_eq!(
        sources.env,
        vec!["FERRUM_ALLOY_BIND", "FERRUM_ALLOY_REQUEST_TIMEOUT_MS"]
    );
    assert_eq!(sources.overrides, vec!["server.bind"]);
}

#[test]
fn config_file_can_be_named_by_environment() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(&dir, "x.toml", "[server]\nmax_connections = 7\n");
    let (config, _) = load_from(
        None,
        env(&[("FERRUM_ALLOY_CONFIG", file.to_str().unwrap())]),
        &Overrides::default(),
    )
    .unwrap();
    assert_eq!(config.server.max_connections, 7);
}

#[test]
fn unknown_keys_and_variables_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let typo = write(&dir, "typo.toml", "[server]\nbnd = \"127.0.0.1:1\"\n");
    let error = load_from(Some(&typo), env(&[]), &Overrides::default()).unwrap_err();
    assert!(
        matches!(&error, ConfigError::Schema(m) if m.contains("bnd")),
        "{error}"
    );

    let section = write(&dir, "section.toml", "[serverr]\n");
    assert!(load_from(Some(&section), env(&[]), &Overrides::default()).is_err());

    let error = load_from(
        None,
        env(&[("FERRUM_ALLOY_BINDD", "x")]),
        &Overrides::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&error, ConfigError::Env { name, .. } if name == "FERRUM_ALLOY_BINDD"),
        "{error}"
    );
}

#[test]
fn invalid_values_are_rejected_not_defaulted() {
    for (name, value) in [
        ("FERRUM_ALLOY_REQUEST_TIMEOUT_MS", "-5"),
        ("FERRUM_ALLOY_REQUEST_TIMEOUT_MS", "soon"),
        ("FERRUM_ALLOY_MANAGEMENT_ENABLED", "yes please"),
        ("FERRUM_ALLOY_OTLP_SAMPLING_RATIO", "NaN"),
    ] {
        assert!(
            load_from(None, env(&[(name, value)]), &Overrides::default()).is_err(),
            "{name}={value}"
        );
    }
    let error = load_from(
        None,
        env(&[("FERRUM_ALLOY_BIND", "localhost:80")]),
        &Overrides::default(),
    )
    .unwrap_err();
    assert!(matches!(error, ConfigError::Schema(_)));
    let dir = tempfile::tempdir().unwrap();
    let broken = write(&dir, "broken.toml", "[server\n");
    assert!(matches!(
        load_from(Some(&broken), env(&[]), &Overrides::default()).unwrap_err(),
        ConfigError::Syntax { .. }
    ));
    let missing = dir.path().join("missing.toml");
    assert!(matches!(
        load_from(Some(&missing), env(&[]), &Overrides::default()).unwrap_err(),
        ConfigError::Read { .. }
    ));
}

#[test]
fn syntax_errors_report_the_location_without_the_source_line() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text, secret) in [
        (
            "token.toml",
            "[management]\ntoken = \"SYNTHETIC-TOKEN-0123456789ABCDEF\" extra\n",
            "SYNTHETIC-TOKEN",
        ),
        (
            "database.toml",
            "[database]\nurl = \"postgres://u:SYNTHETIC-PASSWORD@db/x\" extra\n",
            "SYNTHETIC-PASSWORD",
        ),
    ] {
        let path = write(&dir, name, text);
        let error = load_from(Some(&path), env(&[]), &Overrides::default()).unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains(secret), "{rendered}");
        assert!(!format!("{error:?}").contains(secret), "{error:?}");
        assert!(rendered.contains(name), "names the file: {rendered}");
        assert!(rendered.contains("at line 2, column "), "{rendered}");
        let ConfigError::Syntax { line, column, .. } = &error else {
            panic!("expected a syntax error, got {error}");
        };
        assert_eq!(*line, Some(2));
        assert!(column.is_some_and(|c| c > 1), "{column:?}");
    }
}

#[test]
fn schema_errors_name_keys_but_never_values() {
    let dir = tempfile::tempdir().unwrap();
    for (text, secret, kind, key) in [
        (
            "[management]\ntoken = 4242424242\n",
            "4242424242",
            "invalid type",
            "management.token",
        ),
        (
            "[server]\nmax_connections = \"SYNTH-S1\"\n",
            "SYNTH-S1",
            "invalid type",
            "server.max_connections",
        ),
        (
            "[server.tls]\ncert_path = \"c\"\nkey_path = \"k\"\nclient_auth = \"SYNTH-S2\"\n",
            "SYNTH-S2",
            "unknown variant",
            "server.tls.client_auth",
        ),
        (
            "[database]\n\"postgres://u:SYNTH-S3@db/x\" = 1\n",
            "SYNTH-S3",
            "unknown field",
            "database",
        ),
    ] {
        let path = write(&dir, "schema.toml", text);
        let error = load_from(Some(&path), env(&[]), &Overrides::default()).unwrap_err();
        let rendered = error.to_string();
        assert!(matches!(error, ConfigError::Schema(_)), "{rendered}");
        assert!(!rendered.contains(secret), "{rendered}");
        assert!(!format!("{error:?}").contains(secret), "{error:?}");
        assert!(rendered.contains(kind), "{rendered}");
        assert!(rendered.contains(&format!("`{key}`")), "{rendered}");
    }
    // A long bare key mixing letters and digits looks like a token.
    let token = "synth4token0123456789abcdefghijklmnop";
    let path = write(&dir, "token.toml", &format!("[database]\n{token} = 1\n"));
    let error = load_from(Some(&path), env(&[]), &Overrides::default()).unwrap_err();
    let rendered = error.to_string();
    assert!(!rendered.contains(token), "{rendered}");
    assert!(rendered.contains("(key redacted)"), "{rendered}");
    // The expected type or variants still come through.
    let path = write(&dir, "variant.toml", "[logging]\nformat = \"xml\"\n");
    let error = load_from(Some(&path), env(&[]), &Overrides::default()).unwrap_err();
    assert!(error.to_string().contains("expected one of"), "{error}");
}

#[test]
fn jwt_key_lifetime_settings_are_validated() {
    let jwt = [
        ("FERRUM_ALLOY_JWT_ISSUER", "https://issuer.test"),
        ("FERRUM_ALLOY_JWT_AUDIENCES", "orders-api"),
        ("FERRUM_ALLOY_JWT_JWKS_URL", "https://issuer.test/jwks"),
    ];
    let (config, _) = load_from(None, env(&jwt), &Overrides::default()).unwrap();
    let settings = config.auth.jwt.as_ref().unwrap();
    assert_eq!(settings.jwks_max_age_ms, 300_000, "bounded by default");
    assert_eq!(settings.jwks_max_stale_ms, 300_000);
    config.validate(&["jwt"]).unwrap();

    let mut vars = jwt.to_vec();
    vars.push(("FERRUM_ALLOY_JWT_JWKS_MAX_AGE_MS", "1000"));
    vars.push(("FERRUM_ALLOY_JWT_JWKS_MAX_STALE_MS", "0"));
    let (config, _) = load_from(None, env(&vars), &Overrides::default()).unwrap();
    let settings = config.auth.jwt.as_ref().unwrap();
    assert_eq!(settings.jwks_max_age_ms, 1_000);
    assert_eq!(settings.jwks_max_stale_ms, 0);
    let error = config.validate(&["jwt"]).unwrap_err().to_string();
    assert!(error.contains("must not exceed"), "{error}");

    let mut zero = jwt.to_vec();
    zero.push(("FERRUM_ALLOY_JWT_JWKS_MAX_AGE_MS", "0"));
    let (config, _) = load_from(None, env(&zero), &Overrides::default()).unwrap();
    let error = config.validate(&["jwt"]).unwrap_err().to_string();
    assert!(error.contains("greater than zero"), "{error}");

    // Both bounds are capped at 24 hours.
    let mut long = jwt.to_vec();
    long.push(("FERRUM_ALLOY_JWT_JWKS_MAX_AGE_MS", "86400001"));
    long.push(("FERRUM_ALLOY_JWT_JWKS_MAX_STALE_MS", "86400001"));
    let (config, _) = load_from(None, env(&long), &Overrides::default()).unwrap();
    let error = config.validate(&["jwt"]).unwrap_err().to_string();
    assert!(
        error.contains("jwks_max_age_ms must not exceed 24 hours"),
        "{error}"
    );
    assert!(
        error.contains("jwks_max_stale_ms must not exceed 24 hours"),
        "{error}"
    );
    let mut day = jwt.to_vec();
    day.push(("FERRUM_ALLOY_JWT_JWKS_MAX_AGE_MS", "86400000"));
    day.push(("FERRUM_ALLOY_JWT_JWKS_MAX_STALE_MS", "86400000"));
    let (config, _) = load_from(None, env(&day), &Overrides::default()).unwrap();
    config.validate(&["jwt"]).unwrap();
}

#[test]
fn management_rate_limits_are_validated() {
    let (config, _) = load_from(None, env(&[]), &Overrides::default()).unwrap();
    let limit = &config.management.rate_limit;
    assert!(limit.enabled, "on by default");
    assert_eq!((limit.requests_per_second, limit.burst), (10, 20));
    assert_eq!(limit.max_clients, 1_024);
    assert_eq!(limit.ipv6_prefix_len, 64);
    assert!(limit.exempt_networks.is_empty());
    config.validate(NO_FEATURES).unwrap();

    let bad = [
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_BURST", "5"),
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_PROBE_BURST", "0"),
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_MAX_CLIENTS", "65537"),
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_IPV6_PREFIX_LEN", "32"),
        (
            "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_EXEMPT_NETWORKS",
            "10.0.0.0/8, ::/0",
        ),
    ];
    let (config, _) = load_from(None, env(&bad), &Overrides::default()).unwrap();
    let limit = &config.management.rate_limit;
    assert_eq!(limit.burst, 5);
    assert_eq!(limit.exempt_networks.len(), 2);
    let error = config.validate(NO_FEATURES).unwrap_err().to_string();
    for expected in [
        "probe_burst must be greater than zero",
        "management.rate_limit.max_clients must be within 1..=65536",
        "management.rate_limit.ipv6_prefix_len must be within 48..=128",
        "management.rate_limit.exempt_networks must not contain",
    ] {
        assert!(error.contains(expected), "{expected}: {error}");
    }

    let vars = [(
        "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_EXEMPT_NETWORKS",
        "::ffff:10.0.0.0/104",
    )];
    let (config, _) = load_from(None, env(&vars), &Overrides::default()).unwrap();
    let error = config.validate(NO_FEATURES).unwrap_err().to_string();
    assert!(
        error.contains("must use IPv4 CIDRs instead of IPv4-mapped IPv6 CIDRs"),
        "{error}"
    );

    // The table must hold every client the listener admits at once.
    let vars = [
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_GLOBAL_BURST", "300"),
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_MAX_CLIENTS", "299"),
    ];
    let (config, _) = load_from(None, env(&vars), &Overrides::default()).unwrap();
    let error = config.validate(NO_FEATURES).unwrap_err().to_string();
    assert!(
        error.contains("max_clients must be at least management.rate_limit.global_burst"),
        "{error}"
    );
    let vars = [
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_IPV6_PREFIX_LEN", "56"),
        ("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_EXEMPT_NETWORKS", ""),
    ];
    let (config, _) = load_from(None, env(&vars), &Overrides::default()).unwrap();
    let limit = &config.management.rate_limit;
    assert_eq!(limit.ipv6_prefix_len, 56);
    assert!(
        limit.exempt_networks.is_empty(),
        "exemptions can be removed"
    );
    config.validate(NO_FEATURES).unwrap();

    // Limits that are not enforced are not checked.
    let mut off = bad.to_vec();
    off.push(("FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_ENABLED", "false"));
    let (config, _) = load_from(None, env(&off), &Overrides::default()).unwrap();
    config.validate(NO_FEATURES).unwrap();
}

#[test]
fn lists_and_secret_files_are_supported() {
    let dir = tempfile::tempdir().unwrap();
    let secret = write(&dir, "token", "0123456789abcdef0123456789abcdef\n");
    let (config, _) = load_from(
        None,
        env(&[
            ("FERRUM_ALLOY_TRUSTED_NETWORKS", "10.0.0.0/8, 127.0.0.1/32,"),
            (
                "FERRUM_ALLOY_MANAGEMENT_TOKEN_FILE",
                secret.to_str().unwrap(),
            ),
        ]),
        &Overrides::default(),
    )
    .unwrap();
    assert_eq!(config.trust.networks.len(), 2);
    assert_eq!(
        config.management.token.as_ref().map(Secret::expose),
        Some("0123456789abcdef0123456789abcdef"),
        "trailing newline trimmed"
    );
    let error = load_from(
        None,
        env(&[
            ("FERRUM_ALLOY_MANAGEMENT_TOKEN", "a"),
            (
                "FERRUM_ALLOY_MANAGEMENT_TOKEN_FILE",
                secret.to_str().unwrap(),
            ),
        ]),
        &Overrides::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("set only one"), "{error}");
    // `_FILE` exists only for secrets.
    assert!(
        load_from(
            None,
            env(&[("FERRUM_ALLOY_BIND_FILE", "/x")]),
            &Overrides::default()
        )
        .is_err()
    );
}

#[test]
fn secrets_are_never_printed() {
    let mut config = AlloyConfig::default();
    config.management.token = Some(Secret::new("super-secret-token-value-1234567890"));
    config.database.url = Some(Secret::new("postgres://user:hunter2@db/orders"));
    let rendered = config.redacted_toml();
    assert!(!rendered.contains("super-secret"), "{rendered}");
    assert!(!rendered.contains("hunter2"), "{rendered}");
    assert!(rendered.contains("<redacted>"));
    let debug = format!("{config:?}");
    assert!(!debug.contains("hunter2") && !debug.contains("super-secret"));
}

#[test]
fn unsafe_combinations_fail_validation() {
    let check = |mutate: &dyn Fn(&mut AlloyConfig), expect: &str| {
        let mut config = AlloyConfig::default();
        mutate(&mut config);
        let error = config.validate(NO_FEATURES).unwrap_err().to_string();
        assert!(error.contains(expect), "expected {expect:?} in {error}");
    };
    check(
        &|c| c.management.bind = "0.0.0.0:9090".parse().unwrap(),
        "management.bind",
    );
    check(
        &|c| c.management.token = Some(Secret::new("short")),
        "at least 32",
    );
    check(&|c| c.management.bind = c.server.bind, "must differ");
    check(&|c| c.otlp.enabled = true, "`otel` feature");
    check(
        &|c| c.edge.mode = EdgeMode::GatewayPreferred,
        "`edge` feature",
    );
    check(
        &|c| c.database.url = Some(Secret::new("postgres://x")),
        "`postgres` feature",
    );
    check(
        &|c| c.server.request_body_limit_bytes = 0,
        "request_body_limit_bytes",
    );
    check(
        &|c| c.health.readiness_path = "readyz".into(),
        "health.readiness_path",
    );
    check(
        &|c| {
            c.cors.enabled = true;
            c.cors.allowed_origins = vec!["*".into()];
            c.cors.allow_credentials = true;
        },
        "allow_credentials",
    );
    check(
        &|c| c.trust.networks = vec!["0.0.0.0/0".parse().unwrap()],
        "every address",
    );
}

#[test]
fn gateway_modes_require_verified_identities() {
    let features = &["edge"];
    let mut config = AlloyConfig::default();
    config.edge.mode = EdgeMode::GatewayRequired;
    config.trust.networks = vec!["10.0.0.0/8".parse().unwrap()];
    let error = config.validate(features).unwrap_err().to_string();
    assert!(error.contains("trust.identities"), "{error}");
    config.trust.identities = vec!["spiffe://ferrum.test/ns/edge/sa/gateway".into()];
    config.validate(features).unwrap();

    let mut config = AlloyConfig::default();
    config.edge.accept_consumer_identity = true;
    config.trust.networks = vec!["10.0.0.0/8".parse().unwrap()];
    assert!(
        config
            .validate(features)
            .unwrap_err()
            .to_string()
            .contains("network trust")
    );
}

#[test]
fn warnings_flag_risky_but_valid_choices() {
    let mut config = AlloyConfig::default();
    config.server.bind = "0.0.0.0:8080".parse().unwrap();
    config.telemetry.trace_context.accept_incoming = ferrum_alloy::telemetry::AcceptPolicy::Any;
    let warnings = config.validate(NO_FEATURES).unwrap();
    let text: Vec<&str> = warnings.iter().map(|w| w.message.as_str()).collect();
    assert!(text.iter().any(|w| w.contains("non-loopback")), "{text:?}");
    assert!(
        text.iter().any(|w| w.contains("force sampling")),
        "{text:?}"
    );
}

#[test]
fn every_environment_variable_is_documented() {
    let docs = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/configuration.md"),
    )
    .unwrap();
    for var in ENV_VARS {
        assert!(
            docs.contains(var.name),
            "{} is missing from docs/configuration.md",
            var.name
        );
    }
    assert!(docs.contains("FERRUM_ALLOY_CONFIG"));
}

#[test]
fn env_var_table_maps_to_real_config_paths() {
    // Setting each variable to a type-appropriate value must deserialize.
    for var in ENV_VARS {
        let value = match var.kind {
            ferrum_alloy::config::EnvKind::Uint => "5",
            ferrum_alloy::config::EnvKind::Float => "0.5",
            ferrum_alloy::config::EnvKind::Bool => "false",
            ferrum_alloy::config::EnvKind::List => match var.name {
                "FERRUM_ALLOY_TRUSTED_NETWORKS" => "10.0.0.0/8",
                _ => "a,b",
            },
            _ => match var.name {
                "FERRUM_ALLOY_BIND" | "FERRUM_ALLOY_MANAGEMENT_BIND" => "127.0.0.1:1234",
                "FERRUM_ALLOY_LOG_FORMAT" => "json",
                "FERRUM_ALLOY_TRACE_CONTEXT_ACCEPT" => "never",
                "FERRUM_ALLOY_SERVER_TIMING" => "disabled",
                "FERRUM_ALLOY_EDGE_MODE" => "standalone",
                "FERRUM_ALLOY_TLS_CLIENT_AUTH" => "none",
                "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_EXEMPT_NETWORKS" => "10.0.0.0/8",
                _ => "value",
            },
        };
        let mut vars = vec![(var.name, value)];
        // TLS and JWT sections need their required siblings.
        if var.path.starts_with(&["server", "tls"]) {
            vars.extend([
                ("FERRUM_ALLOY_TLS_CERT_PATH", "c"),
                ("FERRUM_ALLOY_TLS_KEY_PATH", "k"),
            ]);
        }
        if var.path.starts_with(&["auth", "jwt"]) {
            vars.extend([
                ("FERRUM_ALLOY_JWT_ISSUER", "i"),
                ("FERRUM_ALLOY_JWT_AUDIENCES", "a"),
            ]);
        }
        vars.sort();
        vars.dedup_by_key(|(name, _)| *name);
        let result = load_from(None, env(&vars), &Overrides::default());
        assert!(result.is_ok(), "{} = {value}: {:?}", var.name, result.err());
    }
}
