//! Outcome accounting of `examples/test-dbg-commands.sh`, driven with a
//! stub `dbg` so the result does not depend on installed toolchains.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const STUB: &str = r#"#!/bin/sh
case "$1" in
  start)
    case "$STUB_MODE" in
      missing) echo "dbg: ghci not found in PATH"; exit 1 ;;
      broken) echo "dbg: daemon failed to start"; exit 1 ;;
    esac ;;
  hit-trend) [ "$STUB_MODE" = partial ] && exit 1 ;;
esac
exit 0
"#;

fn run(mode: &str) -> (bool, String) {
    let tmp = tempfile::tempdir().unwrap();
    let stub = tmp.path().join("dbg");
    std::fs::write(&stub, STUB).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/test-dbg-commands.sh"
        ))
        .arg("haskell")
        .env("DBG", &stub)
        .env("OUT_DIR", tmp.path().join("out"))
        .env("STUB_MODE", mode)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn missing_debugger_is_a_skip_and_exits_zero() {
    let (ok, out) = run("missing");
    assert!(ok, "{out}");
    assert!(out.contains("0 ok, 1 skipped, 0 failed"), "{out}");
}

#[test]
fn start_failure_without_a_toolchain_signature_fails() {
    let (ok, out) = run("broken");
    assert!(!ok, "{out}");
    assert!(out.contains("fib: fail, ack: fail"), "{out}");
    assert!(out.contains("0 ok, 0 skipped, 1 failed"), "{out}");
}

#[test]
fn partial_command_failure_reports_counts_and_fails() {
    let (ok, out) = run("partial");
    assert!(!ok, "{out}");
    assert!(out.contains("fib: 14/15, ack: 14/15"), "{out}");
    assert!(out.contains("0 ok, 0 skipped, 1 failed"), "{out}");
}
