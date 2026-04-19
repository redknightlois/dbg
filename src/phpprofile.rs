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
    /// Extra event counters beyond the first two (indexed by position in
    /// the `events:` header after skipping the first two columns). Used
    /// to surface native callgrind events like Bcm (branch mispredictions),
    /// Bi (indirect branches), Bim, etc.
    pub extra_self: Vec<u64>,
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
    /// Event column names from the `events:` header (e.g. ["Ir", "Bc", "Bcm", "Bi", "Bim"]).
    /// Empty for PHP/Xdebug profiles which don't emit `events:`.
    pub event_names: Vec<String>,
    /// Total counters for each event column, parallel to `event_names`.
    /// Populated from the `summary:` line.
    pub event_totals: Vec<u64>,
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
        // Extra per-function counters beyond time+memory (columns 3+)
        let mut fn_extra_self: HashMap<u32, Vec<u64>> = HashMap::new();

        let mut current_fl: u32 = 0;
        let mut current_fn: u32 = 0;
        let mut _current_cfl: u32 = 0;
        let mut current_cfn: u32 = 0;
        let mut pending_call_count: u64 = 0;
        let mut total_time: u64 = 0;
        let mut total_memory: i64 = 0;
        let mut command = String::new();
        // Event column names from `events:` header
        let mut event_names: Vec<String> = Vec::new();
        // Total counters from `summary:` line
        let mut event_totals: Vec<u64> = Vec::new();

        // Registry for bare-name callgrind format (e.g. stackprof output)
        let mut bare_name_ids: HashMap<String, u32> = HashMap::new();
        let mut next_bare_id: u32 = 500_000;

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
            {
                continue;
            }
            // `events:` — capture column names for native callgrind profiles
            // (e.g. "Ir Bc Bcm Bi Bim"). PHP/Xdebug profiles use longer names
            // like "Time_(10ns) Memory_(bytes)" and may also emit this line.
            if let Some(rest) = line.strip_prefix("events:") {
                event_names = rest
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                continue;
            }

            // Summary line — capture ALL event totals (not just first two)
            if let Some(rest) = line.strip_prefix("summary: ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(t) = parts.first() {
                    total_time = t.parse().unwrap_or(0);
                }
                if let Some(m) = parts.get(1) {
                    total_memory = m.parse().unwrap_or(0);
                }
                // Capture all columns for later display
                event_totals = parts
                    .iter()
                    .filter_map(|s| s.parse::<u64>().ok())
                    .collect();
                continue;
            }

            // fl=(id) [name] or fl=bare_name
            if let Some(rest) = line.strip_prefix("fl=") {
                if let Some((id_str, name)) = parse_id_assignment(rest, &mut bare_name_ids, &mut next_bare_id) {
                    current_fl = id_str;
                    if !name.is_empty() {
                        file_names.insert(id_str, name.to_string());
                    }
                }
                continue;
            }

            // fn=(id) [name] or fn=bare_name
            if let Some(rest) = line.strip_prefix("fn=") {
                if let Some((id_str, name)) = parse_id_assignment(rest, &mut bare_name_ids, &mut next_bare_id) {
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

            // cfl=(id) [name] or cfl=bare_name
            if let Some(rest) = line.strip_prefix("cfl=") {
                if let Some((id_str, name)) = parse_id_assignment(rest, &mut bare_name_ids, &mut next_bare_id) {
                    _current_cfl = id_str;
                    if !name.is_empty() {
                        file_names.insert(id_str, name.to_string());
                    }
                }
                continue;
            }

            // cfn=(id) [name] or cfn=bare_name
            if let Some(rest) = line.strip_prefix("cfn=") {
                if let Some((id_str, name)) = parse_id_assignment(rest, &mut bare_name_ids, &mut next_bare_id) {
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

            // Cost line: line_number time memory [extra...]
            // Native callgrind also uses `+N` (relative offset) and `*` prefixes;
            // we handle the absolute-address form that starts with a digit here.
            // Relative-offset lines (`+N ...`) are handled below.
            let cost_parts_opt: Option<Vec<&str>> = if line
                .chars()
                .next()
                .map_or(false, |c| c.is_ascii_digit())
            {
                let p: Vec<&str> = line.split_whitespace().collect();
                if p.len() >= 2 { Some(p) } else { None }
            } else if line.starts_with('+') || line.starts_with('*') {
                // Relative-offset format: `+N time [extra...]` or `* line time [extra...]`
                let p: Vec<&str> = line.split_whitespace().collect();
                // For `+N` lines, col[0]="+N", col[1..] are counters.
                // For `* line` lines, col[0]="*", col[1]=line, col[2..] are counters.
                // In both cases we treat col[1] as time and col[2] as memory.
                if p.len() >= 2 { Some(p) } else { None }
            } else {
                None
            };
            if let Some(parts) = cost_parts_opt {
                let (time_idx, mem_idx, extra_start): (usize, usize, usize) =
                    if line.starts_with('*') {
                        (2, 3, 4) // * line time memory extra...
                    } else {
                        (1, 2, 3) // N time memory extra...  or  +N time memory extra...
                    };
                let time: u64 = parts.get(time_idx).and_then(|s| s.parse().ok()).unwrap_or(0);
                let memory: i64 = parts.get(mem_idx).and_then(|s| s.parse().ok()).unwrap_or(0);
                // Extra counters (Bcm etc.)
                let n_extra = event_names.len().saturating_sub(2);
                let extra: Vec<u64> = (0..n_extra)
                    .map(|i| {
                        parts
                            .get(extra_start + i)
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0)
                    })
                    .collect();

                if pending_call_count > 0 {
                    // This is a callee cost line — use ID placeholder, resolved after parsing
                    let callee_name = format!("__id_{}__", current_cfn);
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
                    // Accumulate extra counters
                    if !extra.is_empty() {
                        let slot = fn_extra_self.entry(current_fn).or_insert_with(|| vec![0u64; n_extra]);
                        for (i, v) in extra.iter().enumerate() {
                            if i < slot.len() {
                                slot[i] += v;
                            }
                        }
                    }
                }
            }
        }

        // Resolve callee ID placeholders now that all fn= names are known
        for calls in fn_calls.values_mut() {
            for call in calls.iter_mut() {
                if let Some(rest) = call.callee.strip_prefix("__id_") {
                    if let Some(id_str) = rest.strip_suffix("__") {
                        if let Ok(id) = id_str.parse::<u32>() {
                            if let Some(name) = fn_names.get(&id) {
                                call.callee = name.clone();
                            }
                        }
                    }
                }
            }
        }

        // Merge calls that now share the same resolved callee name
        for calls in fn_calls.values_mut() {
            let mut merged: Vec<CallRecord> = Vec::new();
            for call in calls.drain(..) {
                if let Some(existing) = merged.iter_mut().find(|c| c.callee == call.callee) {
                    existing.call_count += call.call_count;
                    existing.time += call.time;
                    existing.memory += call.memory;
                } else {
                    merged.push(call);
                }
            }
            *calls = merged;
        }

        // Build function list
        let mut functions = Vec::new();
        let mut all_fn_ids: Vec<u32> = fn_names.keys().copied().collect();
        all_fn_ids.sort();
        let n_extra = event_names.len().saturating_sub(2);

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
            let extra_self = fn_extra_self
                .get(&id)
                .cloned()
                .unwrap_or_else(|| vec![0u64; n_extra]);

            functions.push(PhpFunction {
                name,
                file,
                self_time,
                self_memory,
                inclusive_time: self_time + callee_time,
                inclusive_memory: self_memory + callee_memory,
                call_count: fn_call_count.get(&id).copied().unwrap_or(1),
                calls,
                extra_self,
            });
        }

        ProfileIndex {
            functions,
            total_time,
            total_memory,
            command,
            event_names,
            event_totals,
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
        // Surface native callgrind counters (Bcm = branch mispredictions, etc.)
        // when the `events:` header declared more than the standard two columns.
        if self.event_names.len() > 2 && !self.event_totals.is_empty() {
            out.push_str("\nHardware counters (from callgrind):\n");
            for (i, name) in self.event_names.iter().enumerate() {
                if let Some(&total) = self.event_totals.get(i) {
                    let label = match name.as_str() {
                        "Ir"  => "Instr refs",
                        "Bc"  => "Branch cond",
                        "Bcm" => "Branch mispred",
                        "Bi"  => "Indirect br",
                        "Bim" => "Indir br mispred",
                        other => other,
                    };
                    out.push_str(&format!("  {:<20} {:>14}\n", label, total));
                }
            }
            // Compute misprediction rate when both Bc and Bcm are present
            let bcm_idx = self.event_names.iter().position(|n| n == "Bcm");
            let bc_idx  = self.event_names.iter().position(|n| n == "Bc");
            if let (Some(bcm_i), Some(bc_i)) = (bcm_idx, bc_idx) {
                if let (Some(&bcm), Some(&bc)) = (self.event_totals.get(bcm_i), self.event_totals.get(bc_i)) {
                    if bc > 0 {
                        let pct = bcm as f64 / bc as f64 * 100.0;
                        out.push_str(&format!("  {:<20} {:>13.1}%\n", "Branch mispredict%", pct));
                    }
                }
            }
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
            // Surface per-function hardware counters when available
            if self.event_names.len() > 2 && !f.extra_self.is_empty() {
                let has_nonzero = f.extra_self.iter().any(|&v| v > 0);
                if has_nonzero {
                    out.push_str("  HW counters (self):\n");
                    for (i, name) in self.event_names.iter().skip(2).enumerate() {
                        if let Some(&v) = f.extra_self.get(i) {
                            if v > 0 {
                                let label = match name.as_str() {
                                    "Bcm" => "Branch mispred",
                                    "Bc"  => "Branch cond",
                                    "Bi"  => "Indirect br",
                                    "Bim" => "Indir br mispred",
                                    other => other,
                                };
                                out.push_str(&format!("    {:<20} {:>12}\n", label, v));
                            }
                        }
                    }
                }
            }
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
        let filtered: Vec<&PhpFunction> = self.filter("");
        // Build callee map: function name → Vec<(callee PhpFunction ref, time)>
        // Start from functions that are not called by anyone (roots).
        let called_names: std::collections::HashSet<&str> = filtered
            .iter()
            .flat_map(|f| f.calls.iter().map(|c| c.callee.as_str()))
            .collect();

        let mut roots: Vec<&PhpFunction> = filtered
            .iter()
            .filter(|f| !called_names.contains(f.name.as_str()) && f.inclusive_time > 0)
            .copied()
            .collect();
        if roots.is_empty() {
            // No uncalled root found (e.g. focus on mutually-recursive functions).
            // Fall back to functions with the highest inclusive time.
            roots = filtered
                .iter()
                .filter(|f| f.inclusive_time > 0)
                .copied()
                .collect();
        }
        roots.sort_by(|a, b| b.inclusive_time.cmp(&a.inclusive_time));
        roots.truncate(n);

        let fn_map: HashMap<&str, &PhpFunction> = filtered
            .iter()
            .map(|f| (f.name.as_str(), *f))
            .collect();

        let mut out = String::new();
        let mut visited = std::collections::HashSet::new();
        for root in &roots {
            self.tree_recurse(root, 0, &fn_map, &mut out, 5, &mut visited);
            visited.clear();
        }
        if out.is_empty() {
            out.push_str("no call tree found\n");
        }
        out
    }

    fn tree_recurse<'a>(
        &self,
        func: &'a PhpFunction,
        depth: usize,
        fn_map: &HashMap<&str, &'a PhpFunction>,
        out: &mut String,
        max_depth: usize,
        visited: &mut std::collections::HashSet<&'a str>,
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
        if !visited.insert(func.name.as_str()) {
            out.push_str(&format!(
                "{}{:>6.1}%  {}  (recursive)\n",
                indent, pct, func.name,
            ));
            return;
        }
        out.push_str(&format!(
            "{}{:>6.1}%  {}  ({}x)\n",
            indent, pct, func.name, func.call_count,
        ));
        let mut sorted = func.calls.clone();
        sorted.sort_by(|a, b| b.time.cmp(&a.time));
        for call in &sorted {
            if let Some(callee) = fn_map.get(call.callee.as_str()) {
                self.tree_recurse(callee, depth + 1, fn_map, out, max_depth, visited);
            }
        }
        visited.remove(func.name.as_str());
    }

    /// `hotpath` — the single most expensive call chain.
    pub fn cmd_hotpath(&self) -> String {
        let filtered: Vec<&PhpFunction> = self.filter("");
        let fn_map: HashMap<&str, &PhpFunction> = filtered
            .iter()
            .map(|f| (f.name.as_str(), *f))
            .collect();

        // Find the root with the most inclusive time (among filtered functions)
        let called_names: std::collections::HashSet<&str> = filtered
            .iter()
            .flat_map(|f| f.calls.iter().map(|c| c.callee.as_str()))
            .collect();

        let root = filtered
            .iter()
            .filter(|f| !called_names.contains(f.name.as_str()) && f.inclusive_time > 0)
            .max_by_key(|f| f.inclusive_time)
            .or_else(|| {
                // No uncalled root found (e.g. focus on mutually-recursive functions).
                // Fall back to the function with the highest inclusive time.
                filtered.iter().filter(|f| f.inclusive_time > 0).max_by_key(|f| f.inclusive_time)
            });

        let Some(root) = root else {
            return "no call data\n".to_string();
        };

        let mut out = format!(
            "hottest path ({}):\n",
            Self::format_time(root.inclusive_time)
        );
        let mut current = *root;
        let mut depth = 0;
        let mut visited = std::collections::HashSet::new();
        loop {
            let indent = "  ".repeat(depth);
            out.push_str(&format!(
                "{}→ {}  ({}, {}x)\n",
                indent,
                current.name,
                Self::format_time(current.self_time),
                current.call_count,
            ));
            if !visited.insert(current.name.as_str()) {
                out.push_str(&format!("{}  (recursive)\n", indent));
                break;
            }
            // Follow the callee with the highest *callee* inclusive
            // time — not the per-call `c.time` the parser recorded.
            // Native callgrind attributes almost no time to pseudo-
            // frames (`(below main)`, stub PLT entries) even though
            // their descendants dominate runtime, so picking by
            // `c.time` stalls the walk one level deep. Picking by
            // `callee.inclusive_time` follows the time wherever it
            // actually lives.
            //
            // Cross-recursion subtlety: when a caller's hottest edge
            // points back at an already-visited frame (A → B → A, or
            // direct self-recursion), let the walk descend to that
            // visited frame so the next iteration triggers the
            // `(recursive)` banner. If a non-visited sibling carries
            // within 50 % of the visited winner's time, prefer it so
            // mutual-recursion (A ↔ B) still shows both legs before
            // terminating on the cycle — without this bias, a
            // dominant self-loop on A would swallow B entirely.
            let max_callee = current
                .calls
                .iter()
                .filter_map(|c| fn_map.get(c.callee.as_str()).copied())
                .max_by_key(|callee| callee.inclusive_time);
            let hottest_callee = match max_callee {
                Some(winner) if visited.contains(winner.name.as_str()) => {
                    let winner_t = winner.inclusive_time;
                    current
                        .calls
                        .iter()
                        .filter_map(|c| fn_map.get(c.callee.as_str()).copied())
                        .filter(|callee| !visited.contains(callee.name.as_str()))
                        .filter(|callee| callee.inclusive_time * 2 >= winner_t)
                        .max_by_key(|callee| callee.inclusive_time)
                        .or(Some(winner))
                }
                other => other,
            };
            if let Some(next) = hottest_callee {
                current = next;
                depth += 1;
            } else {
                break;
            }
        }
        out
    }
}

fn parse_id_assignment<'a>(
    s: &'a str,
    bare_name_ids: &mut HashMap<String, u32>,
    next_bare_id: &mut u32,
) -> Option<(u32, &'a str)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Standard callgrind format: "(123) name" or "(123)"
    if s.starts_with('(') {
        let end_paren = s.find(')')?;
        let id: u32 = s[1..end_paren].parse().ok()?;
        let rest = s[end_paren + 1..].trim();
        return Some((id, rest));
    }
    // Bare name format (e.g. stackprof --callgrind): "Object#fibonacci"
    let name = s;
    let id = *bare_name_ids.entry(name.to_string()).or_insert_with(|| {
        let id = *next_bare_id;
        *next_bare_id += 1;
        id
    });
    Some((id, name))
}

/// Derive the `help` header label from the REPL prompt. The phpprofile
/// REPL is shared by callgrind / xdebug / stackprof backends — each
/// passes its own prompt (`callgrind> `, `xdebug> `, …), so stripping
/// the trailing `> ` gives the right backend name. The old code
/// hardcoded `"php-profile commands:"` for every caller, which made
/// `dbg help` on a callgrind session start with a PHP label.
pub fn help_label(prompt: &str) -> &str {
    let trimmed = prompt.trim_end_matches(char::is_whitespace);
    let trimmed = trimmed.trim_end_matches('>');
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        "php-profile"
    } else {
        trimmed
    }
}

/// Parse the tail of a `hotspots`-family REPL command into
/// `(limit, pattern)`. Accepts `N [pat]`, `--top N [pat]`, `-n N [pat]`,
/// or just `[pat]`.
///
/// Before this was factored out, the match arm did
/// `arg1.parse().unwrap_or(default)` which silently swallowed flag
/// spellings: `hotspots --top 3` parsed `--top` as the limit (fell
/// back to default 10) and treated `3` as a pattern filter.
pub fn parse_top_n_args(rest: &str, default: usize) -> (usize, String) {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if toks.is_empty() {
        return (default, String::new());
    }
    let (n, pat_toks) = match toks[0] {
        "--top" | "-n" | "-N" => {
            if toks.len() >= 2 {
                match toks[1].parse::<usize>() {
                    Ok(v) => (v, &toks[2..]),
                    Err(_) => (default, &toks[1..]),
                }
            } else {
                (default, &toks[1..])
            }
        }
        _ => match toks[0].parse::<usize>() {
            Ok(v) => (v, &toks[1..]),
            Err(_) => (default, &toks[..]),
        },
    };
    (n, pat_toks.join(" "))
}

/// Run the interactive REPL. Reads commands from stdin, writes results to stdout.
pub fn run_repl(cachegrind_path: &str, prompt: &str) -> io::Result<()> {
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
        print!("{prompt}");
        stdout.flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (cmd, rest) = match line.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (line, ""),
        };
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let arg1 = parts.first().copied().unwrap_or("");
        let arg2 = parts.get(1).copied().unwrap_or("");
        let pat = if arg2.is_empty() { arg1.to_string() } else { format!("{arg1} {arg2}") };

        let result = match cmd {
            "hotspots" => {
                let (n, pattern) = parse_top_n_args(rest, 10);
                index.cmd_hotspots(n, &pattern)
            }
            "flat" => {
                let (n, pattern) = parse_top_n_args(rest, 20);
                index.cmd_flat(n, &pattern)
            }
            "calls" if arg1.is_empty() => "usage: calls <pattern>\n".into(),
            "calls" => index.cmd_calls(&pat),
            "callers" if arg1.is_empty() => "usage: callers <pattern>\n".into(),
            "callers" => index.cmd_callers(&pat),
            "inspect" if arg1.is_empty() => "usage: inspect <pattern>\n".into(),
            "inspect" => index.cmd_inspect(&pat),
            "stats" => index.cmd_stats(arg1),
            "memory" => {
                let (n, pattern) = parse_top_n_args(rest, 10);
                index.cmd_memory(n, &pattern)
            }
            "search" if arg1.is_empty() => "usage: search <pattern>\n".into(),
            "search" => index.cmd_search(&pat),
            "tree" => {
                let (n, _) = parse_top_n_args(rest, 10);
                index.cmd_tree(n)
            }
            "hotpath" => index.cmd_hotpath(),
            "focus" if arg1.is_empty() => "usage: focus <pattern>\n".into(),
            "focus" => {
                index.focus = Some(pat.clone());
                format!("focus set: {}\n", pat)
            }
            "ignore" if arg1.is_empty() => "usage: ignore <pattern>\n".into(),
            "ignore" => {
                index.ignore = Some(pat.clone());
                format!("ignore set: {}\n", pat)
            }
            "reset" => {
                index.focus = None;
                index.ignore = None;
                "filters cleared\n".into()
            }
            "help" => format!(
                "{label} commands:\n  \
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
                 exit                 quit\n",
                label = help_label(prompt),
            ),
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
    fn help_label_tracks_prompt_not_hardcoded() {
        // Regression: the help header was hardcoded to
        // `"php-profile commands:"` inside `run_repl`, so a callgrind
        // session typing `help` saw PHP-labelled output. The label
        // must follow whichever prompt the hosting backend supplied.
        assert_eq!(help_label("callgrind> "), "callgrind");
        assert_eq!(help_label("xdebug> "), "xdebug");
        assert_eq!(help_label("stackprof> "), "stackprof");
        assert_eq!(help_label("php-profile> "), "php-profile");
        // Prompts without a trailing `>` still work.
        assert_eq!(help_label("callgrind "), "callgrind");
        // Empty / weird prompts fall back to the historical label.
        assert_eq!(help_label(""), "php-profile");
        assert_eq!(help_label(">"), "php-profile");
    }

    #[test]
    fn parse_top_n_flag_and_positional_and_pattern() {
        // Regression: the hotspots/flat/memory REPL verbs used
        // `arg1.parse().unwrap_or(default)`, so `--top N` fell back
        // to the default while the actual number got treated as a
        // pattern filter. `hotspots --top 3` returned 10 rows.
        assert_eq!(parse_top_n_args("--top 3", 10), (3, String::new()));
        assert_eq!(parse_top_n_args("-n 7 Matrix", 10), (7, "Matrix".into()));
        // Plain positional form still works.
        assert_eq!(parse_top_n_args("5 Matrix", 10), (5, "Matrix".into()));
        assert_eq!(parse_top_n_args("5", 10), (5, String::new()));
        // Empty → default, no pattern.
        assert_eq!(parse_top_n_args("", 10), (10, String::new()));
        // Leading pattern, no number → default, pattern preserved.
        assert_eq!(parse_top_n_args("Matrix", 10), (10, "Matrix".into()));
        // Pattern spanning multiple words survives.
        assert_eq!(
            parse_top_n_args("--top 4 foo bar", 10),
            (4, "foo bar".into())
        );
        // Bogus value after --top → default, token stays in pattern.
        assert_eq!(
            parse_top_n_args("--top nope Matrix", 10),
            (10, "nope Matrix".into())
        );
    }

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
        let mut bare = HashMap::new();
        let mut next = 500_000;
        let (id, name) = parse_id_assignment("(2) /tmp/demo.php", &mut bare, &mut next).unwrap();
        assert_eq!(id, 2);
        assert_eq!(name, "/tmp/demo.php");
    }

    #[test]
    fn parse_id_assignment_without_name() {
        let mut bare = HashMap::new();
        let mut next = 500_000;
        let (id, name) = parse_id_assignment("(2)", &mut bare, &mut next).unwrap();
        assert_eq!(id, 2);
        assert_eq!(name, "");
    }

    #[test]
    fn parse_id_assignment_bare_name() {
        let mut bare = HashMap::new();
        let mut next = 500_000;
        let (id, name) = parse_id_assignment("Object#fibonacci", &mut bare, &mut next).unwrap();
        assert_eq!(id, 500_000);
        assert_eq!(name, "Object#fibonacci");
        // Same name should return same id
        let (id2, _) = parse_id_assignment("Object#fibonacci", &mut bare, &mut next).unwrap();
        assert_eq!(id2, 500_000);
        // Different name gets next id
        let (id3, _) = parse_id_assignment("Object#compute", &mut bare, &mut next).unwrap();
        assert_eq!(id3, 500_001);
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
    fn cmd_hotpath_prefers_callee_inclusive_time_over_call_record_time() {
        // Regression: when a caller has two callees and the parser
        // recorded the *bigger* callee with a smaller per-call time
        // (pseudo-frames, stub PLT entries, and split cost blocks all
        // produce this shape), the traversal used to pick the
        // small-but-well-attributed callee and miss the hot leaf
        // entirely. Picking by the callee's own inclusive_time
        // follows the time regardless of how it was recorded on the
        // edge.
        let cg = "\
events: Time Memory
summary: 1000 0

fl=(1) app
fn=(1) main
1 10 0
cfn=(2) cheap_but_well_attributed
calls=1 1
1 50 0
cfn=(3) hot_but_poorly_attributed
calls=1 1
1 5 0

fl=(1)
fn=(2) cheap_but_well_attributed
1 50 0

fl=(1)
fn=(3) hot_but_poorly_attributed
1 935 0
";
        let idx = ProfileIndex::parse(cg);
        let out = idx.cmd_hotpath();
        assert!(
            out.contains("hot_but_poorly_attributed"),
            "hotpath must follow callee inclusive_time, not the call-edge \
             attribution; got:\n{out}"
        );
        assert!(
            !out.contains("cheap_but_well_attributed"),
            "hotpath walked the wrong branch:\n{out}"
        );
    }

    #[test]
    fn cmd_hotpath_descends_through_pseudo_frames() {
        // Regression: callgrind output produced by perf/native tools
        // often has pseudo-frames like `(below main)` whose explicit
        // call record has near-zero time attribution even though the
        // callee accounts for 99.9% of the program. When choosing the
        // "hottest" callee by the recorded call time, the algorithm
        // stalled one level deep on the low-time pseudo-call and never
        // reached the actual hot leaf. The traversal must prefer the
        // callee whose *own* inclusive_time dominates.
        let cg = "\
events: Time Memory
summary: 3062000000 0

fl=(1) /bin/prog
fn=(1) 0x23140
1 230 0
cfn=(2) (below main)
calls=1 1
1 60 0

fl=(1)
fn=(2) (below main)
1 60 0
cfn=(3) main
calls=1 1
1 3061999940 0

fl=(1)
fn=(3) main
1 100 0
cfn=(4) rand
calls=1000 1
1 3061999840 0

fl=(1)
fn=(4) rand
1 3061999840 0
";
        let idx = ProfileIndex::parse(cg);
        let out = idx.cmd_hotpath();
        assert!(
            out.contains("rand"),
            "hotpath must descend past `(below main)` to the real \
             hot leaf; got:\n{out}"
        );
        assert!(out.contains("main"), "main frame dropped:\n{out}");
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
    fn cmd_hotpath_terminates_on_recursion() {
        let cg = "\
events: Time Memory
summary: 1000 0

fl=(1) test.rb
fn=(1) main
1 100 0
cfn=(2)
calls=1 1
1 900 0

fn=(2) fib
1 500 0
cfn=(2)
calls=100 1
1 400 0
";
        let idx = ProfileIndex::parse(cg);
        let out = idx.cmd_hotpath();
        assert!(out.contains("recursive"), "output: {}", out);
        // Must terminate, not infinite loop
        assert!(out.lines().count() < 10);
    }

    #[test]
    fn cmd_tree_terminates_on_recursion() {
        let cg = "\
events: Time Memory
summary: 1000 0

fl=(1) test.rb
fn=(1) main
1 100 0
cfn=(2)
calls=1 1
1 900 0

fn=(2) fib
1 500 0
cfn=(2)
calls=100 1
1 400 0
";
        let idx = ProfileIndex::parse(cg);
        let out = idx.cmd_tree(5);
        assert!(out.contains("recursive"));
        assert!(out.lines().count() < 10);
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

    #[test]
    fn focus_filters_hotpath() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.focus = Some("Matrix".to_string());
        let out = idx.cmd_hotpath();
        // Should only follow Matrix-prefixed functions
        for line in out.lines().skip(1) {
            // Each "→ name" line should be a Matrix function
            if let Some(name_part) = line.trim().strip_prefix("→ ") {
                let name = name_part.split("  ").next().unwrap_or("");
                assert!(name.contains("Matrix"), "unexpected function in focused hotpath: {}", name);
            }
        }
        assert!(!out.contains("buildRandom"));
        assert!(!out.contains("{main}"));
    }

    #[test]
    fn focus_filters_tree() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.focus = Some("Matrix".to_string());
        let out = idx.cmd_tree(5);
        assert!(!out.contains("buildRandom"), "tree output: {}", out);
        assert!(!out.contains("{main}"), "tree output: {}", out);
        assert!(!out.contains("php::"), "tree output: {}", out);
        // Should contain Matrix functions
        assert!(out.contains("Matrix"), "tree output: {}", out);
    }

    #[test]
    fn ignore_filters_hotpath() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.ignore = Some("multiply".to_string());
        let out = idx.cmd_hotpath();
        assert!(!out.contains("multiply"), "hotpath output: {}", out);
    }

    #[test]
    fn ignore_filters_tree() {
        let mut idx = ProfileIndex::parse(SAMPLE);
        idx.ignore = Some("multiply".to_string());
        let out = idx.cmd_tree(5);
        assert!(!out.contains("multiply"), "tree output: {}", out);
    }

    // =========================================================================
    // StackProf / Ruby profiler (bare-name callgrind format)
    // =========================================================================
    mod stackprof {
        use super::*;

        const SAMPLE: &str = include_str!("../tests/fixtures/stackprof_callgrind_sample.out");

        #[test]
        fn parse_finds_all_functions() {
            let idx = ProfileIndex::parse(SAMPLE);
            // Set#add, Object#fibonacci, block in <top (required)>,
            // <top (required)>, Benchmark.measure, Kernel#load, Set#include?
            assert_eq!(idx.functions.len(), 7);
        }

        #[test]
        fn parse_totals() {
            let idx = ProfileIndex::parse(SAMPLE);
            assert_eq!(idx.total_time, 313470);
        }

        #[test]
        fn parse_bare_name_self_time() {
            let idx = ProfileIndex::parse(SAMPLE);
            let fib = idx.functions.iter().find(|f| f.name == "Object#fibonacci").unwrap();
            assert_eq!(fib.self_time, 16200);
        }

        #[test]
        fn parse_bare_name_calls() {
            let idx = ProfileIndex::parse(SAMPLE);
            let fib = idx.functions.iter().find(|f| f.name == "Object#fibonacci").unwrap();
            assert_eq!(fib.calls.len(), 1);
            let self_call = &fib.calls[0];
            assert_eq!(self_call.callee, "Object#fibonacci");
            assert_eq!(self_call.call_count, 2624);
        }

        #[test]
        fn parse_inclusive_time() {
            let idx = ProfileIndex::parse(SAMPLE);
            let fib = idx.functions.iter().find(|f| f.name == "Object#fibonacci").unwrap();
            // self 16200 + recursive call 262400
            assert_eq!(fib.inclusive_time, 16200 + 262400);
        }

        #[test]
        fn cmd_hotspots() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_hotspots(5, "");
            assert!(out.contains("Object#fibonacci"));
        }

        #[test]
        fn cmd_flat() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_flat(5, "");
            // fibonacci has highest self time
            let lines: Vec<&str> = out.lines().collect();
            assert!(lines[2].contains("Object#fibonacci"));
        }

        #[test]
        fn cmd_calls() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_calls("fibonacci");
            assert!(out.contains("Object#fibonacci"));
            assert!(out.contains("2624x"));
        }

        #[test]
        fn cmd_callers() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_callers("fibonacci");
            assert!(out.contains("block in <top (required)>"));
            assert!(out.contains("Object#fibonacci"));
        }

        #[test]
        fn cmd_inspect() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_inspect("fibonacci");
            assert!(out.contains("Object#fibonacci"));
            assert!(out.contains("Self:"));
            assert!(out.contains("Inclusive:"));
        }

        #[test]
        fn cmd_search() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_search("Object");
            assert!(out.contains("Object#fibonacci"));
        }

        #[test]
        fn cmd_stats() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_stats("");
            assert!(out.contains("Functions:      7"));
        }

        #[test]
        fn cmd_hotpath_terminates_on_recursion() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_hotpath();
            assert!(out.contains("hottest path"), "output: {}", out);
            // fibonacci is recursive — must not infinite loop
            assert!(out.lines().count() < 20, "output: {}", out);
            // If the hotpath reaches fibonacci→fibonacci, it should mark recursion
            if out.contains("Object#fibonacci") {
                let fib_count = out.matches("Object#fibonacci").count();
                // Should appear at most twice (enter + recursive marker)
                assert!(fib_count <= 2, "fibonacci appears {} times: {}", fib_count, out);
            }
        }

        #[test]
        fn cmd_tree_terminates_on_recursion() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_tree(5);
            assert!(out.lines().count() < 30, "output: {}", out);
        }
    }

    // =========================================================================
    // Callgrind / native profiler (valgrind numeric-ID format)
    // =========================================================================
    mod callgrind_native {
        use super::*;

        const SAMPLE: &str = include_str!("../tests/fixtures/callgrind_native_sample.out");

        #[test]
        fn parse_finds_all_functions() {
            let idx = ProfileIndex::parse(SAMPLE);
            // main, matrix_set, matrix_multiply, build_random, matrix_trace,
            // qsort_compare, do_sort
            assert_eq!(idx.functions.len(), 7);
        }

        #[test]
        fn parse_totals() {
            let idx = ProfileIndex::parse(SAMPLE);
            assert_eq!(idx.total_time, 2473500);
        }

        #[test]
        fn parse_self_time() {
            let idx = ProfileIndex::parse(SAMPLE);
            let mul = idx.functions.iter().find(|f| f.name == "matrix_multiply").unwrap();
            assert_eq!(mul.self_time, 175000);
        }

        #[test]
        fn parse_forward_ref_callee() {
            let idx = ProfileIndex::parse(SAMPLE);
            // main calls build_random (cfn=(4) appears before fn=(4) is defined)
            let main = idx.functions.iter().find(|f| f.name == "main").unwrap();
            let br = main.calls.iter().find(|c| c.callee == "build_random").unwrap();
            assert_eq!(br.call_count, 2);
        }

        #[test]
        fn parse_back_ref_callee() {
            let idx = ProfileIndex::parse(SAMPLE);
            // matrix_multiply calls matrix_set (cfn=(2) after fn=(2) defined)
            let mul = idx.functions.iter().find(|f| f.name == "matrix_multiply").unwrap();
            let set = mul.calls.iter().find(|c| c.callee == "matrix_set").unwrap();
            assert_eq!(set.call_count, 900);
        }

        #[test]
        fn parse_recursive_calls() {
            let idx = ProfileIndex::parse(SAMPLE);
            let qsort = idx.functions.iter().find(|f| f.name == "qsort_compare").unwrap();
            assert_eq!(qsort.calls.len(), 1);
            assert_eq!(qsort.calls[0].callee, "qsort_compare");
            assert_eq!(qsort.calls[0].call_count, 3100);
        }

        #[test]
        fn cmd_hotspots() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_hotspots(5, "");
            assert!(out.contains("main"));
            assert!(out.contains("qsort_compare"));
        }

        #[test]
        fn cmd_flat() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_flat(5, "");
            assert!(out.contains("matrix_multiply"));
        }

        #[test]
        fn cmd_calls() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_calls("main");
            assert!(out.contains("build_random"));
            assert!(out.contains("matrix_multiply"));
            assert!(out.contains("matrix_trace"));
        }

        #[test]
        fn cmd_callers() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_callers("matrix_set");
            assert!(out.contains("matrix_multiply"));
            assert!(out.contains("build_random"));
        }

        #[test]
        fn cmd_inspect() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_inspect("qsort_compare");
            assert!(out.contains("Self:"));
            assert!(out.contains("Callees:"));
            assert!(out.contains("qsort_compare"));
        }

        #[test]
        fn cmd_search() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_search("matrix");
            assert!(out.contains("matrix_multiply"));
            assert!(out.contains("matrix_set"));
            assert!(out.contains("matrix_trace"));
        }

        #[test]
        fn cmd_stats() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_stats("");
            assert!(out.contains("Functions:      7"));
        }

        #[test]
        fn cmd_hotpath_terminates_on_recursion() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_hotpath();
            assert!(out.contains("hottest path"), "output: {}", out);
            // qsort_compare is recursive — must not infinite loop
            assert!(out.lines().count() < 20, "output: {}", out);
            if out.contains("qsort_compare") {
                let count = out.matches("qsort_compare").count();
                assert!(count <= 2, "qsort_compare appears {} times: {}", count, out);
            }
        }

        #[test]
        fn cmd_tree_terminates_on_recursion() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_tree(5);
            assert!(out.lines().count() < 30, "output: {}", out);
            // Recursive functions should be marked
            if out.contains("qsort_compare") {
                // Should appear but not blow up
                let count = out.matches("qsort_compare").count();
                assert!(count <= 3, "qsort_compare appears {} times: {}", count, out);
            }
        }

        #[test]
        fn cmd_hotpath_follows_through_forward_refs() {
            let idx = ProfileIndex::parse(SAMPLE);
            let out = idx.cmd_hotpath();
            // main→build_random or main→matrix_multiply should be reachable
            // (not broken by forward-ref callee resolution)
            assert!(out.contains("main"), "output: {}", out);
            let lines = out.lines().count();
            // Should follow at least 2 levels deep
            assert!(lines >= 3, "hotpath too shallow ({}): {}", lines, out);
        }
    }

    // =========================================================================
    // Callgrind with multiple hardware counters (Ir Bc Bcm Bi Bim)
    // =========================================================================
    mod callgrind_branch_counters {
        use super::*;

        /// Minimal callgrind profile with 5-event columns: Ir Bc Bcm Bi Bim.
        /// Exercises the branch-misprediction surfacing added for scenario 14.
        const MULTI_EVENT: &str = "\
version: 1
creator: callgrind-3.25.1
pid: 999
cmd: ./bench
part: 1
positions: line

events: Ir Bc Bcm Bi Bim

fl=(1) /tmp/bench.c
fn=(1) classify
10 1000000 500000 50000 100 5
+1 200000 100 10 0 0

fl=(1)
fn=(2) main
30 5000 100 2 50 1

summary: 1205000 600200 50012 150 6
";

        #[test]
        fn parse_event_names() {
            let idx = ProfileIndex::parse(MULTI_EVENT);
            assert_eq!(
                idx.event_names,
                vec!["Ir", "Bc", "Bcm", "Bi", "Bim"],
                "event_names must reflect the events: header"
            );
        }

        #[test]
        fn parse_event_totals_from_summary() {
            let idx = ProfileIndex::parse(MULTI_EVENT);
            assert_eq!(
                idx.event_totals,
                vec![1205000, 600200, 50012, 150, 6],
                "event_totals must reflect all 5 summary columns"
            );
        }

        #[test]
        fn cmd_stats_surfaces_branch_mispred() {
            let idx = ProfileIndex::parse(MULTI_EVENT);
            let out = idx.cmd_stats("");
            assert!(
                out.contains("Branch mispred"),
                "cmd_stats must show branch mispredictions when Bcm is present:\n{out}"
            );
            // Total Bcm from summary line is 50012
            assert!(
                out.contains("50012"),
                "cmd_stats must include the total Bcm count (50012):\n{out}"
            );
        }

        #[test]
        fn cmd_stats_shows_mispredict_rate() {
            let idx = ProfileIndex::parse(MULTI_EVENT);
            let out = idx.cmd_stats("");
            assert!(
                out.contains("Branch mispredict%"),
                "cmd_stats must show mispredict % when both Bc and Bcm are present:\n{out}"
            );
        }

        #[test]
        fn per_function_bcm_surfaced_in_inspect() {
            let idx = ProfileIndex::parse(MULTI_EVENT);
            let out = idx.cmd_inspect("classify");
            assert!(
                out.contains("Branch mispred"),
                "cmd_inspect must show per-function Bcm when non-zero:\n{out}"
            );
            // The classify function has Bcm=50000+10=50010 from its two cost lines
            assert!(
                out.contains("50010") || out.contains("50000"),
                "cmd_inspect must show a Bcm value near 50000 for classify:\n{out}"
            );
        }

        #[test]
        fn stats_no_hw_section_for_standard_php_profile() {
            // PHP/Xdebug profiles emit events: Time_(10ns) Memory_(bytes).
            // They have exactly 2 columns — the HW counters section must NOT appear.
            let php = "events: Time_(10ns) Memory_(bytes)\n\
                       fl=(1) /app/index.php\nfn=(1) main\n1 5000 1024\n\
                       summary: 5000 1024\n";
            let idx = ProfileIndex::parse(php);
            let out = idx.cmd_stats("");
            assert!(
                !out.contains("Hardware counters"),
                "PHP profiles must not show a hardware-counters section:\n{out}"
            );
        }
    }

    // =========================================================================
    // Xdebug / PHP profiler — recursion-specific tests
    // (basic parsing already covered by top-level tests using SAMPLE)
    // =========================================================================
    mod xdebug_recursion {
        use super::*;

        /// PHP-style recursive function (e.g. recursive directory scan)
        const RECURSIVE_PHP: &str = "\
events: Time_(10ns) Memory_(bytes)

fl=(1) /app/scan.php
fn=(1) {main}
1 500 0
cfl=(1)
cfn=(2)
calls=1 5
1 90000 40000

fl=(1)
fn=(2) scan_dir
5 3000 1000
cfl=(1)
cfn=(2)
calls=50 5
6 85000 38000
cfl=(1)
cfn=(3)
calls=200 20
10 2000 1000

fl=(1)
fn=(3) process_file
20 2000 1000

summary: 182500 80000
";

        #[test]
        fn parse_finds_all_functions() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            assert_eq!(idx.functions.len(), 3);
        }

        #[test]
        fn parse_recursive_self_call() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let scan = idx.functions.iter().find(|f| f.name == "scan_dir").unwrap();
            let self_call = scan.calls.iter().find(|c| c.callee == "scan_dir").unwrap();
            assert_eq!(self_call.call_count, 50);
        }

        #[test]
        fn cmd_hotpath_terminates() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_hotpath();
            assert!(out.contains("hottest path"), "output: {}", out);
            assert!(out.contains("recursive"), "output: {}", out);
            assert!(out.lines().count() < 10, "output: {}", out);
        }

        #[test]
        fn cmd_tree_terminates() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_tree(5);
            assert!(out.contains("recursive"), "output: {}", out);
            assert!(out.lines().count() < 15, "output: {}", out);
        }

        #[test]
        fn cmd_hotspots() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_hotspots(5, "");
            assert!(out.contains("scan_dir"));
        }

        #[test]
        fn cmd_calls() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_calls("scan_dir");
            assert!(out.contains("scan_dir"));
            assert!(out.contains("50x"));
            assert!(out.contains("process_file"));
        }

        #[test]
        fn cmd_callers() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_callers("scan_dir");
            assert!(out.contains("{main}"));
            assert!(out.contains("scan_dir")); // recursive caller
        }

        #[test]
        fn cmd_inspect() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_inspect("scan_dir");
            assert!(out.contains("Self:"));
            assert!(out.contains("Inclusive:"));
            assert!(out.contains("Callees:"));
        }

        #[test]
        fn cmd_stats() {
            let idx = ProfileIndex::parse(RECURSIVE_PHP);
            let out = idx.cmd_stats("");
            assert!(out.contains("Functions:      3"));
        }

        #[test]
        fn focus_on_recursive_fn_shows_hotpath() {
            let mut idx = ProfileIndex::parse(RECURSIVE_PHP);
            idx.focus = Some("scan_dir".to_string());
            let out = idx.cmd_hotpath();
            assert!(out.contains("scan_dir"), "output: {}", out);
            assert!(out.contains("recursive"), "output: {}", out);
        }

        #[test]
        fn focus_on_recursive_fn_shows_tree() {
            let mut idx = ProfileIndex::parse(RECURSIVE_PHP);
            idx.focus = Some("scan_dir".to_string());
            let out = idx.cmd_tree(5);
            assert!(out.contains("scan_dir"), "output: {}", out);
            assert!(!out.contains("{main}"), "output: {}", out);
            assert!(!out.contains("process_file"), "output: {}", out);
        }
    }

    // =========================================================================
    // Cross-recursion with focus/ignore filters
    // =========================================================================
    mod cross_recursion_filters {
        use super::*;

        /// mutual_a ↔ mutual_b cross-recursion, plus fibonacci (self-recursive)
        const CROSS_RECURSIVE: &str = "\
events: Time_(10ns) Memory_(bytes)

fl=(1) /app/test.php
fn=(1) {main}
1 500 0
cfn=(2)
calls=10 5
1 30000 0
cfn=(4)
calls=5 20
1 50000 0

fl=(1)
fn=(2) mutual_a
5 2000 0
cfn=(3)
calls=20000 10
6 15000 0
cfn=(2)
calls=20000 10
6 13000 0

fl=(1)
fn=(3) mutual_b
10 1500 0
cfn=(2)
calls=20000 5
11 14000 0

fl=(1)
fn=(4) fibonacci
20 25000 0
cfn=(4)
calls=5000000 20
20 25000 0

summary: 176000 0
";

        #[test]
        fn parse_cross_recursion() {
            let idx = ProfileIndex::parse(CROSS_RECURSIVE);
            let a = idx.functions.iter().find(|f| f.name == "mutual_a").unwrap();
            let b_call = a.calls.iter().find(|c| c.callee == "mutual_b").unwrap();
            assert_eq!(b_call.call_count, 20000);
            let b = idx.functions.iter().find(|f| f.name == "mutual_b").unwrap();
            let a_call = b.calls.iter().find(|c| c.callee == "mutual_a").unwrap();
            assert_eq!(a_call.call_count, 20000);
        }

        #[test]
        fn hotpath_terminates_with_cross_recursion() {
            let idx = ProfileIndex::parse(CROSS_RECURSIVE);
            let out = idx.cmd_hotpath();
            assert!(out.lines().count() < 15, "output: {}", out);
        }

        #[test]
        fn tree_terminates_with_cross_recursion() {
            let idx = ProfileIndex::parse(CROSS_RECURSIVE);
            let out = idx.cmd_tree(5);
            assert!(out.lines().count() < 20, "output: {}", out);
        }

        #[test]
        fn focus_mutual_shows_cross_recursive_hotpath() {
            let mut idx = ProfileIndex::parse(CROSS_RECURSIVE);
            idx.focus = Some("mutual".to_string());
            let out = idx.cmd_hotpath();
            // Should find mutual_a as root (highest inclusive time among mutual_*)
            assert!(out.contains("mutual_a"), "output: {}", out);
            assert!(out.contains("mutual_b"), "output: {}", out);
            assert!(out.contains("recursive"), "output: {}", out);
            assert!(!out.contains("fibonacci"), "output: {}", out);
            assert!(!out.contains("{main}"), "output: {}", out);
        }

        #[test]
        fn focus_mutual_shows_cross_recursive_tree() {
            let mut idx = ProfileIndex::parse(CROSS_RECURSIVE);
            idx.focus = Some("mutual".to_string());
            let out = idx.cmd_tree(5);
            assert!(out.contains("mutual_a"), "output: {}", out);
            assert!(out.contains("mutual_b"), "output: {}", out);
            assert!(!out.contains("fibonacci"), "output: {}", out);
            assert!(!out.contains("{main}"), "output: {}", out);
        }

        #[test]
        fn ignore_fibonacci_keeps_mutual() {
            let mut idx = ProfileIndex::parse(CROSS_RECURSIVE);
            idx.ignore = Some("fibonacci".to_string());
            let out = idx.cmd_tree(5);
            assert!(out.contains("mutual_a"), "output: {}", out);
            assert!(!out.contains("fibonacci"), "output: {}", out);
        }

        #[test]
        fn ignore_mutual_keeps_fibonacci() {
            let mut idx = ProfileIndex::parse(CROSS_RECURSIVE);
            idx.ignore = Some("mutual".to_string());
            let out = idx.cmd_hotpath();
            assert!(!out.contains("mutual"), "output: {}", out);
            assert!(out.contains("fibonacci"), "output: {}", out);
        }
    }
}
