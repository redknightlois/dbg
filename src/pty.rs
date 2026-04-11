use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::poll::{PollFd, PollFlags, poll};
use nix::pty::{OpenptyResult, openpty};
use nix::sys::signal::Signal;
use nix::unistd::{ForkResult, Pid, close, dup2, execvpe, fork, setsid};
use regex::Regex;

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

    /// Wait for the initial prompt after spawn.
    pub fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        self.read_until_prompt(timeout)
    }

    /// Send a command and wait for the prompt. Returns output between
    /// the echoed command and the next prompt.
    pub fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        // Write command
        let mut master_file = unsafe {
            std::fs::File::from_raw_fd(self.master.as_raw_fd())
        };
        write!(master_file, "{cmd}\n")?;
        // Don't drop — that would close the fd
        std::mem::forget(master_file);

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
            let mut f = unsafe { std::fs::File::from_raw_fd(self.master.as_raw_fd()) };
            let _ = write!(f, "{quit_cmd}\n");
            std::mem::forget(f);

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
                // Timeout — check if we have a prompt in what we accumulated
                if self.prompt_re.is_match(&strip_ansi(&accumulated)) {
                    break;
                }
                bail!("timeout waiting for prompt");
            }

            let mut master_file = unsafe {
                std::fs::File::from_raw_fd(self.master.as_raw_fd())
            };
            let bytes_read = master_file.read(&mut buf).unwrap_or(0);
            std::mem::forget(master_file);

            if bytes_read == 0 {
                break;
            }

            accumulated.push_str(&String::from_utf8_lossy(&buf[..bytes_read]));

            // Check for prompt at end of accumulated output
            let cleaned = strip_ansi(&accumulated);
            if self.prompt_re.is_match(&cleaned) {
                // Small extra wait to ensure no more output is coming
                std::thread::sleep(Duration::from_millis(20));

                let mut master_file = unsafe {
                    std::fs::File::from_raw_fd(self.master.as_raw_fd())
                };
                let extra_fd = PollFd::new(self.master.as_fd(), PollFlags::POLLIN);
                if poll(&mut [extra_fd], 30u16).unwrap_or(0) > 0 {
                    let extra = master_file.read(&mut buf).unwrap_or(0);
                    if extra > 0 {
                        accumulated.push_str(&String::from_utf8_lossy(&buf[..extra]));
                    }
                }
                std::mem::forget(master_file);
                break;
            }
        }

        Ok(accumulated)
    }
}

fn strip_ansi(s: &str) -> String {
    // Fast path: no escape char means no ANSI
    if !s.contains('\x1b') && !s.contains("[K") {
        return s.to_string();
    }
    let re = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]|\[K|\[2K").unwrap();
    re.replace_all(s, "").to_string()
}

impl Drop for DebuggerProcess {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGTERM);
    }
}
