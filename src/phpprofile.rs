//! Cachegrind/Xdebug profile parser and interactive REPL.
//!
//! Parses Xdebug profiler output (cachegrind format) into structured
//! function records, then provides a command-line interface for querying them.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

/// A call from one function to another.
#[derive(Debug, Clone)]
pub struct CallRecord {
    /// Callee function name.
    pub callee: String,
    /// Number of times called.
    pub call_count: u64,
    /// Inclusive time of the call.
    pub time: u64,
    /// Inclusive memory of the call.
    pub memory: i64,
}

/// A profiled PHP function.
#[derive(Debug, Clone)]
pub struct PhpFunction {
    /// Function name, e.g. "Matrix->multiply" or "php::array_fill"
    pub name: String,
    /// Source file path.
    pub file: String,
    /// Self (exclusive) time in 10ns units.
    pub self_time: u64,
    /// Self (exclusive) memory in bytes.
    pub self_memory: i64,
    /// Inclusive time (self + callees).
    pub inclusive_time: u64,
    /// Inclusive memory (self + callees).
    pub inclusive_memory: i64,
    /// Number of times this function was invoked (number of cost blocks).
    pub call_count: u64,
    /// Functions called by this one.
    pub calls: Vec<CallRecord>,
}

/// Parsed index of all functions in a cachegrind profile.
pub struct ProfileIndex {
    pub functions: Vec<PhpFunction>,
    /// Total program time.
    pub total_time: u64,
    /// Total program memory.
    pub total_memory: i64,
    /// Profiled script.
    pub command: String,
    /// Focus filter (function name substring).
    focus: Option<String>,
    /// Ignore filter (function name substring).
    ignore: Option<String>,
}

impl ProfileIndex {
    /// Parse cachegrind format text into a profile index.
    pub fn parse(text: &str) -> Self {
        // ID → name maps
        let mut file_names: HashMap<u32, String> = HashMap::new();
        let mut fn_names: HashMap<u32, String> = HashMap::new();

        // Aggregated data per function ID
        let mut fn_self_time: HashMap<u32, u64> = HashMap::new();
        let mut fn_self_memory: HashMap<u32, i64> = HashMap::new();
        let mut fn_call_count: HashMap<u32, u64> = HashMap::new();
        let mut fn_file: HashMap<u32, u32> = HashMap::new();
        let mut fn_calls: HashMap<u32, Vec<CallRecord>> = HashMap::new();

        let mut current_fl: u32 = 0;
        let mut current_fn: u32 = 0;
        let mut _current_cfl: u32 = 0;
        let mut current_cfn: u32 = 0;
        let mut pending_call_count: u64 = 0;
        let mut total_time: u64 = 0;
        let mut total_memory: i64 = 0;
        let mut command = String::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Header fields
            if let Some(rest) = line.strip_prefix("cmd: ") {
                command = rest.to_string();
                continue;
            }
            if line.starts_with("version:")
                || line.starts_with("creator:")
                || line.starts_with("part:")
                || line.starts_with("positions:")
                || line.starts_with("events:")
            {
                continue;
            }

            // Summary line
            if let Some(rest) = line.strip_prefix("summary: ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(t) = parts.first() {
                    total_time = t.parse().unwrap_or(0);
                }
                if let Some(m) = parts.get(1) {
                    total_memory = m.parse().unwrap_or(0);
                }
                continue;
            }

            // fl=(id) [name]
            if let Some(rest) = line.strip_prefix("fl=") {
                if let Some((id_str, name)) = parse_id_assignment(rest) {
                    current_fl = id_str;
                    if !name.is_empty() {
                        file_names.insert(id_str, name.to_string());
                    }
                }
                continue;
            }

            // fn=(id) [name]
            if let Some(rest) = line.strip_prefix("fn=") {
                if let Some((id_str, name)) = parse_id_assignment(rest) {
                    current_fn = id_str;
                    if !name.is_empty() {
                        fn_names.insert(id_str, name.to_string());
                    }
                    // Track file association
                    fn_file.entry(current_fn).or_insert(current_fl);
                    // Increment call count (each fn= block is one invocation)
                    *fn_call_count.entry(current_fn).or_insert(0) += 1;
                }
                continue;
            }

            // cfl=(id) [name]
            if let Some(rest) = line.strip_prefix("cfl=") {
                if let Some((id_str, name)) = parse_id_assignment(rest) {
                    _current_cfl = id_str;
                    if !name.is_empty() {
                        file_names.insert(id_str, name.to_string());
                    }
                }
                continue;
            }

            // cfn=(id) [name]
            if let Some(rest) = line.strip_prefix("cfn=") {
                if let Some((id_str, name)) = parse_id_assignment(rest) {
                    current_cfn = id_str;
                    if !name.is_empty() {
                        fn_names.insert(id_str, name.to_string());
                    }
                }
                continue;
            }

            // calls=count target_position ...
            if let Some(rest) = line.strip_prefix("calls=") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(c) = parts.first() {
                    pending_call_count = c.parse().unwrap_or(0);
                }
                continue;
            }

            // Cost line: line_number time memory
            if line.chars().next().map_or(false, |c| c.is_ascii_digit()) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let time: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let memory: i64 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

                    if pending_call_count > 0 {
                        // This is a callee cost line
                        let callee_name = fn_names.get(&current_cfn).cloned().unwrap_or_else(|| format!("fn#{}", current_cfn));
                        let calls = fn_calls.entry(current_fn).or_default();
                        // Merge with existing call to same target
                        if let Some(existing) = calls.iter_mut().find(|c| c.callee == callee_name) {
                            existing.call_count += pending_call_count;
                            existing.time += time;
                            existing.memory += memory;
                        } else {
                            calls.push(CallRecord {
                                callee: callee_name,
                                call_count: pending_call_count,
                                time,
                                memory,
                            });
                        }
                        pending_call_count = 0;
                    } else {
                        // Self cost line
                        *fn_self_time.entry(current_fn).or_insert(0) += time;
                        *fn_self_memory.entry(current_fn).or_insert(0) += memory;
                    }
                }
            }
        }

        // Build function list
        let mut functions = Vec::new();
        let mut all_fn_ids: Vec<u32> = fn_names.keys().copied().collect();
        all_fn_ids.sort();

        for id in all_fn_ids {
            let name = fn_names.get(&id).cloned().unwrap_or_else(|| format!("fn#{}", id));
            let file = fn_file
                .get(&id)
                .and_then(|fid| file_names.get(fid))
                .cloned()
                .unwrap_or_default();
            let self_time = fn_self_time.get(&id).copied().unwrap_or(0);
            let self_memory = fn_self_memory.get(&id).copied().unwrap_or(0);
            let calls = fn_calls.get(&id).cloned().unwrap_or_default();
            let callee_time: u64 = calls.iter().map(|c| c.time).sum();
            let callee_memory: i64 = calls.iter().map(|c| c.memory).sum();

            functions.push(PhpFunction {
                name,
                file,
                self_time,
                self_memory,
                inclusive_time: self_time + callee_time,
                inclusive_memory: self_memory + callee_memory,
                call_count: fn_call_count.get(&id).copied().unwrap_or(1),
                calls,
            });
        }

        ProfileIndex {
            functions,
            total_time,
            total_memory,
            command,
            focus: None,
            ignore: None,
        }
    }

    /// Filter functions whose name matches a substring (case-insensitive),
    /// respecting focus/ignore filters.
    fn filter(&self, pattern: &str) -> Vec<&PhpFunction> {
        self.functions
            .iter()
            .filter(|f| {
                if !pattern.is_empty() && pattern != "." {
                    if !f.name.to_lowercase().contains(&pattern.to_lowercase()) {
                        return false;
                    }
                }
                if let Some(ref focus) = self.focus {
                    if !f.name.to_lowercase().contains(&focus.to_lowercase()) {
                        return false;
                    }
                }
                if let Some(ref ignore) = self.ignore {
                    if f.name.to_lowercase().contains(&ignore.to_lowercase()) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    fn format_time(t: u64) -> String {
        if t >= 1_000_000 {
            format!("{:.1}ms", t as f64 / 100_000.0)
        } else if t >= 1_000 {
            format!("{:.1}µs", t as f64 / 100.0)
        } else {
            format!("{}0ns", t)
        }
    }

    fn format_memory(m: i64) -> String {
        let abs = m.unsigned_abs();
        let sign = if m < 0 { "-" } else { "" };
        if abs >= 1_048_576 {
            format!("{}{:.1}MB", sign, abs as f64 / 1_048_576.0)
        } else if abs >= 1_024 {
            format!("{}{:.1}KB", sign, abs as f64 / 1_024.0)
        } else {
            format!("{}{}B", sign, abs)
        }
    }

    fn format_pct(&self, time: u64) -> String {
        if self.total_time > 0 {
            format!("{:.1}%", time as f64 / self.total_time as f64 * 100.0)
        } else {
            "-%".to_string()
        }
    }

    /// `hotspots [N] [pattern]` — top N functions by inclusive time.
    pub fn cmd_hotspots(&self, n: usize, pattern: &str) -> String {
        let mut matched = self.filter(pattern);
        matched.sort_by(|a, b| b.inclusive_time.cmp(&a.inclusive_time));
        let mut out = String::new();
        for f in matched.iter().take(n) {
            out.push_str(&format!(
                "{:>7} {:>8}  {:<40} {}x  {}\n",
                self.format_pct(f.inclusive_time),
                Self::format_time(f.inclusive_time),
                f.name,
                f.call_count,
                Self::format_memory(f.inclusive_memory),
            ));
        }
        if out.is_empty() {
            out.push_str("no functions found\n");
        }
        out
    }

    /// `flat [N] [pattern]` — top N functions by self time.
    pub fn cmd_flat(&self, n: usize, pattern: &str) -> String {
        let mut matched = self.filter(pattern);
        matched.sort_by(|a, b| b.self_time.cmp(&a.self_time));
        let mut out = String::new();
        out.push_str(&format!(
            "{:>7} {:>8}  {:<40} {:>8}  {}\n",
            "self%", "self", "function", "calls", "self mem"
        ));
        out.push_str(&format!("{}\n", "-".repeat(80)));
        for f in matched.iter().take(n) {
            out.push_str(&format!(
                "{:>7} {:>8}  {:<40} {:>7}x  {}\n",
                self.format_pct(f.self_time),
                Self::format_time(f.self_time),
                f.name,
                f.call_count,
                Self::format_memory(f.self_memory),
            ));
        }
        if matched.is_empty() {
            out.push_str("no functions found\n");
        }
        out
    }

    /// `calls <pattern>` — what does this function call?
    pub fn cmd_calls(&self, pattern: &str) -> String {
        let matched = self.filter(pattern);
        let mut out = String::new();
        for f in &matched {
            if f.calls.is_empty() {
                out.push_str(&format!("{}: no calls\n", f.name));
            } else {
                out.push_str(&format!("{} ({} callees):\n", f.name, f.calls.len()));
                let mut sorted = f.calls.clone();
                sorted.sort_by(|a, b| b.time.cmp(&a.time));
                for c in &sorted {
                    out.push_str(&format!(
                        "  → {:<40} {}x  {}  {}\n",
                        c.callee,
                        c.call_count,
                        Self::format_time(c.time),
                        Self::format_memory(c.memory),
                    ));
                }
            }
        }
        if out.is_empty() {
            out.push_str("no functions found\n");
        }
        out
    }

    /// `callers <pattern>` — who calls this function?
    pub fn cmd_callers(&self, pattern: &str) -> String {
        let pat_lower = pattern.to_lowercase();
        let mut out = String::new();
        for f in &self.functions {
            let hits: Vec<&CallRecord> = f
                .calls
                .iter()
                .filter(|c| c.callee.to_lowercase().contains(&pat_lower))
                .collect();
            if !hits.is_empty() {
                for c in &hits {
                    out.push_str(&format!(
                        "{:<40} → {:<30} {}x  {}\n",
                        f.name,
                        c.callee,
                        c.call_count,
                        Self::format_time(c.time),
                    ));
                }
            }
        }
        if out.is_empty() {
            out.push_str(&format!("no callers found for '{}'\n", pattern));
        }
        out
    }

    /// `stats [pattern]` — summary statistics.
    pub fn cmd_stats(&self, pattern: &str) -> String {
        let matched = self.filter(pattern);
        if matched.is_empty() {
            return "no functions found\n".into();
        }

        let total_self_time: u64 = matched.iter().map(|f| f.self_time).sum();
        let total_self_mem: i64 = matched.iter().map(|f| f.self_memory).sum();
        let total_incl_time: u64 = matched.iter().map(|f| f.inclusive_time).sum();
        let total_calls: u64 = matched.iter().map(|f| f.call_count).sum();

        let label = if pattern.is_empty() || pattern == "." {
            format!("--- {} ---", self.command)
        } else {
            format!("--- filter: {} ---", pattern)
        };

        let mut out = format!("{}\n", label);
        out.push_str(&format!("Functions:      {}\n", matched.len()));
        out.push_str(&format!("Total calls:    {}\n", total_calls));
        out.push_str(&format!(
            "Self time:      {} ({})\n",
            Self::format_time(total_self_time),
            self.format_pct(total_self_time)
        ));
        out.push_str(&format!(
            "Inclusive time: {}\n",
            Self::format_time(total_incl_time)
        ));
        out.push_str(&format!("Self memory:    {}\n", Self::format_memory(total_self_mem)));
        if self.total_time > 0 {
            out.push_str(&format!(
                "Program total:  {}  {}\n",
                Self::format_time(self.total_time),
                Self::format_memory(self.total_memory)
            ));
        }
        out
    }

    /// `inspect <pattern>` — detailed view of matching functions.
    pub fn cmd_inspect(&self, pattern: &str) -> String {
        let matched = self.filter(pattern);
        let mut out = String::new();
        for f in &matched {
            out.push_str(&format!("{}  ({})\n", f.name, f.file));
            out.push_str(&format!(
                "  Self:      {:>8}  ({})\n",
                Self::format_time(f.self_time),
                self.format_pct(f.self_time)
            ));
            out.push_str(&format!(
                "  Inclusive: {:>8}  ({})\n",
                Self::format_time(f.inclusive_time),
                self.format_pct(f.inclusive_time)
            ));
            out.push_str(&format!(
                "  Memory:    {:>8} self, {} inclusive\n",
                Self::format_memory(f.self_memory),
                Self::format_memory(f.inclusive_memory)
            ));
            out.push_str(&format!("  Calls:     {}x\n", f.call_count));
            if !f.calls.is_empty() {
                out.push_str("  Callees:\n");
                let mut sorted = f.calls.clone();
                sorted.sort_by(|a, b| b.time.cmp(&a.time));
                for c in &sorted {
                    out.push_str(&format!(
                        "    → {:<36} {}x  {}  {}\n",
                        c.callee,
                        c.call_count,
                        Self::format_time(c.time),
                        Self::format_memory(c.memory),
                    ));
                }
            }
            out.push('\n');
        }
        if out.is_empty() {
            out.push_str("no functions found\n");
        }
        out
    }

    /// `memory [N]` — top N functions by self memory allocation.
    pub fn cmd_memory(&self, n: usize, pattern: &str) -> String {
        let mut matched = self.filter(pattern);
        matched.sort_by(|a, b| b.self_memory.cmp(&a.self_memory));
        let mut out = String::new();
        for f in matched.iter().take(n) {
            if f.self_memory == 0 && f.inclusive_memory == 0 {
                continue;
            }
            out.push_str(&format!(
                "{:>8} self  {:>8} incl  {:<40} {}x\n",
                Self::format_memory(f.self_memory),
                Self::format_memory(f.inclusive_memory),
                f.name,
                f.call_count,
            ));
        }
        if out.is_empty() {
            out.push_str("no memory-allocating functions found\n");
        }
        out
    }

    /// `search <pattern>` — find functions matching a pattern.
    pub fn cmd_search(&self, pattern: &str) -> String {
        let pat_lower = pattern.to_lowercase();
        let mut matched: Vec<&PhpFunction> = self
            .functions
            .iter()
            .filter(|f| f.name.to_lowercase().contains(&pat_lower) && f.self_time > 0)
            .collect();
        if matched.is_empty() {
            return format!("no functions matching '{}'\n", pattern);
        }
        matched.sort_by(|a, b| b.inclusive_time.cmp(&a.inclusive_time));
        let mut out = format!("{} matches:\n", matched.len());
        for f in matched.iter().take(30) {
            out.push_str(&format!(
                "  {:>7}  {:<40} {}x\n",
                self.format_pct(f.inclusive_time),
                f.name,
                f.call_count,
            ));
        }
        out
    }

    /// `tree [N]` — call tree from roots, top N branches by time.
    pub fn cmd_tree(&self, n: usize) -> String {
        // Build callee map: function name → Vec<(callee PhpFunction ref, time)>
        // Start from functions that are not called by anyone (roots).
        let called_names: std::collections::HashSet<&str> = self
            .functions
            .iter()
            .flat_map(|f| f.calls.iter().map(|c| c.callee.as_str()))
            .collect();

        let mut roots: Vec<&PhpFunction> = self
            .functions
            .iter()
            .filter(|f| !called_names.contains(f.name.as_str()) && f.inclusive_time > 0)
            .collect();
        roots.sort_by(|a, b| b.inclusive_time.cmp(&a.inclusive_time));
        roots.truncate(n);

        let fn_map: HashMap<&str, &PhpFunction> = self
            .functions
            .iter()
            .map(|f| (f.name.as_str(), f))
            .collect();

        let mut out = String::new();
        for root in &roots {
            self.tree_recurse(root, 0, &fn_map, &mut out, 5);
        }
        if out.is_empty() {
            out.push_str("no call tree found\n");
        }
        out
    }

    fn tree_recurse(
        &self,
        func: &PhpFunction,
        depth: usize,
        fn_map: &HashMap<&str, &PhpFunction>,
        out: &mut String,
        max_depth: usize,
    ) {
        if depth > max_depth {
            return;
        }
        let pct = if self.total_time > 0 {
            func.inclusive_time as f64 / self.total_time as f64 * 100.0
        } else {
            0.0
        };
        if pct < 0.5 {
            return;
        }
        let indent = "  ".repeat(depth);
        out.push_str(&format!(
            "{}{:>6.1}%  {}  ({}x)\n",
            indent, pct, func.name, func.call_count,
        ));
        let mut sorted = func.calls.clone();
        sorted.sort_by(|a, b| b.time.cmp(&a.time));
        for call in &sorted {
            if let Some(callee) = fn_map.get(call.callee.as_str()) {
                self.tree_recurse(callee, depth + 1, fn_map, out, max_depth);
            }
        }
    }

    /// `hotpath` — the single most expensive call chain.
    pub fn cmd_hotpath(&self) -> String {
        let fn_map: HashMap<&str, &PhpFunction> = self
            .functions
            .iter()
            .map(|f| (f.name.as_str(), f))
            .collect();

        // Find the root with the most inclusive time
        let called_names: std::collections::HashSet<&str> = self
            .functions
            .iter()
            .flat_map(|f| f.calls.iter().map(|c| c.callee.as_str()))
            .collect();

        let root = self
            .functions
            .iter()
            .filter(|f| !called_names.contains(f.name.as_str()) && f.inclusive_time > 0)
            .max_by_key(|f| f.inclusive_time);

        let Some(root) = root else {
            return "no call data\n".to_string();
        };

        let mut out = format!(
            "hottest path ({}):\n",
            Self::format_time(root.inclusive_time)
        );
        let mut current = root;
        let mut depth = 0;
        loop {
            let indent = "  ".repeat(depth);
            out.push_str(&format!(
                "{}→ {}  ({}, {}x)\n",
                indent,
                current.name,
                Self::format_time(current.self_time),
                current.call_count,
            ));
            // Follow the callee with the highest time
            if let Some(hottest_call) = current.calls.iter().max_by_key(|c| c.time) {
                if let Some(callee) = fn_map.get(hottest_call.callee.as_str()) {
                    current = callee;
                    depth += 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        out
    }
}

fn parse_id_assignment(s: &str) -> Option<(u32, &str)> {
    // Parse "(123) name" or "(123)"
    let s = s.trim();
    if !s.starts_with('(') {
        return None;
    }
    let end_paren = s.find(')')?;
    let id: u32 = s[1..end_paren].parse().ok()?;
    let rest = s[end_paren + 1..].trim();
    Some((id, rest))
}

/// Run the interactive REPL. Reads commands from stdin, writes results to stdout.
pub fn run_repl(cachegrind_path: &str) -> io::Result<()> {
    let text = std::fs::read_to_string(cachegrind_path)?;
    let mut index = ProfileIndex::parse(&text);

    eprintln!(
        "--- ready: {} functions profiled ---",
        index.functions.len()
    );
    eprintln!("Type: help");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("php-profile> ");
        stdout.flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        let cmd = parts[0];
        let arg1 = parts.get(1).copied().unwrap_or("");
        let arg2 = parts.get(2).copied().unwrap_or("");

        let result = match cmd {
            "hotspots" => {
                let n: usize = arg1.parse().unwrap_or(10);
                index.cmd_hotspots(n, arg2)
            }
            "flat" => {
                let n: usize = arg1.parse().unwrap_or(20);
                index.cmd_flat(n, arg2)
            }
            "calls" => {
                if arg1.is_empty() {
                    "usage: calls <pattern>\n".into()
                } else {
                    let pat = if arg2.is_empty() {
                        arg1.to_string()
                    } else {
                        format!("{} {}", arg1, arg2)
                    };
                    index.cmd_calls(&pat)
                }
            }
            "callers" => {
                if arg1.is_empty() {
                    "usage: callers <pattern>\n".into()
                } else {
                    let pat = if arg2.is_empty() {
                        arg1.to_string()
                    } else {
                        format!("{} {}", arg1, arg2)
                    };
                    index.cmd_callers(&pat)
                }
            }
            "inspect" => {
                if arg1.is_empty() {
                    "usage: inspect <pattern>\n".into()
                } else {
                    let pat = if arg2.is_empty() {
                        arg1.to_string()
                    } else {
                        format!("{} {}", arg1, arg2)
                    };
                    index.cmd_inspect(&pat)
                }
            }
            "stats" => index.cmd_stats(arg1),
            "memory" => {
                let n: usize = arg1.parse().unwrap_or(10);
                index.cmd_memory(n, arg2)
            }
            "search" => {
                if arg1.is_empty() {
                    "usage: search <pattern>\n".into()
                } else {
                    let pat = if arg2.is_empty() {
                        arg1.to_string()
                    } else {
                        format!("{} {}", arg1, arg2)
                    };
                    index.cmd_search(&pat)
                }
            }
            "tree" => {
                let n: usize = arg1.parse().unwrap_or(10);
                index.cmd_tree(n)
            }
            "hotpath" => index.cmd_hotpath(),
            "focus" => {
                if arg1.is_empty() {
                    "usage: focus <pattern>\n".into()
                } else {
                    let pat = if arg2.is_empty() {
                        arg1.to_string()
                    } else {
                        format!("{} {}", arg1, arg2)
                    };
                    index.focus = Some(pat.clone());
                    format!("focus set: {}\n", pat)
                }
            }
            "ignore" => {
                if arg1.is_empty() {
                    "usage: ignore <pattern>\n".into()
                } else {
                    let pat = if arg2.is_empty() {
                        arg1.to_string()
                    } else {
                        format!("{} {}", arg1, arg2)
                    };
                    index.ignore = Some(pat.clone());
                    format!("ignore set: {}\n", pat)
                }
            }
            "reset" => {
                index.focus = None;
                index.ignore = None;
                "filters cleared\n".into()
            }
            "help" => {
                "php-profile commands:\n  \
                 hotspots [N] [pat]   top N functions by inclusive time (default 10)\n  \
                 flat [N] [pat]       top N functions by self time (default 20)\n  \
                 calls <pattern>      what does this function call?\n  \
                 callers <pattern>    who calls this function?\n  \
                 inspect <pattern>    detailed breakdown of matching functions\n  \
                 stats [pattern]      summary statistics\n  \
                 memory [N] [pat]     top N functions by memory allocation\n  \
                 search <pattern>     find functions matching a pattern\n  \
                 tree [N]             call tree from roots (top N branches)\n  \
                 hotpath              single most expensive call chain\n  \
                 focus <pattern>      filter all commands to matching functions\n  \
                 ignore <pattern>     exclude matching functions from all commands\n  \
                 reset                clear focus/ignore filters\n  \
                 help                 show this help\n  \
                 exit                 quit\n"
                    .into()
            }
            "exit" | "quit" => break,
            _ => format!(
                "unknown command: {}. Type 'help' for available commands.\n",
                cmd
            ),
        };

        print!("{}", result);
        stdout.flush()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/xdebug_cachegrind_sample.out");

    #[test]
    fn parse_finds_all_functions() {
        let idx = ProfileIndex::parse(SAMPLE);
        assert_eq!(idx.functions.len(), 11);
    }

    #[test]
    fn parse_command() {
        let idx = ProfileIndex::parse(SAMPLE);
        assert_eq!(idx.command, "/tmp/demo.php");
    }

    #[test]
    fn parse_totals() {
        let idx = ProfileIndex::parse(SAMPLE);
        assert_eq!(idx.total_time, 429023);
        assert_eq!(idx.total_memory, 480384);
    }

    #[test]
    fn parse_self_time() {
        let idx = ProfileIndex::parse(SAMPLE);
        let multiply = idx.functions.iter().find(|f| f.name == "Matrix->multiply").unwrap();
        assert_eq!(multiply.self_time, 175000);
    }

    #[test]
    fn parse_inclusive_time() {
        let idx = ProfileIndex::parse(SAMPLE);
        let multiply = idx.functions.iter().find(|f| f.name == "Matrix->multiply").unwrap();
        // self 175000 + constructor 141 + set 9500
        assert_eq!(multiply.inclusive_time, 175000 + 141 + 9500);
    }

    #[test]
    fn parse_calls() {
        let idx = ProfileIndex::parse(SAMPLE);
        let multiply = idx.functions.iter().find(|f| f.name == "Matrix->multiply").unwrap();
        assert_eq!(multiply.calls.len(), 2);
        let set_call = multiply.calls.iter().find(|c| c.callee == "Matrix->set").unwrap();
        assert_eq!(set_call.call_count, 900);
    }

    #[test]
    fn parse_main_calls_buildrandom() {
        let idx = ProfileIndex::parse(SAMPLE);
        let main = idx.functions.iter().find(|f| f.name == "main").unwrap();
        let br_calls: Vec<&CallRecord> = main.calls.iter().filter(|c| c.callee == "buildRandom").collect();
        assert_eq!(br_calls.len(), 1);
        assert_eq!(br_calls[0].call_count, 2);
    }

    #[test]
    fn cmd_hotspots_returns_sorted() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_hotspots(3, "");
        let lines: Vec<&str> = out.lines().collect();
        // {main} or main should be first (highest inclusive time)
        assert!(lines[0].contains("main") || lines[0].contains("{main}"));
    }

    #[test]
    fn cmd_flat_returns_sorted_by_self() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_flat(3, "");
        // Header + separator + data
        let lines: Vec<&str> = out.lines().collect();
        // First data line (after header+separator) should be multiply (highest self time)
        assert!(lines[2].contains("multiply"));
    }

    #[test]
    fn cmd_calls_shows_callees() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_calls("multiply");
        assert!(out.contains("Matrix->set"));
        assert!(out.contains("900x"));
    }

    #[test]
    fn cmd_callers_shows_callers() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_callers("multiply");
        assert!(out.contains("main"));
    }

    #[test]
    fn cmd_callers_buildrandom() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_callers("buildRandom");
        assert!(out.contains("main"));
        assert!(out.contains("2x"));
    }

    #[test]
    fn cmd_stats_all() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_stats("");
        assert!(out.contains("Functions:      11"));
        assert!(out.contains("/tmp/demo.php"));
    }

    #[test]
    fn cmd_stats_filtered() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_stats("Matrix");
        assert!(out.contains("filter: Matrix"));
        // Matrix->__construct, set, get, multiply, trace = 5
        assert!(out.contains("Functions:      5"));
    }

    #[test]
    fn cmd_inspect_shows_detail() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_inspect("multiply");
        assert!(out.contains("Matrix->multiply"));
        assert!(out.contains("Self:"));
        assert!(out.contains("Inclusive:"));
        assert!(out.contains("Callees:"));
        assert!(out.contains("Matrix->set"));
    }

    #[test]
    fn cmd_memory_shows_allocations() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_memory(5, "");
        assert!(out.contains("Matrix->set"));
    }

    #[test]
    fn format_time_units() {
        assert_eq!(ProfileIndex::format_time(5), "50ns");
        assert_eq!(ProfileIndex::format_time(1500), "15.0µs");
        assert_eq!(ProfileIndex::format_time(1_500_000), "15.0ms");
    }

    #[test]
    fn format_memory_units() {
        assert_eq!(ProfileIndex::format_memory(500), "500B");
        assert_eq!(ProfileIndex::format_memory(2048), "2.0KB");
        assert_eq!(ProfileIndex::format_memory(1_048_576), "1.0MB");
        assert_eq!(ProfileIndex::format_memory(-500), "-500B");
    }

    #[test]
    fn parse_id_assignment_with_name() {
        let (id, name) = parse_id_assignment("(2) /tmp/demo.php").unwrap();
        assert_eq!(id, 2);
        assert_eq!(name, "/tmp/demo.php");
    }

    #[test]
    fn parse_id_assignment_without_name() {
        let (id, name) = parse_id_assignment("(2)").unwrap();
        assert_eq!(id, 2);
        assert_eq!(name, "");
    }

    #[test]
    fn cmd_search_finds_matches() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_search("Matrix");
        assert!(out.contains("5 matches"));
        assert!(out.contains("Matrix->multiply"));
        assert!(out.contains("Matrix->set"));
    }

    #[test]
    fn cmd_search_no_matches() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_search("nonexistent");
        assert!(out.contains("no functions matching"));
    }

    #[test]
    fn cmd_search_case_insensitive() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_search("matrix");
        assert!(out.contains("5 matches"));
    }

    #[test]
    fn cmd_tree_shows_hierarchy() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_tree(5);
        assert!(out.contains("{main}"));
        assert!(out.contains("main"));
    }

    #[test]
    fn cmd_hotpath_shows_chain() {
        let idx = ProfileIndex::parse(SAMPLE);
        let out = idx.cmd_hotpath();
        assert!(out.contains("hottest path"));
        assert!(out.contains("→"));
        // Should follow the chain from root to the hottest leaf
        assert!(out.contains("main") || out.contains("{main}"));
    }

    #[test]
    fn focus_filters_hotspots() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.focus = Some("Matrix".to_string());
        let out = idx.cmd_hotspots(20, "");
        assert!(out.contains("Matrix->multiply"));
        assert!(!out.contains("buildRandom"));
        assert!(!out.contains("php::mt_rand"));
    }

    #[test]
    fn ignore_filters_hotspots() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.ignore = Some("Matrix".to_string());
        let out = idx.cmd_hotspots(20, "");
        assert!(!out.contains("Matrix->multiply"));
        assert!(!out.contains("Matrix->set"));
        assert!(out.contains("buildRandom"));
    }

    #[test]
    fn focus_and_ignore_combined() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.focus = Some("Matrix".to_string());
        idx.ignore = Some("set".to_string());
        let out = idx.cmd_hotspots(20, "");
        assert!(out.contains("Matrix->multiply"));
        assert!(!out.contains("Matrix->set"));
        assert!(!out.contains("buildRandom"));
    }

    #[test]
    fn reset_clears_filters() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.focus = Some("Matrix".to_string());
        idx.focus = None;
        idx.ignore = None;
        let out = idx.cmd_hotspots(20, "");
        assert!(out.contains("buildRandom"));
        assert!(out.contains("Matrix->multiply"));
    }
}
