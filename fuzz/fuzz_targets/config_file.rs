//! Configuration files are operator input, and their errors must not echo
//! values that may be secrets.

#![no_main]

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ferrum_alloy::config::{load_from, Overrides};
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

/// Seeds place this value in keys and values. It is not a plain TOML key, so
/// no error message may repeat it.
const CANARY: &str = "fuzz:canary/0123456789";

struct InputFile {
    file: File,
    path: PathBuf,
    _directory: TempDir,
}

impl InputFile {
    fn create() -> std::io::Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("input.toml");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            _directory: directory,
        })
    }

    fn write(&mut self, data: &[u8]) -> std::io::Result<&Path> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        self.file.write_all(data)?;
        self.file.flush()?;
        Ok(&self.path)
    }
}

fn input_file() -> Option<&'static Mutex<InputFile>> {
    static INPUT_FILE: OnceLock<Option<Mutex<InputFile>>> = OnceLock::new();
    INPUT_FILE
        .get_or_init(|| InputFile::create().ok().map(Mutex::new))
        .as_ref()
}

fuzz_target!(|data: &[u8]| {
    let Some(input_file) = input_file() else {
        return;
    };
    let Ok(mut input_file) = input_file.lock() else {
        return;
    };
    let Ok(path) = input_file.write(data) else {
        return;
    };
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
