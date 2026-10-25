#[allow(dead_code)]
#[path = "../src/gdbg/db.rs"]
mod db;
#[allow(dead_code)]
#[path = "../src/gdbg/kernel.rs"]
mod kernel;

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn default_output_preserves_full_fields_with_pipe_separators() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.gpu.db");
    let db = db::GpuDb::create(&path).unwrap();
    let file =
        "profiles/a_complete_collection_path_longer_than_forty_three_characters/trace.nsys-rep";
    let layer = db.add_layer("nsys", file, None, Some(1.0), None).unwrap();
    let collected = "2026-10-25T17:42:01.123456Z";
    db.conn
        .execute("UPDATE layers SET collected_at=?1", [collected])
        .unwrap();
    let before = "at::native::unrolled_elementwise_kernel_with_a_long_and_distinct_α_suffix";
    let after = "internal::gemvx::kernel_with_a_long_and_distinct_β_suffix";
    for (name, start) in [(before, 0.0), (after, 12.0)] {
        db.conn.execute(
            "INSERT INTO launches (kernel_name,duration_us,start_us,stream_id,layer_id) VALUES (?1,10,?2,1,?3)",
            rusqlite::params![name, start, layer],
        ).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_gdbg"))
        .args(["--from", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"gaps 8\nkernels 8\nlayers\ntimeline 8\nquit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("# | Start | Duration | Before | After"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("1 | 10.0us | 2.0us | {before} | {after}")),
        "{stdout}"
    );
    assert!(stdout.contains(file), "{stdout}");
    assert!(stdout.contains(collected), "{stdout}");
    assert!(
        stdout.lines().filter(|line| line.contains(before)).count() >= 3,
        "{stdout}"
    );
    assert!(
        stdout.lines().filter(|line| line.contains(after)).count() >= 3,
        "{stdout}"
    );
    assert!(!stdout.contains("..."), "{stdout}");
    assert!(!stdout.contains('─'), "{stdout}");
    assert!(!stdout.contains("gpu>"), "{stdout}");
}
