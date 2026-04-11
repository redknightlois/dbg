use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Parsed profile data for interactive querying.
pub struct ProfileData {
    frames: Vec<Frame>,
    /// Per-frame inclusive time (ms)
    inclusive: Vec<f64>,
    /// Per-frame exclusive time (ms)
    exclusive: Vec<f64>,
    /// Parent → children with time
    children: HashMap<usize, Vec<(usize, f64)>>,
    /// Child → parents with time
    parents: HashMap<usize, Vec<(usize, f64)>>,
    /// All observed stacks: (stack of frame indices, duration)
    stacks: Vec<(Vec<usize>, f64)>,
    /// Total profile time
    total_time: f64,
    /// Number of threads
    thread_count: usize,
    /// Focus filter (frame name substring)
    focus: Option<String>,
    /// Ignore filter (frame name substring)
    ignore: Option<String>,
}

#[derive(Deserialize)]
struct SpeedscopeFile {
    shared: SpeedscopeShared,
    profiles: Vec<SpeedscopeProfile>,
}

#[derive(Deserialize)]
struct SpeedscopeShared {
    frames: Vec<SpeedscopeFrame>,
}

#[derive(Deserialize)]
struct SpeedscopeFrame {
    name: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct SpeedscopeProfile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    events: Vec<SpeedscopeEvent>,
}

#[derive(Deserialize)]
struct SpeedscopeEvent {
    #[serde(rename = "type")]
    event_type: String,
    at: f64,
    frame: usize,
}

#[derive(Clone)]
struct Frame {
    name: String,
}

impl ProfileData {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context("failed to read speedscope file")?;
        let file: SpeedscopeFile =
            serde_json::from_str(&content).context("failed to parse speedscope JSON")?;

        let frames: Vec<Frame> = file
            .shared
            .frames
            .iter()
            .map(|f| Frame {
                name: f.name.clone(),
            })
            .collect();

        let n = frames.len();
        let mut inclusive = vec![0.0f64; n];
        let mut exclusive = vec![0.0f64; n];
        let mut children: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
        let mut parents: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
        let mut stacks: Vec<(Vec<usize>, f64)> = Vec::new();
        let mut total_time = 0.0f64;
        let thread_count = file.profiles.len();

        for profile in &file.profiles {
            let mut stack: Vec<(usize, f64)> = Vec::new(); // (frame_idx, open_time)

            for event in &profile.events {
                let idx = event.frame;
                if idx >= n {
                    continue;
                }

                match event.event_type.as_str() {
                    "O" => {
                        stack.push((idx, event.at));
                    }
                    "C" => {
                        if let Some((opened_idx, opened_at)) = stack.pop() {
                            let duration = event.at - opened_at;
                            if duration > 0.0 {
                                inclusive[opened_idx] += duration;

                                // Exclusive: subtract children's time
                                // (we'll compute this after)

                                // Record parent-child
                                if let Some(&(parent_idx, _)) = stack.last() {
                                    children
                                        .entry(parent_idx)
                                        .or_default()
                                        .push((opened_idx, duration));
                                    parents
                                        .entry(opened_idx)
                                        .or_default()
                                        .push((parent_idx, duration));
                                }

                                // Record stack trace
                                let mut trace: Vec<usize> =
                                    stack.iter().map(|(idx, _)| *idx).collect();
                                trace.push(opened_idx);
                                stacks.push((trace, duration));

                                // Track total from root frames
                                if stack.is_empty() {
                                    total_time += duration;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Compute exclusive = inclusive - sum(children inclusive)
        for i in 0..n {
            let child_time: f64 = children
                .get(&i)
                .map(|c| c.iter().map(|(_, t)| t).sum())
                .unwrap_or(0.0);
            exclusive[i] = (inclusive[i] - child_time).max(0.0);
        }

        Ok(Self {
            frames,
            inclusive,
            exclusive,
            children,
            parents,
            stacks,
            total_time,
            thread_count,
            focus: None,
            ignore: None,
        })
    }

    pub fn handle_command(&mut self, cmd: &str) -> String {
        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
        if parts.is_empty() {
            return String::new();
        }

        match parts[0] {
            "top" => {
                let n: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
                self.cmd_top(n)
            }
            "callers" => {
                let pattern = parts[1..].join(" ");
                if pattern.is_empty() {
                    return "usage: callers <function-name>".to_string();
                }
                self.cmd_callers(&pattern)
            }
            "callees" => {
                let pattern = parts[1..].join(" ");
                if pattern.is_empty() {
                    return "usage: callees <function-name>".to_string();
                }
                self.cmd_callees(&pattern)
            }
            "traces" => {
                let n: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
                self.cmd_traces(n)
            }
            "tree" => {
                let n: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
                self.cmd_tree(n)
            }
            "hotpath" => self.cmd_hotpath(),
            "threads" => self.cmd_threads(),
            "stats" => self.cmd_stats(),
            "search" => {
                let pattern = parts[1..].join(" ");
                if pattern.is_empty() {
                    return "usage: search <pattern>".to_string();
                }
                self.cmd_search(&pattern)
            }
            "focus" => {
                let pattern = parts[1..].join(" ");
                if pattern.is_empty() {
                    return "usage: focus <function-name>".to_string();
                }
                self.focus = Some(pattern.clone());
                format!("focus set: {pattern}")
            }
            "ignore" => {
                let pattern = parts[1..].join(" ");
                if pattern.is_empty() {
                    return "usage: ignore <function-name>".to_string();
                }
                self.ignore = Some(pattern.clone());
                format!("ignore set: {pattern}")
            }
            "reset" => {
                self.focus = None;
                self.ignore = None;
                "filters cleared".to_string()
            }
            "help" => self.cmd_help(),
            _ => format!("unknown command: {}. Type 'help' for commands.", parts[0]),
        }
    }

    fn matches_filter(&self, frame_idx: usize) -> bool {
        let name = &self.frames[frame_idx].name;
        if let Some(ref focus) = self.focus {
            if !name.contains(focus.as_str()) {
                return false;
            }
        }
        if let Some(ref ignore) = self.ignore {
            if name.contains(ignore.as_str()) {
                return false;
            }
        }
        true
    }

    fn stack_matches_filter(&self, stack: &[usize]) -> bool {
        if let Some(ref focus) = self.focus {
            if !stack
                .iter()
                .any(|&idx| self.frames[idx].name.contains(focus.as_str()))
            {
                return false;
            }
        }
        if let Some(ref ignore) = self.ignore {
            if stack
                .iter()
                .any(|&idx| self.frames[idx].name.contains(ignore.as_str()))
            {
                return false;
            }
        }
        true
    }

    fn cmd_top(&self, n: usize) -> String {
        let mut entries: Vec<(usize, f64, f64)> = (0..self.frames.len())
            .filter(|&i| self.matches_filter(i))
            .filter(|&i| self.inclusive[i] > 0.0)
            .map(|i| (i, self.inclusive[i], self.exclusive[i]))
            .collect();

        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        entries.truncate(n);

        let total = self.total_time.max(0.001);
        let mut out = format!(
            "{:<60} {:>10} {:>10}\n",
            "Function", "Inclusive", "Exclusive"
        );
        for (idx, inc, exc) in entries {
            let name = shorten(&self.frames[idx].name, 58);
            out.push_str(&format!(
                "{:<60} {:>9.1}% {:>9.1}%\n",
                name,
                inc / total * 100.0,
                exc / total * 100.0
            ));
        }
        out
    }

    fn cmd_callers(&self, pattern: &str) -> String {
        let targets: Vec<usize> = self.find_frames(pattern);
        if targets.is_empty() {
            return format!("no function matching '{pattern}'");
        }

        let mut out = String::new();
        for &idx in &targets {
            out.push_str(&format!("callers of {}:\n", self.frames[idx].name));
            if let Some(parent_list) = self.parents.get(&idx) {
                let mut aggregated: HashMap<usize, f64> = HashMap::new();
                for &(parent_idx, time) in parent_list {
                    *aggregated.entry(parent_idx).or_default() += time;
                }
                let mut sorted: Vec<(usize, f64)> = aggregated.into_iter().collect();
                sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                for (parent_idx, time) in sorted.iter().take(15) {
                    out.push_str(&format!(
                        "  {:>8.2}ms  {}\n",
                        time,
                        shorten(&self.frames[*parent_idx].name, 70)
                    ));
                }
            } else {
                out.push_str("  (root — no callers)\n");
            }
        }
        out
    }

    fn cmd_callees(&self, pattern: &str) -> String {
        let targets: Vec<usize> = self.find_frames(pattern);
        if targets.is_empty() {
            return format!("no function matching '{pattern}'");
        }

        let mut out = String::new();
        for &idx in &targets {
            out.push_str(&format!("callees of {}:\n", self.frames[idx].name));
            if let Some(child_list) = self.children.get(&idx) {
                let mut aggregated: HashMap<usize, f64> = HashMap::new();
                for &(child_idx, time) in child_list {
                    *aggregated.entry(child_idx).or_default() += time;
                }
                let mut sorted: Vec<(usize, f64)> = aggregated.into_iter().collect();
                sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                for (child_idx, time) in sorted.iter().take(15) {
                    out.push_str(&format!(
                        "  {:>8.2}ms  {}\n",
                        time,
                        shorten(&self.frames[*child_idx].name, 70)
                    ));
                }
            } else {
                out.push_str("  (leaf — no callees)\n");
            }
        }
        out
    }

    fn cmd_traces(&self, n: usize) -> String {
        let filtered: Vec<&(Vec<usize>, f64)> = self
            .stacks
            .iter()
            .filter(|(stack, _)| self.stack_matches_filter(stack))
            .collect();

        // Aggregate identical stacks
        let mut aggregated: HashMap<Vec<usize>, f64> = HashMap::new();
        for (stack, time) in &filtered {
            *aggregated.entry(stack.clone()).or_default() += time;
        }

        let mut sorted: Vec<(Vec<usize>, f64)> = aggregated.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        sorted.truncate(n);

        let mut out = String::new();
        for (stack, time) in &sorted {
            out.push_str(&format!("-----------+{:-<60}\n", ""));
            out.push_str(&format!("{:>8.2}ms  ", time));
            for (i, &idx) in stack.iter().rev().enumerate() {
                if i > 0 {
                    out.push_str("            ");
                }
                out.push_str(&shorten(&self.frames[idx].name, 70));
                out.push('\n');
            }
        }
        out
    }

    fn cmd_tree(&self, n: usize) -> String {
        // Find top-N root frames by inclusive time
        let mut roots: Vec<(usize, f64)> = (0..self.frames.len())
            .filter(|&i| self.inclusive[i] > 0.0 && !self.parents.contains_key(&i))
            .map(|i| (i, self.inclusive[i]))
            .collect();
        roots.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        roots.truncate(n);

        let total = self.total_time.max(0.001);
        let mut out = String::new();
        for (idx, _) in &roots {
            self.tree_recurse(*idx, 0, total, &mut out, 5);
        }
        out
    }

    fn tree_recurse(
        &self,
        idx: usize,
        depth: usize,
        total: f64,
        out: &mut String,
        max_depth: usize,
    ) {
        if depth > max_depth {
            return;
        }
        let indent = "  ".repeat(depth);
        let pct = self.inclusive[idx] / total * 100.0;
        if pct < 0.5 {
            return;
        }
        out.push_str(&format!(
            "{}{:>6.1}%  {}\n",
            indent,
            pct,
            shorten(&self.frames[idx].name, 60)
        ));

        if let Some(child_list) = self.children.get(&idx) {
            let mut aggregated: HashMap<usize, f64> = HashMap::new();
            for &(child_idx, time) in child_list {
                *aggregated.entry(child_idx).or_default() += time;
            }
            let mut sorted: Vec<(usize, f64)> = aggregated.into_iter().collect();
            sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            for (child_idx, _) in sorted {
                self.tree_recurse(child_idx, depth + 1, total, out, max_depth);
            }
        }
    }

    fn cmd_hotpath(&self) -> String {
        // Find the stack with the most time
        if self.stacks.is_empty() {
            return "no stacks recorded".to_string();
        }

        let mut aggregated: HashMap<Vec<usize>, f64> = HashMap::new();
        for (stack, time) in &self.stacks {
            *aggregated.entry(stack.clone()).or_default() += time;
        }

        let hottest = aggregated.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap());

        match hottest {
            Some((stack, time)) => {
                let mut out = format!("hottest path ({:.2}ms):\n", time);
                for (i, &idx) in stack.iter().enumerate() {
                    let indent = "  ".repeat(i);
                    out.push_str(&format!(
                        "{}→ {}\n",
                        indent,
                        shorten(&self.frames[idx].name, 70)
                    ));
                }
                out
            }
            None => "no stacks recorded".to_string(),
        }
    }

    fn cmd_threads(&self) -> String {
        format!("{} threads observed in profile", self.thread_count)
    }

    fn cmd_stats(&self) -> String {
        let active_frames = self.inclusive.iter().filter(|&&t| t > 0.0).count();
        format!(
            "total time: {:.2}ms\nframes: {} ({} active)\nthreads: {}\nstacks: {}\nfocus: {}\nignore: {}",
            self.total_time,
            self.frames.len(),
            active_frames,
            self.thread_count,
            self.stacks.len(),
            self.focus.as_deref().unwrap_or("(none)"),
            self.ignore.as_deref().unwrap_or("(none)"),
        )
    }

    fn cmd_search(&self, pattern: &str) -> String {
        let matches: Vec<(usize, &Frame)> = self
            .frames
            .iter()
            .enumerate()
            .filter(|(i, f)| {
                f.name.contains(pattern) && self.inclusive[*i] > 0.0
            })
            .collect();

        if matches.is_empty() {
            return format!("no functions matching '{pattern}'");
        }

        let total = self.total_time.max(0.001);
        let mut out = format!("{} matches:\n", matches.len());
        for (idx, frame) in matches.iter().take(30) {
            out.push_str(&format!(
                "  {:>6.1}%  {}\n",
                self.inclusive[*idx] / total * 100.0,
                shorten(&frame.name, 70)
            ));
        }
        out
    }

    fn cmd_help(&self) -> String {
        "commands: top [N], callers <func>, callees <func>, traces [N], tree [N], hotpath, threads, stats, search <pattern>, focus <func>, ignore <func>, reset".to_string()
    }

    fn find_frames(&self, pattern: &str) -> Vec<usize> {
        self.frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name.contains(pattern))
            .map(|(i, _)| i)
            .collect()
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_speedscope() -> String {
        r#"{
            "shared": {
                "frames": [
                    {"name": "main"},
                    {"name": "compute"},
                    {"name": "sort"},
                    {"name": "alloc"}
                ]
            },
            "profiles": [{
                "name": "thread_0",
                "events": [
                    {"type": "O", "at": 0.0, "frame": 0},
                    {"type": "O", "at": 0.0, "frame": 1},
                    {"type": "O", "at": 0.0, "frame": 2},
                    {"type": "C", "at": 5.0, "frame": 2},
                    {"type": "O", "at": 5.0, "frame": 3},
                    {"type": "C", "at": 7.0, "frame": 3},
                    {"type": "C", "at": 8.0, "frame": 1},
                    {"type": "C", "at": 10.0, "frame": 0}
                ]
            }]
        }"#
        .to_string()
    }

    fn load_sample() -> ProfileData {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dbg-test-profile-{}", id));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.speedscope.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(sample_speedscope().as_bytes()).unwrap();
        ProfileData::load(&path).unwrap()
    }

    #[test]
    fn load_parses_frames() {
        let p = load_sample();
        assert_eq!(p.frames.len(), 4);
        assert_eq!(p.frames[0].name, "main");
        assert_eq!(p.frames[2].name, "sort");
    }

    #[test]
    fn load_computes_inclusive_time() {
        let p = load_sample();
        assert!((p.inclusive[0] - 10.0).abs() < 0.01); // main: 0-10
        assert!((p.inclusive[1] - 8.0).abs() < 0.01); // compute: 0-8
        assert!((p.inclusive[2] - 5.0).abs() < 0.01); // sort: 0-5
        assert!((p.inclusive[3] - 2.0).abs() < 0.01); // alloc: 5-7
    }

    #[test]
    fn load_computes_exclusive_time() {
        let p = load_sample();
        assert!((p.exclusive[0] - 2.0).abs() < 0.01); // main: 10 - 8 = 2
        assert!((p.exclusive[1] - 1.0).abs() < 0.01); // compute: 8 - 5 - 2 = 1
        assert!((p.exclusive[2] - 5.0).abs() < 0.01); // sort: leaf
        assert!((p.exclusive[3] - 2.0).abs() < 0.01); // alloc: leaf
    }

    #[test]
    fn cmd_top_default() {
        let mut p = load_sample();
        let out = p.handle_command("top");
        assert!(out.contains("main"));
        assert!(out.contains("compute"));
        assert!(out.contains("sort"));
    }

    #[test]
    fn cmd_top_with_limit() {
        let mut p = load_sample();
        let out = p.handle_command("top 2");
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        // header + 2 entries
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn cmd_callers() {
        let mut p = load_sample();
        let out = p.handle_command("callers sort");
        assert!(out.contains("compute"));
    }

    #[test]
    fn cmd_callers_not_found() {
        let mut p = load_sample();
        let out = p.handle_command("callers nonexistent");
        assert!(out.contains("no function matching"));
    }

    #[test]
    fn cmd_callees() {
        let mut p = load_sample();
        let out = p.handle_command("callees compute");
        assert!(out.contains("sort"));
        assert!(out.contains("alloc"));
    }

    #[test]
    fn cmd_traces() {
        let mut p = load_sample();
        let out = p.handle_command("traces 5");
        assert!(out.contains("main"));
    }

    #[test]
    fn cmd_tree() {
        let mut p = load_sample();
        let out = p.handle_command("tree");
        assert!(out.contains("main"));
        assert!(out.contains("100.0%"));
    }

    #[test]
    fn cmd_hotpath() {
        let mut p = load_sample();
        let out = p.handle_command("hotpath");
        assert!(out.contains("main"));
    }

    #[test]
    fn cmd_stats() {
        let mut p = load_sample();
        let out = p.handle_command("stats");
        assert!(out.contains("total time:"));
        assert!(out.contains("frames: 4"));
        assert!(out.contains("threads: 1"));
    }

    #[test]
    fn cmd_threads() {
        let mut p = load_sample();
        let out = p.handle_command("threads");
        assert!(out.contains("1 threads"));
    }

    #[test]
    fn cmd_search() {
        let mut p = load_sample();
        let out = p.handle_command("search sort");
        assert!(out.contains("sort"));
        assert!(out.contains("1 matches"));
    }

    #[test]
    fn cmd_search_no_pattern() {
        let mut p = load_sample();
        let out = p.handle_command("search");
        assert!(out.contains("usage:"));
    }

    #[test]
    fn cmd_focus_filters_top() {
        let mut p = load_sample();
        p.handle_command("focus sort");
        let out = p.handle_command("top");
        assert!(out.contains("sort"));
        assert!(!out.contains("alloc"));
    }

    #[test]
    fn cmd_ignore_filters_top() {
        let mut p = load_sample();
        p.handle_command("ignore sort");
        let out = p.handle_command("top");
        assert!(!out.contains("sort"));
        assert!(out.contains("main"));
    }

    #[test]
    fn cmd_reset_clears_filters() {
        let mut p = load_sample();
        p.handle_command("focus sort");
        let out = p.handle_command("reset");
        assert!(out.contains("filters cleared"));
        let top = p.handle_command("top");
        assert!(top.contains("main"));
        assert!(top.contains("sort"));
    }

    #[test]
    fn cmd_help() {
        let mut p = load_sample();
        let out = p.handle_command("help");
        assert!(out.contains("top"));
        assert!(out.contains("callers"));
        assert!(out.contains("focus"));
    }

    #[test]
    fn cmd_unknown() {
        let mut p = load_sample();
        let out = p.handle_command("garbage");
        assert!(out.contains("unknown command"));
    }

    #[test]
    fn cmd_empty() {
        let mut p = load_sample();
        let out = p.handle_command("");
        assert!(out.is_empty());
    }

    #[test]
    fn shorten_within_limit() {
        assert_eq!(shorten("abc", 10), "abc");
    }

    #[test]
    fn shorten_truncates() {
        let result = shorten("long function name here", 10);
        assert!(result.len() <= 12); // 9 chars + multi-byte …
        assert!(result.ends_with('…'));
    }
}
