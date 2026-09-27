//! The built binary drains and exits cleanly on a real Ctrl-C: SIGINT on
//! Unix, and a console `CTRL_C_EVENT` on Windows, where Alloy waits only for
//! `tokio::signal::ctrl_c`.

#![cfg(any(unix, windows))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn ctrl_c_drains_and_exits_cleanly() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_example-minimal"));
    command
        .env("FERRUM_ALLOY_BIND", "127.0.0.1:0")
        .env("FERRUM_ALLOY_MANAGEMENT_ENABLED", "false")
        .env("FERRUM_ALLOY_LOG_FORMAT", "json")
        .env("FERRUM_ALLOY_LOG_FILTER", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A console of its own, so the Ctrl-C event reaches the service and
        // not this test harness. CREATE_NEW_PROCESS_GROUP is deliberately not
        // used: it disables Ctrl-C for the new process group.
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        command.creation_flags(CREATE_NEW_CONSOLE);
    }
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();

    // Find the bound address in the "serving" event.
    let addr = loop {
        let line = lines
            .next()
            .expect("process exited before serving")
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        if event["message"] == "serving" {
            let listen = event["listen"].as_str().unwrap();
            break listen
                .trim_start_matches("Some(")
                .trim_end_matches(')')
                .to_owned();
        }
    };

    let mut stream = TcpStream::connect(&addr).unwrap();
    stream
        .write_all(b"GET /hello HTTP/1.1\r\nhost: t\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("Hello from Ferrum Alloy"));

    let reader = std::thread::spawn(move || {
        let mut saw_complete = false;
        for line in lines.map_while(Result::ok) {
            saw_complete |= line.contains("shutdown complete");
        }
        saw_complete
    });
    send_ctrl_c(&child);
    let started = Instant::now();
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if started.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            panic!("the service did not exit after Ctrl-C");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let saw_complete = reader.join().unwrap();
    // A process killed by an unhandled Ctrl-C exits unsuccessfully
    // (STATUS_CONTROL_C_EXIT on Windows, signal 2 on Unix).
    assert!(exit.success(), "exit status {exit}");
    assert!(saw_complete, "the shutdown sequence ran to completion");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[cfg(unix)]
fn send_ctrl_c(child: &Child) {
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Delivers a real `CTRL_C_EVENT` to the service's console. A helper process
/// detaches from its own console, attaches to the service's, ignores Ctrl-C
/// itself, and raises the event for every process on that console. This is
/// the documented way to signal another console process; the Win32 calls run
/// in PowerShell because this workspace forbids `unsafe` code.
#[cfg(windows)]
fn send_ctrl_c(child: &Child) {
    const SCRIPT: &str = r#"param([uint32]$ProcessId)
$ErrorActionPreference = 'Stop'
Add-Type -Namespace FerrumAlloyTest -Name Win32 -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool AttachConsole(uint processId);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool SetConsoleCtrlHandler(System.IntPtr handler, bool add);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroupId);
'@
function Assert-Win32([bool]$Ok, [string]$Call) {
    if (-not $Ok) {
        $code = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "$Call failed with Win32 error $code"
    }
}
[void][FerrumAlloyTest.Win32]::FreeConsole()
Assert-Win32 ([FerrumAlloyTest.Win32]::AttachConsole($ProcessId)) 'AttachConsole'
Assert-Win32 ([FerrumAlloyTest.Win32]::SetConsoleCtrlHandler([System.IntPtr]::Zero, $true)) 'SetConsoleCtrlHandler'
Assert-Win32 ([FerrumAlloyTest.Win32]::GenerateConsoleCtrlEvent(0, 0)) 'GenerateConsoleCtrlEvent'
[void][FerrumAlloyTest.Win32]::FreeConsole()
"#;
    let script = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("send-ctrl-c-{}.ps1", child.id()));
    std::fs::write(&script, SCRIPT).unwrap();
    let output = Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive"])
        .args(["-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg(child.id().to_string())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&script);
    assert!(
        output.status.success(),
        "sending Ctrl-C failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
