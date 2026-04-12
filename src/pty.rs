use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::poll::{PollFd, PollFlags, poll};
use nix::pty::{OpenptyResult, openpty};
use nix::sys::signal::Signal;
use nix::unistd::{ForkResult, Pid, close, dup2, execvpe, fork, setsid};
use regex::Regex;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]|\x1b\[K|\x1b\[2K").unwrap());

/// A debugger process running in a PTY.
pub struct DebuggerProcess {
    master: OwnedFd,
    child_pid: Pid,
    prompt_re: Regex,
}

impl DebuggerProcess {
    /// Spawn a debugger in a PTY.
    pub fn spawn(
        bin: &str,
        args: &[String],
        env_extra: &[(String, String)],
        prompt_pattern: &str,
    ) -> Result<Self> {
        let OpenptyResult { master, slave } = openpty(None, None)?;

        // Safety: fork is unsafe because it duplicates the process.
        let fork_result = unsafe { fork() }?;
        match fork_result {
            ForkResult::Child => {
                // Child: set up PTY as stdin/stdout/stderr, exec debugger.
                drop(master);
                setsid().ok();

                let slave_fd = slave.as_raw_fd();
                dup2(slave_fd, 0).ok();
                dup2(slave_fd, 1).ok();
                dup2(slave_fd, 2).ok();
                if slave_fd > 2 {
                    close(slave_fd).ok();
                }

                // Build environment
                let mut env: Vec<(String, String)> = std::env::vars().collect();
                for (k, v) in env_extra {
                    // Remove existing, then add
                    env.retain(|(ek, _)| ek != k);
                    env.push((k.clone(), v.clone()));
                }
                env.retain(|(k, _)| k != "TERM");
                env.push(("TERM".into(), "dumb".into()));

                let c_bin =
                    std::ffi::CString::new(bin).unwrap_or_else(|_| std::process::exit(127));
                let mut c_args = vec![c_bin.clone()];
                for a in args {
                    c_args.push(
                        std::ffi::CString::new(a.as_str())
                            .unwrap_or_else(|_| std::process::exit(127)),
                    );
                }
                let c_env: Vec<std::ffi::CString> = env
                    .iter()
                    .map(|(k, v)| {
                        std::ffi::CString::new(format!("{k}={v}"))
                            .unwrap_or_else(|_| std::process::exit(127))
                    })
                    .collect();

                execvpe(&c_bin, &c_args, &c_env).ok();
                std::process::exit(127);
            }
            ForkResult::Parent { child } => {
                drop(slave);
                let prompt_re = Regex::new(prompt_pattern)
                    .context("invalid prompt pattern")?;
                Ok(Self {
                    master,
                    child_pid: child,
                    prompt_re,
                })
            }
        }
    }

    /// Write bytes to the master fd without creating a File (which would
    /// close the fd on drop or panic).
    fn write_master(&self, data: &[u8]) -> Result<()> {
        let fd = self.master.as_raw_fd();
        let mut written = 0;
        while written < data.len() {
            match nix::unistd::write(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, &data[written..]) {
                Ok(n) => written += n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Read bytes from the master fd without creating a File.
    fn read_master(&self, buf: &mut [u8]) -> usize {
        nix::unistd::read(self.master.as_raw_fd(), buf).unwrap_or(0)
    }

    /// Wait for the initial prompt after spawn.
    pub fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        self.read_until_prompt(timeout)
    }

    /// Send a command and wait for the prompt. Returns output between
    /// the echoed command and the next prompt.
    pub fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        // Write command
        self.write_master(format!("{cmd}\n").as_bytes())?;

        // Read until prompt
        let raw = self.read_until_prompt(timeout)?;

        // Strip ANSI codes
        let clean = strip_ansi(&raw);

        // Remove all prompt occurrences from the output
        let no_prompts = self.prompt_re.replace_all(&clean, "");

        // Remove echoed command (first line)
        let lines: Vec<&str> = no_prompts.lines().collect();
        let start = if !lines.is_empty() && lines[0].contains(cmd.trim()) {
            1
        } else {
            0
        };

        // Skip trailing empty lines
        let mut end = lines.len();
        while end > start && lines[end - 1].trim().is_empty() {
            end -= 1;
        }

        let output = lines[start..end].join("\n");
        Ok(output.trim().to_string())
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&self) -> bool {
        nix::sys::wait::waitpid(self.child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG))
            .is_ok_and(|s| matches!(s, nix::sys::wait::WaitStatus::StillAlive))
    }

    /// Send quit command and wait for exit.
    pub fn quit(&self, quit_cmd: &str) {
        if self.is_alive() {
            let _ = self.write_master(format!("{quit_cmd}\n").as_bytes());

            // Give it a moment
            std::thread::sleep(Duration::from_millis(500));

            // Force kill if still alive
            if self.is_alive() {
                let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGKILL);
            }
        }
    }

    fn read_until_prompt(&self, timeout: Duration) -> Result<String> {
        let mut buf = [0u8; 4096];
        let mut accumulated = String::new();
        let start = Instant::now();

        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                bail!("timeout waiting for prompt");
            }

            let fd = PollFd::new(self.master.as_fd(), PollFlags::POLLIN);
            let ms = remaining.as_millis().min(u16::MAX as u128) as u16;
            let n = poll(&mut [fd], ms)?;

            if n == 0 {
                // Poll timed out (possibly due to u16 cap) — check prompt
                if self.prompt_re.is_match(&strip_ansi(&accumulated)) {
                    break;
                }
                // Real timeout is checked at loop top; continue to re-check
                continue;
            }

            let bytes_read = self.read_master(&mut buf);

            if bytes_read == 0 {
                break;
            }

            accumulated.push_str(&String::from_utf8_lossy(&buf[..bytes_read]));

            // Check for prompt at end of accumulated output
            let cleaned = strip_ansi(&accumulated);
            if self.prompt_re.is_match(&cleaned) {
                // Small extra wait to ensure no more output is coming
                std::thread::sleep(Duration::from_millis(20));

                let extra_fd = PollFd::new(self.master.as_fd(), PollFlags::POLLIN);
                if poll(&mut [extra_fd], 30u16).unwrap_or(0) > 0 {
                    let extra = self.read_master(&mut buf);
                    if extra > 0 {
                        accumulated.push_str(&String::from_utf8_lossy(&buf[..extra]));
                    }
                }
                break;
            }
        }

        Ok(accumulated)
    }
}

fn strip_ansi(s: &str) -> String {
    // Fast path: no escape char means no ANSI
    if !s.contains('\x1b') {
        return s.to_string();
    }
    ANSI_RE.replace_all(s, "").to_string()
}

impl Drop for DebuggerProcess {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGTERM);
    }
}
