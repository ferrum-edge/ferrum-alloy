//! Deterministic substitution at real command writer boundaries. Hooks exist
//! only in the unit-test binary; the shipped CLI has no test switch or env var.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;

use super::*;

type Hook = Box<dyn FnMut(&str, &Path)>;

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub(super) fn checkpoint(stage: &str, path: &Path) {
    HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(stage, path);
        }
    });
}

struct ClearHook;

impl Drop for ClearHook {
    fn drop(&mut self) {
        HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}

fn fixture(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures")
        .join(path)
}

fn command(kind: &str, root: &Path, input: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["ferrum-alloy".into()];
    let manifest = fixture("manifests/orders-api.toml");
    let file = root.join("output.json");
    let text: &[&str] = match kind {
        "new" => &["new", "svc", "--path"],
        "gitforgeops" => &["edge", "export", "--format", "gitforgeops", "--output"],
        "edge-new" | "edge-force" => &["edge", "export", "--output"],
        "openapi" => &["openapi", "export", "--output"],
        "diagnose" => &["diagnose", "--write-report"],
        _ => panic!("unknown command"),
    };
    args.extend(text.iter().map(|arg| OsString::from(*arg)));
    args.push(if matches!(kind, "new" | "gitforgeops") {
        root.as_os_str().to_owned()
    } else {
        file.into_os_string()
    });
    if kind.starts_with("edge-") || kind == "gitforgeops" {
        args.extend(["--manifest".into(), manifest.into_os_string()]);
    }
    if kind == "edge-force" {
        args.push("--force".into());
    }
    if kind == "openapi" {
        args.extend(["--input".into(), input.as_os_str().to_owned()]);
    }
    if kind == "diagnose" {
        args.extend([
            "--otlp".into(),
            fixture("otlp/edge-alloy-trace.jsonl").into_os_string(),
            "--trace-id".into(),
            "4bf92f3577b34da6a3ce929d0e0e4736".into(),
        ]);
    }
    args
}

fn invoke(args: Vec<OsString>) -> Result<ExitCode, CliError> {
    crate::run(crate::Cli::try_parse_from(args).unwrap())
}

fn input(dir: &Path) -> PathBuf {
    let path = dir.join("input.json");
    std::fs::write(
        &path,
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{}}"#,
    )
    .unwrap();
    path
}

#[cfg(unix)]
fn link_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn link_dir(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

fn link_file(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target, link).unwrap();
}

/// A separate thread moves the original directory, then plants a link while
/// the writer waits at the selected boundary. Unix must perform the actual
/// substitution. Windows must deny the move while the directory is held, and
/// permit the same move after command completion. No timing lottery or skip.
fn substituted_command(
    args: Vec<OsString>,
    stage: &'static str,
    selected: &Path,
    moved: &Path,
    outside: &Path,
    held: bool,
) -> Result<ExitCode, CliError> {
    let (start, started) = mpsc::sync_channel(1);
    let (done, finished) = mpsc::sync_channel(1);
    let source = selected.to_path_buf();
    let destination = moved.to_path_buf();
    let target = outside.to_path_buf();
    let attacker = std::thread::spawn(move || {
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        let result = std::fs::rename(&source, &destination);
        #[cfg(unix)]
        let expect_blocked = false;
        #[cfg(windows)]
        let expect_blocked = held;
        if expect_blocked {
            assert!(result.is_err(), "held directory could be renamed");
            assert!(source.is_dir());
            assert!(!destination.exists());
        } else {
            result.unwrap();
            link_dir(&target, &source);
        }
        done.send(()).unwrap();
    });
    #[cfg(unix)]
    let _ = held;
    let selected_hook = selected.to_path_buf();
    let mut fired = false;
    HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |point, path| {
            if !fired && point == stage && path == selected_hook.as_path() {
                fired = true;
                start.send(()).unwrap();
                finished.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }));
    });
    let clear = ClearHook;
    let result = invoke(args);
    drop(clear);
    attacker.join().unwrap();
    #[cfg(windows)]
    if held {
        std::fs::rename(selected, moved).unwrap();
        link_dir(outside, selected);
    }
    result
}

fn check_output(kind: &str, root: &Path) {
    let leaf = match kind {
        "new" => "src/main.rs",
        "gitforgeops" => "resources/ferrum/proxies/orders-api.yaml",
        _ => "output.json",
    };
    assert!(root.join(leaf).is_file(), "{kind}: {}", root.display());
    if kind == "new" {
        assert!(root.join("src/lib.rs").is_file());
        assert!(root.join("alloy.toml").is_file());
    }
    if matches!(kind, "openapi" | "diagnose") {
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join(leaf)).unwrap()).unwrap();
        assert!(json.is_object());
    }
    #[cfg(unix)]
    if kind == "diagnose" {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            root.join(leaf).metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn every_command_keeps_the_accepted_root_during_substitution() {
    for kind in [
        "new",
        "gitforgeops",
        "edge-new",
        "edge-force",
        "openapi",
        "diagnose",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        let moved = dir.path().join("retained");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("output.json"), b"untouched").unwrap();
        let args = command(kind, &root, &input(dir.path()));
        let result = substituted_command(args, "root", &root, &moved, &outside, true);
        assert_eq!(result.unwrap(), ExitCode::SUCCESS, "{kind}");
        check_output(kind, &moved);
        assert_eq!(
            std::fs::read(outside.join("output.json")).unwrap(),
            b"untouched"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }
}

#[test]
fn generated_commands_retain_subdirectories_for_later_files() {
    for (kind, relative, leaf) in [
        ("new", "src", "lib.rs"),
        ("gitforgeops", "resources/ferrum", "proxies/orders-api.yaml"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        let selected = root.join(relative);
        let moved = dir.path().join("retained-child");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let args = command(kind, &root, &input(dir.path()));
        let result = substituted_command(args, "directory", &selected, &moved, &outside, true);
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        assert!(moved.join(leaf).is_file());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}

#[test]
fn atomic_commands_keep_temporary_files_and_rename_in_the_same_directory() {
    for kind in ["edge-force", "openapi", "diagnose"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        let moved = dir.path().join("retained");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("output.json"), b"untouched").unwrap();
        let args = command(kind, &root, &input(dir.path()));
        let result = substituted_command(args, "rename", &root, &moved, &outside, true);
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        check_output(kind, &moved);
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(outside.join("output.json")).unwrap(),
            b"untouched"
        );
    }
}

#[test]
fn failed_atomic_replacement_cleans_up_in_the_retained_directory() {
    for kind in ["edge-force", "openapi", "diagnose"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        let moved = dir.path().join("retained");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("output.json")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("output.json"), b"untouched").unwrap();
        let args = command(kind, &root, &input(dir.path()));
        let result = substituted_command(args, "rename", &root, &moved, &outside, true);
        assert!(result.is_err());
        assert!(moved.join("output.json").is_dir());
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(outside.join("output.json")).unwrap(),
            b"untouched"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }
}

#[test]
fn creation_never_accepts_a_substituted_directory_link() {
    for (stage, relative) in [("root-created", ""), ("directory-created", "src")] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        let selected = if relative.is_empty() {
            root.clone()
        } else {
            root.join(relative)
        };
        let moved = dir.path().join("before-open");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let args = command("new", &root, &input(dir.path()));
        assert!(
            substituted_command(args, stage, &selected, &moved, &outside, false).is_err()
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}

#[test]
fn exclusive_leaves_refuse_links_planted_after_directory_acquisition() {
    for (kind, relative) in [
        ("new", "Cargo.toml"),
        ("gitforgeops", "resources/ferrum/proxies/orders-api.yaml"),
        ("edge-new", "output.json"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        std::fs::create_dir(&root).unwrap();
        let target = dir.path().join("outside-file");
        std::fs::write(&target, b"untouched").unwrap();
        let leaf = root.join(relative);
        let planted = leaf.clone();
        let outside = target.clone();
        let mut fired = false;
        HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |stage, path| {
                if !fired && stage == "leaf" && path == planted.as_path() {
                    fired = true;
                    let source = outside.clone();
                    let destination = planted.clone();
                    std::thread::spawn(move || link_file(&source, &destination))
                        .join()
                        .unwrap();
                }
            }));
        });
        let clear = ClearHook;
        let result = invoke(command(kind, &root, &input(dir.path())));
        drop(clear);
        assert!(result.is_err(), "{kind}");
        assert!(
            std::fs::symlink_metadata(&leaf)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
    }
}

#[test]
fn atomic_leaves_replace_links_planted_after_temporary_writing() {
    for kind in ["edge-force", "openapi", "diagnose"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("output");
        std::fs::create_dir(&root).unwrap();
        let target = dir.path().join("outside-file");
        std::fs::write(&target, b"untouched").unwrap();
        let selected = root.clone();
        let outside = target.clone();
        let mut fired = false;
        HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |stage, path| {
                if !fired && stage == "rename" && path == selected.as_path() {
                    fired = true;
                    let source = outside.clone();
                    let destination = selected.join("output.json");
                    std::thread::spawn(move || link_file(&source, &destination))
                        .join()
                        .unwrap();
                }
            }));
        });
        let clear = ClearHook;
        let result = invoke(command(kind, &root, &input(dir.path())));
        drop(clear);
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        check_output(kind, &root);
        assert!(
            !std::fs::symlink_metadata(root.join("output.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }
}

#[test]
fn temporary_names_differ_between_attempts() {
    let (first, second) = (temp_suffix(), temp_suffix());
    assert_ne!(first, second);
    assert!(first.ends_with(".tmp"));
}
