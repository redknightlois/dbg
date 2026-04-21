//! Regression test for the "daemon dies after `dbg start` returns" bug.
//!
//! Background: Claude Code's Bash tool (and any pipe-capturing parent)
//! waits for EOF on the child's stdout pipe before it releases the tool
//! call. `dbg start` does `fork + setsid` for the daemon but used to
//! leave fd 0/1 inherited from the caller. The grandchild daemon kept
//! the stdout pipe open, so the harness either blocked until timeout or
//! reaped the whole process tree when it tore the pipe down — taking
//! the daemon with it. `setsid` alone isn't enough: it guards against
//! SIGHUP-on-tty-close, not pipe-teardown reaping.
//!
//! This test reproduces the exact scenario by driving the real `dbg`
//! binary with stdout captured on a pipe, waiting for `dbg` (the fork
//! parent) to exit, and then asserting the pipe EOFs promptly. The
//! grandchild sleeps 5s; if it still holds fd 1, the pipe stays open
//! and the poll times out.

use std::io::Read;
use std::os::fd::AsFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::pipe;

#[test]
fn dbg_start_releases_parent_stdout_pipe() {
    // pipe() returns (read, write). We'll hand the write end to `dbg`
    // as its stdout, keep the read end here, then poll for EOF after
    // the fork-parent exits.
    let (read_end, write_end) = pipe().expect("pipe");
    // Mark both ends CLOEXEC. We hand `write_end` to Stdio::from(), which
    // dup2's it onto the child's fd 1 (the dup clears CLOEXEC on fd 1,
    // exactly what we want) and then closes the original in the test
    // process. Without CLOEXEC, the *original* raw fd would also survive
    // the exec into `dbg` as a non-standard fd the daemon has no reason
    // to know about — not representative of what Claude Code's Bash tool
    // actually exposes (only 0/1/2). CLOEXEC on read_end is just hygiene
    // so concurrent test-process forks don't leak it.
    use std::os::fd::AsRawFd;
    fcntl(read_end.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).expect("cloexec r");
    fcntl(write_end.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).expect("cloexec w");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dbg"))
        .env("DBG_DETACH_SELF_TEST", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(write_end))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dbg");

    // Fork parent should return immediately — if it doesn't, something
    // unrelated is wrong. Bound the wait so the test can't hang.
    let wait_started = Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if wait_started.elapsed() > Duration::from_secs(2) => {
                let _ = child.kill();
                panic!("dbg fork-parent did not exit within 2s");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    assert!(status.success(), "dbg exited non-zero: {status:?}");

    // Fork parent has exited. The grandchild (simulated daemon) is
    // still alive, sleeping 5s. The question: is our pipe already at
    // EOF? It is iff the grandchild detached fd 1.
    //
    // Poll with a 1s timeout. POLLHUP => EOF (pipe has no more writers).
    let fd = read_end.as_fd();
    let mut fds = [PollFd::new(fd, PollFlags::POLLIN | PollFlags::POLLHUP)];
    let ready = poll(&mut fds, PollTimeout::from(1000u16)).expect("poll");

    assert!(
        ready > 0,
        "pipe did not EOF within 1s after dbg exited — \
         the grandchild daemon still holds fd 1, which is exactly the \
         bug that makes Claude Code's Bash tool hang or reap the daemon"
    );

    // Drain: a detached daemon must not have written anything to our
    // pipe. Read to confirm EOF (0 bytes).
    let mut f = std::fs::File::from(read_end);
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf).expect("read");
    assert_eq!(
        n, 0,
        "expected EOF, got {n} bytes: {:?}",
        &buf[..n.min(buf.len())]
    );
}
