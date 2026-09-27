//! Property tests: configuration errors never repeat a supplied value, and
//! secrets never appear in rendered configuration, whatever the value is.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;

use ferrum_alloy::config::{Overrides, load_from};
use proptest::prelude::*;

/// Configuration files that place `{secret}` where it must be rejected: wrong
/// type, bad address, unknown variant, unknown quoted key, and syntax errors.
const INVALID_FILES: &[&str] = &[
    "[server]\nmax_connections = \"{secret}\"\n",
    "[server]\nbind = \"{secret}\"\n",
    "[logging]\nformat = \"{secret}\"\n",
    "[management]\ntoken = [\"{secret}\"]\n",
    "[database]\n\"{secret}\" = 1\n",
    "[management]\ntoken = \"{secret}\" extra\n",
    "[database]\nurl = \"postgres://u:{secret}@db/x\" extra\n",
];

/// Variables that must reject `{secret}` as their value.
const INVALID_ENV: &[&str] = &[
    "FERRUM_ALLOY_DATABASE_MAX_CONNECTIONS",
    "FERRUM_ALLOY_OTLP_SAMPLING_RATIO",
    "FERRUM_ALLOY_MANAGEMENT_ENABLED",
    "FERRUM_ALLOY_MANAGEMENT_BIND",
    "FERRUM_ALLOY_LOG_FORMAT",
];

/// Secret-holding variables.
const SECRET_ENV: &[&str] = &[
    "FERRUM_ALLOY_MANAGEMENT_TOKEN",
    "FERRUM_ALLOY_DATABASE_URL",
];

/// A value no schema text contains, that parses as no number or boolean, and
/// that needs no escaping inside a TOML basic string.
const SECRET: &str = "SYNTH:[A-Za-z0-9/@+=._~-]{4,40}";

fn env(name: &str, value: &str) -> Vec<(OsString, OsString)> {
    vec![(OsString::from(name), OsString::from(value))]
}

proptest! {
    #[test]
    fn invalid_files_are_reported_without_the_value(
        secret in SECRET,
        file in 0..INVALID_FILES.len()
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alloy.toml");
        std::fs::write(&path, INVALID_FILES[file].replace("{secret}", &secret)).unwrap();
        let Err(error) = load_from(Some(&path), Vec::new(), &Overrides::default()) else {
            return Err(TestCaseError::fail(format!("accepted {:?}", INVALID_FILES[file])));
        };
        let rendered = error.to_string();
        let debug = format!("{error:?}");
        prop_assert!(!rendered.contains(&secret), "{}", rendered);
        prop_assert!(!debug.contains(&secret), "{}", debug);
    }

    #[test]
    fn invalid_environment_values_are_reported_without_the_value(
        secret in SECRET,
        name in 0..INVALID_ENV.len()
    ) {
        let vars = env(INVALID_ENV[name], &secret);
        let Err(error) = load_from(None, vars, &Overrides::default()) else {
            return Err(TestCaseError::fail(format!("accepted {}", INVALID_ENV[name])));
        };
        let rendered = error.to_string();
        let debug = format!("{error:?}");
        prop_assert!(!rendered.contains(&secret), "{}", rendered);
        prop_assert!(!debug.contains(&secret), "{}", debug);
    }

    #[test]
    fn secrets_are_never_rendered(secret in SECRET, name in 0..SECRET_ENV.len()) {
        let vars = env(SECRET_ENV[name], &secret);
        let (config, _sources) = load_from(None, vars, &Overrides::default()).unwrap();
        prop_assert!(!config.redacted_toml().contains(&secret));
        prop_assert!(!format!("{config:?}").contains(&secret));
    }
}
