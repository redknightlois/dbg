//! A `dbg start` whose `--break` does not register leaves no live daemon,
//! no saved session, and a latest-session pointer that still names the
//! session that was latest before it. Only an accepted start publishes the
//! latest-session pointer.

use std::path::Path;
use std::process::{Command, Output};

fn dbg(work: &Path, runtime: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbg"))
        .args(args)
        .current_dir(work)
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("DBG_SESSION")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn pid_files(runtime: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(runtime).unwrap().flatten() {
        for entry in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pid") {
                out.push(name);
            }
        }
    }
    out
}

fn assert_rejected(start: &Output) {
    let stderr = String::from_utf8_lossy(&start.stderr);
    assert!(!start.status.success(), "{stderr}");
    assert!(stderr.contains("failed to register"), "{stderr}");
}

#[test]
fn failed_break_leaves_no_daemon_and_keeps_the_previous_latest_session() {
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("hello.py"), "print(\"hi\")\n").unwrap();
    let dbg = |args: &[&str]| dbg(work.path(), runtime.path(), args);
    let bad_start = ["start", "pdb", "hello.py", "--break", "hello.py:999"];

    assert!(dbg(&["start", "pdb", "hello.py"]).status.success());
    let previous = stdout(&dbg(&["status"]));
    assert!(previous.contains("active session"), "{previous}");
    let daemons = pid_files(runtime.path());
    assert_eq!(daemons.len(), 1, "{daemons:?}");

    assert_rejected(&dbg(&bad_start));
    assert_eq!(stdout(&dbg(&["status"])), previous);
    assert_eq!(pid_files(runtime.path()), daemons);

    dbg(&["kill"]);
    assert_rejected(&dbg(&bad_start));
    let status = stdout(&dbg(&["status"]));
    assert!(status.contains("no session"), "{status}");
    let sessions = stdout(&dbg(&["sessions"]));
    assert!(!sessions.contains("hello.py-"), "{sessions}");
    assert!(pid_files(runtime.path()).is_empty());
}

fn latest_pointers(runtime: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(runtime).unwrap().flatten() {
        for entry in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry.file_name().to_string_lossy().ends_with(".latest") {
                out.extend(std::fs::read_to_string(entry.path()).ok());
            }
        }
    }
    out
}

#[test]
fn only_an_accepted_start_publishes_the_latest_session() {
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("hello.py"), "print(\"hi\")\n").unwrap();
    let dbg = |args: &[&str]| dbg(work.path(), runtime.path(), args);

    assert!(dbg(&["start", "pdb", "hello.py"]).status.success());
    let status = stdout(&dbg(&["status"]));
    assert!(status.contains("active session"), "{status}");
    let accepted = latest_pointers(runtime.path());
    assert_eq!(accepted.len(), 1, "{accepted:?}");

    // Sample the pointer for the whole life of a rejected start.
    let done = std::sync::atomic::AtomicBool::new(false);
    let (rejected, seen) = std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            let mut seen = std::collections::BTreeSet::new();
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                seen.extend(latest_pointers(runtime.path()));
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            seen
        });
        let rejected = dbg(&["start", "pdb", "hello.py", "--break", "hello.py:999"]);
        done.store(true, std::sync::atomic::Ordering::Release);
        (rejected, sampler.join().unwrap())
    });
    // Asserted after the join: a panic inside the scope would join a sampler that never stops.
    assert_rejected(&rejected);
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), accepted);
    dbg(&["kill"]);
}
