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

// --- V8 .cpuprofile format ---

#[derive(Deserialize)]
struct V8CpuProfile {
    nodes: Vec<V8Node>,
    samples: Vec<u64>,
    #[serde(rename = "timeDeltas")]
    time_deltas: Vec<f64>,
}

#[derive(Deserialize)]
struct V8Node {
    id: u64,
    #[serde(rename = "callFrame")]
    call_frame: V8CallFrame,
    #[serde(default)]
    children: Vec<u64>,
}

#[derive(Deserialize)]
struct V8CallFrame {
    #[serde(rename = "functionName")]
    function_name: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "lineNumber", default)]
    line_number: i64,
}

impl ProfileData {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context("failed to read profile file")?;

        // Auto-detect: V8 cpuprofile has "nodes" + "samples" at top level
        if path.extension().is_some_and(|e| e == "cpuprofile")
            || (content.contains("\"nodes\"") && content.contains("\"timeDeltas\""))
        {
            return Self::load_v8_cpuprofile(&content);
        }

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

    /// Load a V8 .cpuprofile and convert to Speedscope evented format in memory.
    fn load_v8_cpuprofile(content: &str) -> Result<Self> {
        let profile: V8CpuProfile =
            serde_json::from_str(content).context("failed to parse V8 cpuprofile JSON")?;

        // Build node-id → index map and parent map
        let id_to_idx: HashMap<u64, usize> = profile
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id, i))
            .collect();
        let mut parent_of: HashMap<u64, u64> = HashMap::new();
        for node in &profile.nodes {
            for &child_id in &node.children {
                parent_of.insert(child_id, node.id);
            }
        }

        // Build frame names, deduplicating nodes with the same identity.
        // V8 creates separate nodes per recursion depth; we merge them.
        let mut frames: Vec<Frame> = Vec::new();
        let mut name_to_frame: HashMap<String, usize> = HashMap::new();
        let mut node_to_frame: Vec<usize> = Vec::with_capacity(profile.nodes.len());

        for node in &profile.nodes {
            let name = if node.call_frame.url.is_empty() {
                let fn_name = &node.call_frame.function_name;
                if fn_name.is_empty() {
                    "(anonymous)".to_string()
                } else {
                    fn_name.clone()
                }
            } else {
                let fn_name = if node.call_frame.function_name.is_empty() {
                    "(anonymous)"
                } else {
                    &node.call_frame.function_name
                };
                format!("{} ({}:{})", fn_name, node.call_frame.url, node.call_frame.line_number + 1)
            };
            let frame_idx = *name_to_frame.entry(name.clone()).or_insert_with(|| {
                let idx = frames.len();
                frames.push(Frame { name });
                idx
            });
            node_to_frame.push(frame_idx);
        }

        // Walk parent chain to build a stack for a given node id.
        // Collapse consecutive recursive calls to the same frame so that
        // e.g. ackermann→ackermann→ackermann appears as a single frame
        // in the stack rather than inflating inclusive time.
        let build_stack = |node_id: u64| -> Vec<usize> {
            let mut stack = Vec::new();
            let mut id = node_id;
            loop {
                if let Some(&node_idx) = id_to_idx.get(&id) {
                    let frame_idx = node_to_frame[node_idx];
                    // Skip if same frame as the previous entry (recursion)
                    if stack.last() != Some(&frame_idx) {
                        stack.push(frame_idx);
                    }
                }
                match parent_of.get(&id) {
                    Some(&pid) => id = pid,
                    None => break,
                }
            }
            stack.reverse();
            stack
        };

        // Convert samples + timeDeltas into Speedscope evented format
        let mut events: Vec<SpeedscopeEvent> = Vec::new();
        let mut time = 0.0f64;
        let mut prev_stack: Vec<usize> = Vec::new();

        for (i, &sample_id) in profile.samples.iter().enumerate() {
            let stack = build_stack(sample_id);
            let delta = profile.time_deltas[i] / 1000.0; // microseconds → ms

            // Find common prefix length
            let common = prev_stack
                .iter()
                .zip(stack.iter())
                .take_while(|(a, b)| a == b)
                .count();

            // Close frames popped from previous stack
            for j in (common..prev_stack.len()).rev() {
                events.push(SpeedscopeEvent {
                    event_type: "C".into(),
                    at: time,
                    frame: prev_stack[j],
                });
            }
            // Open new frames pushed onto stack
            for &frame_idx in &stack[common..] {
                events.push(SpeedscopeEvent {
                    event_type: "O".into(),
                    at: time,
                    frame: frame_idx,
                });
            }

            time += delta;
            prev_stack = stack;
        }

        // Close remaining frames
        for j in (0..prev_stack.len()).rev() {
            events.push(SpeedscopeEvent {
                event_type: "C".into(),
                at: time,
                frame: prev_stack[j],
            });
        }

        // Now process events the same way as Speedscope loading
        let n = frames.len();
        let mut inclusive = vec![0.0f64; n];
        let mut exclusive = vec![0.0f64; n];
        let mut children: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
        let mut parents: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
        let mut stacks: Vec<(Vec<usize>, f64)> = Vec::new();
        let mut total_time = 0.0f64;

        let mut stack: Vec<(usize, f64)> = Vec::new();
        for event in &events {
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
                            let mut trace: Vec<usize> =
                                stack.iter().map(|(idx, _)| *idx).collect();
                            trace.push(opened_idx);
                            stacks.push((trace, duration));
                            if stack.is_empty() {
                                total_time += duration;
                            }
                        }
                    }
                }
                _ => {}
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
            thread_count: 1,
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

        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
                sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
                sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
        roots.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        roots.truncate(n);

        let total = self.total_time.max(0.001);
        let mut out = String::new();
        let mut visited = std::collections::HashSet::new();
        for (idx, _) in &roots {
            self.tree_recurse(*idx, 0, total, &mut out, 5, &mut visited);
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
        visited: &mut std::collections::HashSet<usize>,
    ) {
        if depth > max_depth || !visited.insert(idx) {
            return;
        }
        let indent = "  ".repeat(depth);
        let pct = self.inclusive[idx] / total * 100.0;
        if pct < 0.5 {
            visited.remove(&idx);
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
            sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (child_idx, _) in sorted {
                self.tree_recurse(child_idx, depth + 1, total, out, max_depth, visited);
            }
        }
        visited.remove(&idx);
    }

    /// Pseudo-frame names emitted by V8 (`(root)`, `(program)`,
    /// `(idle)`, `(garbage collector)`) and some Speedscope producers.
    /// A stack consisting only of these is not a call path — it's
    /// runtime overhead.
    fn is_trivial_pseudo_stack(&self, stack: &[usize]) -> bool {
        stack.iter().all(|&i| {
            let n = self.frames.get(i).map(|f| f.name.as_str()).unwrap_or("");
            matches!(
                n,
                "(root)" | "(program)" | "(idle)" | "(garbage collector)"
            )
        })
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

        // V8 cpuprofiles dump an enormous amount of "idle" time against
        // a single `(root)` / `(program)` / `(idle)` pseudo-frame — in
        // a sync-I/O-blocked scenario that aggregate dominates every
        // real call stack, so "hottest path" used to collapse to just
        // `→ (root)` even though the actual work was deeper. Drop
        // single-frame pseudo-stacks so the walk picks the hottest
        // real call chain instead.
        let hottest = aggregated
            .iter()
            .filter(|(stack, _)| !self.is_trivial_pseudo_stack(stack))
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .or_else(|| aggregated.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal)));

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
        return s.to_string();
    }
    // Find the last char boundary at or before max-1 to leave room for '…'
    let mut truncate_at = 0;
    for (i, _) in s.char_indices() {
        if i >= max {
            break;
        }
        truncate_at = i;
    }
    format!("{}…", &s[..truncate_at])
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
    fn cmd_hotpath_skips_v8_pseudo_root_stack() {
        // Regression: V8 cpuprofile samples attributed to the
        // `(root)` / `(program)` pseudo-nodes often aggregate to more
        // time than any real call stack — especially under sync I/O
        // where JS is blocked. `hotpath` used to collapse to
        // `→ (root)` even when a real chain like
        // `resolveConfig → readFileSync` held most of the time.
        // Trivial pseudo-stacks must be excluded so the walk picks
        // the hottest real call chain instead.
        let dir = std::env::temp_dir().join("dbg-test-profile-pseudo");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.speedscope.json");
        let doc = r#"{
            "$schema": "https://www.speedscope.app/file-format-schema.json",
            "shared": {
                "frames": [
                    {"name": "(root)"},
                    {"name": "main"},
                    {"name": "readFileSync"}
                ]
            },
            "profiles": [{
                "type": "evented",
                "name": "p",
                "unit": "milliseconds",
                "startValue": 0,
                "endValue": 20,
                "events": [
                    {"type": "O", "at": 0.0, "frame": 0},
                    {"type": "C", "at": 50.0, "frame": 0},
                    {"type": "O", "at": 50.0, "frame": 1},
                    {"type": "O", "at": 50.1, "frame": 2},
                    {"type": "C", "at": 60.0, "frame": 2},
                    {"type": "C", "at": 60.1, "frame": 1}
                ]
            }]
        }"#;
        std::fs::write(&path, doc).unwrap();
        let mut p = ProfileData::load(&path).unwrap();
        let out = p.handle_command("hotpath");
        // The `(root)` pseudo-stack aggregated to 50ms while the real
        // `main`-rooted stacks totalled ~10ms — before the filter,
        // `(root)` would win outright and the output would be just
        // `→ (root)`. With the filter, a real frame must appear.
        assert!(
            out.contains("main"),
            "hotpath must escape the pseudo-root pileup and reach \
             real call frames; got:\n{out}"
        );
        assert!(
            !out.lines().any(|l| l.trim() == "→ (root)"),
            "hotpath collapsed to pseudo `(root)` stack:\n{out}"
        );
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

    #[test]
    fn shorten_multibyte_utf8_no_panic() {
        // CJK characters are 3 bytes each in UTF-8
        let cjk = "函数名称很长的函数";
        let result = shorten(cjk, 5);
        assert!(result.ends_with('…'));
        // Must not panic — the old code would slice mid-character
    }

    #[test]
    fn shorten_emoji_no_panic() {
        let emoji = "🔥🔥🔥🔥🔥main";
        let result = shorten(emoji, 8);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn shorten_boundary_exact() {
        // Exactly at the limit — no truncation
        assert_eq!(shorten("abcde", 5), "abcde");
        // One over — truncated
        let r = shorten("abcdef", 5);
        assert!(r.ends_with('…'));
        assert!(r.len() <= 8); // 4 ASCII + 3-byte …
    }

    #[test]
    fn tree_with_recursive_profile_no_infinite_loop() {
        // Build a profile with a cycle: A -> B -> A
        // A is the root (no parents entry), B -> A creates the cycle
        let frames = vec![
            Frame { name: "A".into() },
            Frame { name: "B".into() },
        ];
        let inclusive = vec![10.0, 8.0];
        let exclusive = vec![2.0, 2.0];
        let mut children = HashMap::new();
        children.insert(0, vec![(1, 8.0)]); // A -> B
        children.insert(1, vec![(0, 6.0)]); // B -> A (cycle!)
        let mut parents = HashMap::new();
        // Only B has a parent (A) — A is a root
        parents.insert(1, vec![(0, 8.0)]);
        let stacks = vec![
            (vec![0, 1], 8.0),
        ];
        let mut p = ProfileData {
            frames,
            inclusive,
            exclusive,
            children,
            parents,
            stacks,
            total_time: 10.0,
            thread_count: 1,
            focus: None,
            ignore: None,
        };
        // This must not stack overflow — cycle detection prevents infinite recursion
        let out = p.handle_command("tree");
        assert!(out.contains("A"));
        assert!(out.contains("B"));
        // A should NOT appear twice (cycle broken)
        let a_count = out.matches("A").count();
        assert_eq!(a_count, 1, "A appeared {a_count} times — cycle not broken: {out}");
    }

    fn sample_v8_cpuprofile() -> String {
        r#"{
            "nodes": [
                {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 0, "children": [2]},
                {"id": 2, "callFrame": {"functionName": "main", "scriptId": "1", "url": "app.js", "lineNumber": 0, "columnNumber": 0}, "hitCount": 0, "children": [3, 4]},
                {"id": 3, "callFrame": {"functionName": "compute", "scriptId": "1", "url": "app.js", "lineNumber": 4, "columnNumber": 0}, "hitCount": 5, "children": []},
                {"id": 4, "callFrame": {"functionName": "sort", "scriptId": "1", "url": "app.js", "lineNumber": 8, "columnNumber": 0}, "hitCount": 3, "children": []}
            ],
            "startTime": 0,
            "endTime": 80000,
            "samples": [3, 3, 3, 3, 3, 4, 4, 4],
            "timeDeltas": [10000, 10000, 10000, 10000, 10000, 10000, 10000, 10000]
        }"#
        .to_string()
    }

    #[test]
    fn load_v8_cpuprofile_parses_frames() {
        let p = ProfileData::load_v8_cpuprofile(&sample_v8_cpuprofile()).unwrap();
        assert_eq!(p.frames.len(), 4);
        assert_eq!(p.frames[0].name, "(root)");
        assert!(p.frames[1].name.contains("main"));
        assert!(p.frames[1].name.contains("app.js:1"));
        assert!(p.frames[2].name.contains("compute"));
        assert!(p.frames[3].name.contains("sort"));
    }

    #[test]
    fn load_v8_cpuprofile_has_time() {
        let p = ProfileData::load_v8_cpuprofile(&sample_v8_cpuprofile()).unwrap();
        assert!(p.total_time > 0.0);
        // compute is sampled 5 times at 10ms each, sort 3 times at 10ms each
        assert!(p.inclusive[2] > 0.0, "compute should have inclusive time");
        assert!(p.inclusive[3] > 0.0, "sort should have inclusive time");
    }

    #[test]
    fn load_v8_cpuprofile_commands_work() {
        let mut p = ProfileData::load_v8_cpuprofile(&sample_v8_cpuprofile()).unwrap();
        let top = p.handle_command("top");
        assert!(top.contains("compute"), "top should list compute: {top}");
        assert!(top.contains("sort"), "top should list sort: {top}");

        let callers = p.handle_command("callers compute");
        assert!(callers.contains("main"), "compute should be called by main: {callers}");

        let stats = p.handle_command("stats");
        assert!(stats.contains("frames: 4"));
    }

    /// V8 cpuprofile with recursive function: root → main → fib → fib → fib
    /// Nodes 3, 4, 5 are all "fib" at the same location but different recursion depths.
    fn sample_v8_recursive() -> String {
        r#"{
            "nodes": [
                {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 0, "children": [2]},
                {"id": 2, "callFrame": {"functionName": "main", "scriptId": "1", "url": "app.js", "lineNumber": 0, "columnNumber": 0}, "hitCount": 0, "children": [3]},
                {"id": 3, "callFrame": {"functionName": "fib", "scriptId": "1", "url": "app.js", "lineNumber": 4, "columnNumber": 0}, "hitCount": 1, "children": [4]},
                {"id": 4, "callFrame": {"functionName": "fib", "scriptId": "1", "url": "app.js", "lineNumber": 4, "columnNumber": 0}, "hitCount": 2, "children": [5]},
                {"id": 5, "callFrame": {"functionName": "fib", "scriptId": "1", "url": "app.js", "lineNumber": 4, "columnNumber": 0}, "hitCount": 3, "children": []}
            ],
            "startTime": 0,
            "endTime": 60000,
            "samples": [3, 4, 4, 5, 5, 5],
            "timeDeltas": [10000, 10000, 10000, 10000, 10000, 10000]
        }"#
        .to_string()
    }

    #[test]
    fn v8_recursive_deduplicates_frames() {
        let p = ProfileData::load_v8_cpuprofile(&sample_v8_recursive()).unwrap();
        // Nodes 3, 4, 5 are all "fib" — should be deduplicated to one frame
        let fib_count = p.frames.iter().filter(|f| f.name.contains("fib")).count();
        assert_eq!(fib_count, 1, "fib should appear once, got {fib_count}; frames: {:?}",
            p.frames.iter().map(|f| &f.name).collect::<Vec<_>>());
    }

    #[test]
    fn v8_recursive_inclusive_time_sane() {
        let p = ProfileData::load_v8_cpuprofile(&sample_v8_recursive()).unwrap();
        // No frame's inclusive time should exceed total_time
        for (i, frame) in p.frames.iter().enumerate() {
            assert!(
                p.inclusive[i] <= p.total_time * 1.01, // 1% tolerance for float
                "frame '{}' inclusive {:.2}ms exceeds total {:.2}ms",
                frame.name, p.inclusive[i], p.total_time
            );
        }
    }

    #[test]
    fn v8_recursive_search_finds_one() {
        let mut p = ProfileData::load_v8_cpuprofile(&sample_v8_recursive()).unwrap();
        let out = p.handle_command("search fib");
        assert!(out.contains("1 matches"), "should find 1 match, got: {out}");
    }

    #[test]
    fn v8_recursive_callers_no_self() {
        let mut p = ProfileData::load_v8_cpuprofile(&sample_v8_recursive()).unwrap();
        let out = p.handle_command("callers fib");
        // fib should be called by main, but NOT by itself
        assert!(out.contains("main"), "fib should be called by main: {out}");
        // Check caller lines (skip "callers of ..." header) for self-calls
        let caller_lines: Vec<&str> = out.lines()
            .filter(|l| l.contains("ms") && !l.starts_with("callers of"))
            .collect();
        for line in &caller_lines {
            assert!(!line.contains("fib"), "fib should not self-call after collapse: {line}");
        }
    }

    #[test]
    fn v8_recursive_callees_is_leaf() {
        let mut p = ProfileData::load_v8_cpuprofile(&sample_v8_recursive()).unwrap();
        let out = p.handle_command("callees fib");
        // After recursion collapse, fib has no callees (only called itself)
        assert!(out.contains("leaf") || !out.contains("fib ("),
            "fib should be leaf or not self-callee: {out}");
    }

    /// V8 cpuprofile with mutual recursion: root → a → b → a → b
    /// Tests that A→B→A doesn't collapse (they're different functions).
    fn sample_v8_mutual_recursion() -> String {
        r#"{
            "nodes": [
                {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 0, "children": [2]},
                {"id": 2, "callFrame": {"functionName": "isEven", "scriptId": "1", "url": "app.js", "lineNumber": 0, "columnNumber": 0}, "hitCount": 1, "children": [3]},
                {"id": 3, "callFrame": {"functionName": "isOdd", "scriptId": "1", "url": "app.js", "lineNumber": 4, "columnNumber": 0}, "hitCount": 1, "children": [4]},
                {"id": 4, "callFrame": {"functionName": "isEven", "scriptId": "1", "url": "app.js", "lineNumber": 0, "columnNumber": 0}, "hitCount": 1, "children": [5]},
                {"id": 5, "callFrame": {"functionName": "isOdd", "scriptId": "1", "url": "app.js", "lineNumber": 4, "columnNumber": 0}, "hitCount": 1, "children": []}
            ],
            "startTime": 0,
            "endTime": 40000,
            "samples": [2, 3, 4, 5],
            "timeDeltas": [10000, 10000, 10000, 10000]
        }"#
        .to_string()
    }

    #[test]
    fn v8_mutual_recursion_preserves_both() {
        let p = ProfileData::load_v8_cpuprofile(&sample_v8_mutual_recursion()).unwrap();
        // isEven and isOdd are different functions — both should exist
        let even = p.frames.iter().filter(|f| f.name.contains("isEven")).count();
        let odd = p.frames.iter().filter(|f| f.name.contains("isOdd")).count();
        assert_eq!(even, 1, "isEven should appear once");
        assert_eq!(odd, 1, "isOdd should appear once");
    }

    #[test]
    fn v8_mutual_recursion_callers_correct() {
        let mut p = ProfileData::load_v8_cpuprofile(&sample_v8_mutual_recursion()).unwrap();
        let callers = p.handle_command("callers isOdd");
        assert!(callers.contains("isEven"), "isOdd should be called by isEven: {callers}");
    }

    #[test]
    fn load_auto_detects_v8_format() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER2: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER2.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dbg-test-v8-{}", id));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.cpuprofile");
        std::fs::write(&path, sample_v8_cpuprofile()).unwrap();
        let p = ProfileData::load(&path).unwrap();
        assert_eq!(p.frames.len(), 4);
        assert!(p.frames[2].name.contains("compute"));
    }
}
