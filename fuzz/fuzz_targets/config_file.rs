//! Configuration files are operator input, and their errors must not echo
//! values that may be secrets.

#![no_main]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

use ferrum_alloy::config::{Overrides, load_from};
use libfuzzer_sys::fuzz_target;

/// Seeds place this value in keys and values. It is not a plain TOML key, so
/// no error message may repeat it.
const CANARY: &str = "fuzz:canary/0123456789";

fn path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("ferrum-alloy-fuzz-{}.toml", std::process::id()))
    })
}

fuzz_target!(|data: &[u8]| {
    let path = path();
    if std::fs::write(path, data).is_err() {
        return;
    }
    let env: [(OsString, OsString); 0] = [];
    match load_from(Some(path), env, &Overrides::default()) {
        Ok((config, _sources)) => {
            let _ = config.redacted_toml();
            let _ = config.check(&[]);
        }
        Err(error) => {
            let rendered = error.to_string();
            let debug = format!("{error:?}");
            assert!(!rendered.contains(CANARY), "{rendered}");
            assert!(!debug.contains(CANARY), "{debug}");
        }
    }
});
