use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Named ignore presets — expand to a concrete substring pattern.
fn ignore_preset(name: &str) -> Option<&'static str> {
    match name {
        // .NET thread-pool workers parked on the lifo semaphore dominate
        // dotnet-trace samples and drown the actual work. Matching any of
        // these frames excludes the whole thread from the baseline.
        "dotnet-idle" => Some("LowLevelLifoSemaphore"),
        _ => None,
    }
}

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
    /// Per root-invocation: the set of every frame that appeared inside
    /// its subtree. Lets filters drop a whole root (and all its child
    /// events) when any descendant matches — the right semantic for both
    /// "idle thread: root wraps semaphore wait" and pprof "each sample
    /// is its own invocation". Without this, the root-level close event
    /// has a minimal path that escapes a path-only filter.
    root_subtrees: Vec<HashSet<usize>>,
    /// Close events, each carrying the thread, open/close timestamps,
    /// full stack path (root → innermost), and the root-invocation index.
    events: Vec<StackEvent>,
    /// Focus filter (frame name substring)
    focus: Option<String>,
    /// Ignore filter (frame name substring)
    ignore: Option<String>,
    /// Time window (ms) — when set, top/stats restrict to events
    /// overlapping [t0, t1) and re-derive inclusive and baseline.
    window: Option<(f64, f64)>,
    /// Named windows so a user can define insert/query/truth once and
    /// switch between them with `phase use <name>`.
    phases: HashMap<String, (f64, f64)>,
}

#[derive(Clone)]
struct StackEvent {
    thread: usize,
    open_at: f64,
    close_at: f64,
    /// Stack path from root (index 0) to the frame that just closed (last).
    path: Vec<usize>,
    /// Which root invocation (index into `root_subtrees`) this event
    /// belongs to. Enables subtree-level filtering.
    root_idx: usize,
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
struct SpeedscopeProfile {
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
        let ext = path.extension().and_then(|e| e.to_str());
        Self::load_str(&content, ext)
    }

    /// Build a `ProfileData` from in-memory profile content. `ext_hint`
    /// is the original file extension (e.g. `Some("cpuprofile")`) when
    /// known — it lets V8 cpuprofile detection short-circuit when the
    /// JSON shape would otherwise be ambiguous. Used by the daemon to
    /// stash and replay profile content from the SessionDb without
    /// touching the original file.
    pub fn load_str(content: &str, ext_hint: Option<&str>) -> Result<Self> {
        // Auto-detect: V8 cpuprofile has "nodes" + "samples" at top level
        if ext_hint == Some("cpuprofile")
            || (content.contains("\"nodes\"") && content.contains("\"timeDeltas\""))
        {
            return Self::load_v8_cpuprofile(content);
        }

        // Text-profile detection. `perf script` output has tab-indented
        // stack frame lines under whitespace-prefixed sample headers;
        // `go tool pprof -traces` output is dominated by `-----+---`
        // sample separators. Both convert to speedscope in-memory and
        // fall through to the JSON path below.
        let looks_like_pprof_traces = content.contains("-----+-")
            || (content.trim_start().starts_with("File:") && content.contains("Total samples"));
        let looks_like_perf_script = !content.trim_start().starts_with('{')
            && content
                .lines()
                .take(200)
                .any(|l| l.starts_with('\t') && l.contains(' '));

        let content_owned;
        let content: &str = if looks_like_pprof_traces {
            content_owned = pprof_traces_to_speedscope(content)
                .context("failed to convert pprof -traces output to speedscope")?;
            &content_owned
        } else if looks_like_perf_script {
            content_owned = perf_script_to_speedscope(content)
                .context("failed to convert perf script output to speedscope")?;
            &content_owned
        } else {
            content
        };

        let file: SpeedscopeFile =
            serde_json::from_str(content).context("failed to parse speedscope JSON")?;

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
        let mut events: Vec<StackEvent> = Vec::new();
        let mut root_subtrees: Vec<HashSet<usize>> = Vec::new();

        for (thread_idx, profile) in file.profiles.iter().enumerate() {
            let mut stack: Vec<(usize, f64)> = Vec::new(); // (frame_idx, open_time)
            // Buffer events for the current root invocation so we can
            // backfill their root_idx once we know which subtree they
            // belong to. The root subtree set is built up as frames open.
            let mut current_root_idx: Option<usize> = None;
            let mut pending_events: Vec<StackEvent> = Vec::new();

            for event in &profile.events {
                let idx = event.frame;
                if idx >= n {
                    continue;
                }

                match event.event_type.as_str() {
                    "O" => {
                        if stack.is_empty() {
                            // Starting a new root invocation.
                            current_root_idx = Some(root_subtrees.len());
                            root_subtrees.push(HashSet::new());
                        }
                        stack.push((idx, event.at));
                        if let Some(ri) = current_root_idx {
                            root_subtrees[ri].insert(idx);
                        }
                    }
                    "C" => {
                        if let Some((opened_idx, opened_at)) = stack.pop() {
                            let duration = event.at - opened_at;
                            if duration > 0.0 {
                                inclusive[opened_idx] += duration;

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
                                stacks.push((trace.clone(), duration));
                                pending_events.push(StackEvent {
                                    thread: thread_idx,
                                    open_at: opened_at,
                                    close_at: event.at,
                                    path: trace,
                                    root_idx: current_root_idx.unwrap_or(0),
                                });

                                // Track total from root frames
                                if stack.is_empty() {
                                    total_time += duration;
                                    // Root just closed — flush buffered
                                    // events; their root_idx is final.
                                    events.append(&mut pending_events);
                                    current_root_idx = None;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Partial (unclosed) trees at end of profile — still emit.
            events.append(&mut pending_events);
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
            root_subtrees,
            events,
            focus: None,
            ignore: None,
            window: None,
            phases: HashMap::new(),
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
        let mut stack_events: Vec<StackEvent> = Vec::new();
        let mut root_subtrees: Vec<HashSet<usize>> = Vec::new();
        let mut total_time = 0.0f64;
        let mut seen: HashSet<usize> = HashSet::new();
        let mut current_root_idx: Option<usize> = None;
        let mut pending_events: Vec<StackEvent> = Vec::new();

        let mut stack: Vec<(usize, f64)> = Vec::new();
        for event in &events {
            let idx = event.frame;
            if idx >= n {
                continue;
            }
            match event.event_type.as_str() {
                "O" => {
                    if stack.is_empty() {
                        current_root_idx = Some(root_subtrees.len());
                        root_subtrees.push(HashSet::new());
                    }
                    stack.push((idx, event.at));
                    seen.insert(idx);
                    if let Some(ri) = current_root_idx {
                        root_subtrees[ri].insert(idx);
                    }
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
                            stacks.push((trace.clone(), duration));
                            pending_events.push(StackEvent {
                                thread: 0,
                                open_at: opened_at,
                                close_at: event.at,
                                path: trace,
                                root_idx: current_root_idx.unwrap_or(0),
                            });
                            if stack.is_empty() {
                                total_time += duration;
                                stack_events.append(&mut pending_events);
                                current_root_idx = None;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        stack_events.append(&mut pending_events);

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
            root_subtrees,
            events: stack_events,
            focus: None,
            ignore: None,
            window: None,
            phases: HashMap::new(),
        })
    }

    /// Baseline wall-time to normalize percentages against.
    ///
    /// With no filters: equals `total_time`. With `focus`/`ignore` active,
    /// we drop any thread whose frame set doesn't satisfy both predicates —
    /// this is what makes an idle thread-pool worker disappear from the
    /// denominator when `ignore LowLevelLifoSemaphore` is set. With a time
    /// window active, it is the overlapped wall-time of qualifying threads
    /// within the window.
    fn filtered_total(&self) -> f64 {
        if self.window.is_none() && self.focus.is_none() && self.ignore.is_none() {
            return self.total_time;
        }
        // Same per-stack rule whether or not a window is active. The
        // window is just a wide-open range when unset.
        self.window_metrics().1
    }

    /// Does this root invocation's subtree satisfy both focus and ignore?
    ///
    /// The check is over the UNION of every frame seen inside that root
    /// call — not just the single closing frame at the event. This is
    /// what makes `ignore LowLevelLifoSemaphore` drop the whole idle
    /// thread even though the root close event's own path is just
    /// `[ThreadPoolWorker]`; the subtree union includes the semaphore
    /// frame. For pprof (1 root per sample) the subtree collapses to
    /// the sample's stack. For multi-invocation general profiles each
    /// root invocation is evaluated independently — sibling subtrees
    /// survive even when one is filtered out.
    fn root_passes(&self, root_idx: usize) -> bool {
        let Some(subtree) = self.root_subtrees.get(root_idx) else {
            return true;
        };
        if let Some(f) = self.focus.as_deref() {
            if !subtree.iter().any(|&i| self.frames[i].name.contains(f)) {
                return false;
            }
        }
        if let Some(ig) = self.ignore.as_deref() {
            if subtree.iter().any(|&i| self.frames[i].name.contains(ig)) {
                return false;
            }
        }
        true
    }

    /// Per-frame inclusive within the current window (plus baseline).
    ///
    /// Events belonging to a dropped root invocation are skipped
    /// entirely — so when a root is filtered, none of its descendants
    /// leak through as ghost rows.
    fn window_metrics(&self) -> (Vec<f64>, f64) {
        let (t0, t1) = self.window.unwrap_or((f64::NEG_INFINITY, f64::INFINITY));
        let mut inc = vec![0.0f64; self.frames.len()];
        let mut baseline = 0.0f64;
        for ev in &self.events {
            if ev.close_at <= t0 || ev.open_at >= t1 {
                continue;
            }
            if !self.root_passes(ev.root_idx) {
                continue;
            }
            let overlap = ev.close_at.min(t1) - ev.open_at.max(t0);
            if overlap <= 0.0 {
                continue;
            }
            // Each frame's own close event accounts for its full
            // inclusive time, so crediting only the innermost frame
            // avoids double-count with ancestor close events.
            if let Some(&fidx) = ev.path.last() {
                inc[fidx] += overlap;
            }
            if ev.path.len() == 1 {
                baseline += overlap;
            }
        }
        (inc, baseline)
    }

    pub fn handle_command(&mut self, cmd: &str) -> String {
        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
        if parts.is_empty() {
            return String::new();
        }

        match parts[0] {
            "top" => {
                // Flags: --no-idle applies the dotnet-idle ignore preset for
                // this invocation without mutating state. Any non-flag token
                // is the row count.
                let mut n: usize = 20;
                let mut no_idle = false;
                for tok in &parts[1..] {
                    if *tok == "--no-idle" {
                        no_idle = true;
                    } else if let Ok(v) = tok.parse::<usize>() {
                        n = v;
                    }
                }
                if no_idle {
                    let prev = self.ignore.clone();
                    self.ignore = Some(ignore_preset("dotnet-idle").unwrap().to_string());
                    let out = self.cmd_top(n);
                    self.ignore = prev;
                    out
                } else {
                    self.cmd_top(n)
                }
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
                // `ignore preset <name>` expands a named preset (e.g.
                // `dotnet-idle` → "LowLevelLifoSemaphore") so the user
                // doesn't have to memorize the frame name.
                if parts.get(1) == Some(&"preset") {
                    let Some(name) = parts.get(2) else {
                        return "usage: ignore preset <name>  (presets: dotnet-idle)".to_string();
                    };
                    let Some(pat) = ignore_preset(name) else {
                        return format!("unknown preset '{name}' (known: dotnet-idle)");
                    };
                    self.ignore = Some(pat.to_string());
                    return format!("ignore set: {pat} (preset {name})");
                }
                let pattern = parts[1..].join(" ");
                if pattern.is_empty() {
                    return "usage: ignore <function-name> | ignore preset <name>".to_string();
                }
                self.ignore = Some(pattern.clone());
                format!("ignore set: {pattern}")
            }
            "window" => self.cmd_window(&parts[1..]),
            "phase" => self.cmd_phase(&parts[1..]),
            "phases" => self.cmd_phase_list(),
            "marks" => {
                let threshold: f64 = parts
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(50.0);
                self.cmd_marks(threshold)
            }
            "reset" => {
                self.focus = None;
                self.ignore = None;
                self.window = None;
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
        // Under any active filter we recompute inclusive from event
        // overlaps — otherwise the numerator leaks counts from dropped
        // samples (a timer frame from an ignored stack would still show
        // up at its pre-filter percentage against a reduced baseline).
        // Exclusive isn't meaningful in that regime without re-doing the
        // children subtraction, so we omit it and show only inclusive.
        let any_filter = self.window.is_some() || self.focus.is_some() || self.ignore.is_some();
        let (inclusive, exclusive_available, total) = if any_filter {
            let (inc, base) = self.window_metrics();
            (inc, false, base)
        } else {
            (self.inclusive.clone(), true, self.filtered_total())
        };

        // If filters eliminated the entire baseline we can't produce
        // meaningful percentages — surface that instead of dividing by
        // ~0 and printing millions-of-percent garbage.
        if total <= 0.01 {
            let active_filters = self.focus.is_some() || self.ignore.is_some() || self.window.is_some();
            if active_filters {
                return "(baseline is empty under current filters — nothing matched; try `reset` or a narrower ignore pattern)\n"
                    .to_string();
            }
        }

        let mut entries: Vec<(usize, f64, f64)> = (0..self.frames.len())
            .filter(|&i| self.matches_filter(i))
            .filter(|&i| inclusive[i] > 0.0)
            .map(|i| (i, inclusive[i], if exclusive_available { self.exclusive[i] } else { 0.0 }))
            .collect();

        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        entries.truncate(n);

        let total = total.max(0.001);
        let mut out = if exclusive_available {
            format!("{:<60} {:>10} {:>10}\n", "Function", "Inclusive", "Exclusive")
        } else {
            format!("{:<60} {:>10}\n", "Function", "Inclusive")
        };
        for (idx, inc, exc) in entries {
            let name = shorten(&self.frames[idx].name, 58);
            if exclusive_available {
                out.push_str(&format!(
                    "{:<60} {:>9.1}% {:>9.1}%\n",
                    name,
                    inc / total * 100.0,
                    exc / total * 100.0
                ));
            } else {
                out.push_str(&format!(
                    "{:<60} {:>9.1}%\n",
                    name,
                    inc / total * 100.0
                ));
            }
        }
        out
    }

    fn cmd_callers(&self, pattern: &str) -> String {
        self.cmd_neighbors(pattern, "callers", "root — no callers", &self.parents)
    }

    fn cmd_callees(&self, pattern: &str) -> String {
        self.cmd_neighbors(pattern, "callees", "leaf — no callees", &self.children)
    }

    /// Render the top-15 caller or callee frames for each match of `pattern`.
    /// `edges` maps a frame index to a list of `(neighbor_frame_idx, time_ms)`
    /// pairs — `parents` for callers, `children` for callees.
    fn cmd_neighbors(
        &self,
        pattern: &str,
        verb: &str,
        empty_label: &str,
        edges: &HashMap<usize, Vec<(usize, f64)>>,
    ) -> String {
        let targets = self.find_frames(pattern);
        if targets.is_empty() {
            return format!("no function matching '{pattern}'");
        }

        let mut out = String::new();
        for &idx in &targets {
            out.push_str(&format!("{verb} of {}:\n", self.frames[idx].name));
            let Some(neighbors) = edges.get(&idx) else {
                out.push_str(&format!("  ({empty_label})\n"));
                continue;
            };
            let mut aggregated: HashMap<usize, f64> = HashMap::new();
            for &(n_idx, time) in neighbors {
                *aggregated.entry(n_idx).or_default() += time;
            }
            let mut sorted: Vec<(usize, f64)> = aggregated.into_iter().collect();
            sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (n_idx, time) in sorted.iter().take(15) {
                out.push_str(&format!(
                    "  {:>8.2}ms  {}\n",
                    time,
                    shorten(&self.frames[*n_idx].name, 70)
                ));
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
        let baseline = self.filtered_total();
        let baseline_line = if (baseline - self.total_time).abs() > 0.001 {
            format!(
                "\nbaseline: {:.2}ms ({:.1}% of total, filters re-normalized)",
                baseline,
                baseline / self.total_time.max(0.001) * 100.0
            )
        } else {
            String::new()
        };
        let window_line = match self.window {
            Some((t0, t1)) => format!("\nwindow: [{t0:.2}ms, {t1:.2}ms)"),
            None => String::new(),
        };
        format!(
            "total time: {:.2}ms\nframes: {} ({} active)\nprofiles: {}\nstacks: {}\nfocus: {}\nignore: {}{}{}",
            self.total_time,
            self.frames.len(),
            active_frames,
            self.thread_count,
            self.stacks.len(),
            self.focus.as_deref().unwrap_or("(none)"),
            self.ignore.as_deref().unwrap_or("(none)"),
            window_line,
            baseline_line,
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

        let total = self.filtered_total().max(0.001);
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

    fn cmd_window(&mut self, args: &[&str]) -> String {
        if args.is_empty() {
            return match self.window {
                Some((t0, t1)) => format!("window: [{t0:.2}ms, {t1:.2}ms)"),
                None => "window: (none). usage: window <t0_ms> <t1_ms> | window clear".to_string(),
            };
        }
        if args[0] == "clear" {
            self.window = None;
            return "window cleared".to_string();
        }
        if args.len() < 2 {
            return "usage: window <t0_ms> <t1_ms> | window clear".to_string();
        }
        let (Ok(t0), Ok(t1)) = (args[0].parse::<f64>(), args[1].parse::<f64>()) else {
            return "usage: window <t0_ms> <t1_ms> — both args must be numbers".to_string();
        };
        if t1 <= t0 {
            return format!("invalid window: t1 ({t1}) must be > t0 ({t0})");
        }
        self.window = Some((t0, t1));
        format!("window set: [{t0:.2}ms, {t1:.2}ms)")
    }

    fn cmd_phase(&mut self, args: &[&str]) -> String {
        match args.first().copied() {
            Some("add") => {
                if args.len() < 4 {
                    return "usage: phase add <name> <t0_ms> <t1_ms>".to_string();
                }
                let name = args[1].to_string();
                let (Ok(t0), Ok(t1)) = (args[2].parse::<f64>(), args[3].parse::<f64>()) else {
                    return "phase add: t0 and t1 must be numbers".to_string();
                };
                if t1 <= t0 {
                    return format!("invalid phase: t1 ({t1}) must be > t0 ({t0})");
                }
                self.phases.insert(name.clone(), (t0, t1));
                format!("phase '{name}' = [{t0:.2}ms, {t1:.2}ms)")
            }
            Some("use") => {
                let Some(name) = args.get(1) else {
                    return "usage: phase use <name>".to_string();
                };
                let Some(&(t0, t1)) = self.phases.get(*name) else {
                    return format!("no phase named '{name}' (see `phases`)");
                };
                self.window = Some((t0, t1));
                format!("window set from phase '{name}': [{t0:.2}ms, {t1:.2}ms)")
            }
            Some("clear") => {
                self.phases.clear();
                self.window = None;
                "phases cleared".to_string()
            }
            Some("list") | None => self.cmd_phase_list(),
            Some(other) => format!("unknown: phase {other}  (add|use|list|clear)"),
        }
    }

    fn cmd_phase_list(&self) -> String {
        if self.phases.is_empty() {
            return "no phases defined (use `phase add <name> <t0> <t1>`)".to_string();
        }
        let mut names: Vec<&String> = self.phases.keys().collect();
        names.sort();
        let mut out = String::new();
        for name in names {
            let (t0, t1) = self.phases[name];
            out.push_str(&format!("  {name:<16} [{t0:>10.2}ms, {t1:>10.2}ms)\n"));
        }
        out
    }

    /// Suggest phase boundaries by finding quiet gaps on any thread —
    /// stretches with no stack activity longer than `threshold_ms`. Useful
    /// when the user hasn't emitted explicit markers: long pauses usually
    /// separate logical workload phases (insert → query → truth).
    fn cmd_marks(&self, threshold_ms: f64) -> String {
        // Bucket event intervals per thread, sort by open, then walk to find
        // gaps between end-of-previous-max-close and next-open.
        let mut per_thread: HashMap<usize, Vec<(f64, f64)>> = HashMap::new();
        for ev in &self.events {
            per_thread
                .entry(ev.thread)
                .or_default()
                .push((ev.open_at, ev.close_at));
        }
        let mut gaps: Vec<(usize, f64, f64)> = Vec::new();
        for (tid, mut ivals) in per_thread {
            ivals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut cover_end = f64::NEG_INFINITY;
            for (o, c) in ivals {
                if o - cover_end >= threshold_ms && cover_end.is_finite() {
                    gaps.push((tid, cover_end, o));
                }
                if c > cover_end {
                    cover_end = c;
                }
            }
        }
        if gaps.is_empty() {
            return format!("no gaps >= {threshold_ms:.1}ms on any thread");
        }
        gaps.sort_by(|a, b| {
            let la = a.2 - a.1;
            let lb = b.2 - b.1;
            lb.partial_cmp(&la).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut out = format!(
            "{} gap(s) >= {:.1}ms (sorted by length):\n",
            gaps.len(),
            threshold_ms
        );
        for (tid, a, b) in gaps.iter().take(20) {
            out.push_str(&format!(
                "  thread {tid:<3}  {:>10.2}ms  →  {:>10.2}ms   ({:.2}ms)\n",
                a,
                b,
                b - a
            ));
        }
        out
    }

    fn cmd_help(&self) -> String {
        "commands: top [N] [--no-idle], callers <func>, callees <func>, traces [N], tree [N], hotpath, threads, stats, search <pattern>, focus <func>, ignore <func> | ignore preset <name>, window <t0> <t1> | window clear, phase add <name> <t0> <t1> | phase use <name> | phase list | phase clear, marks [threshold_ms], reset".to_string()
    }

    fn find_frames(&self, pattern: &str) -> Vec<usize> {
        self.frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name.contains(pattern))
            .map(|(i, _)| i)
            .collect()
    }

    /// Per-frame `(name, inclusive_ms, exclusive_ms)` over the full
    /// (unfiltered) profile. Used by `dbg diff` to compare two
    /// profile sessions function-by-function. Frames with the same
    /// name are merged so V8's per-recursion-depth split nodes don't
    /// produce duplicate rows.
    pub fn frame_metrics(&self) -> Vec<(String, f64, f64)> {
        let mut by_name: HashMap<&str, (f64, f64)> = HashMap::new();
        for (i, frame) in self.frames.iter().enumerate() {
            let entry = by_name.entry(frame.name.as_str()).or_default();
            entry.0 += self.inclusive[i];
            entry.1 += self.exclusive[i];
        }
        by_name
            .into_iter()
            .map(|(name, (inc, exc))| (name.to_string(), inc, exc))
            .collect()
    }

    /// Total time across root frames — denominator for percentage
    /// reporting in `dbg diff` so a slowdown shows up as both an
    /// absolute ms increase and a relative share.
    pub fn total_ms(&self) -> f64 {
        self.total_time
    }
}

/// Parse `perf script` output into speedscope-format JSON.
///
/// Input is `comm [tid] TS.FRAC: PERIOD EVENT:` headers each followed by
/// tab-indented `IP symbol+offset (dso)` frame lines, separated by blank
/// lines. Perf prints innermost frame first; we reverse to root→leaf and
/// emit an evented speedscope profile per thread using common-prefix
/// diffing across consecutive samples (same shape as the V8 loader).
/// Builds speedscope-format JSON: interns frame names into a shared table and
/// collects per-thread/per-sample evented profiles. Used by both the perf
/// script loader and the pprof -traces loader.
struct SpeedscopeBuilder {
    frame_names: Vec<String>,
    name_to_idx: HashMap<String, usize>,
    profiles: Vec<serde_json::Value>,
}

impl SpeedscopeBuilder {
    fn new() -> Self {
        Self {
            frame_names: Vec::new(),
            name_to_idx: HashMap::new(),
            profiles: Vec::new(),
        }
    }

    fn intern(&mut self, name: String) -> usize {
        if let Some(&i) = self.name_to_idx.get(&name) {
            return i;
        }
        let i = self.frame_names.len();
        self.name_to_idx.insert(name.clone(), i);
        self.frame_names.push(name);
        i
    }

    fn add_profile(&mut self, name: String, events: Vec<serde_json::Value>) {
        self.profiles.push(serde_json::json!({
            "name": name,
            "events": events,
        }));
    }

    fn build(self) -> String {
        let frames: Vec<_> = self.frame_names.iter()
            .map(|n| serde_json::json!({"name": n}))
            .collect();
        serde_json::json!({
            "shared": { "frames": frames },
            "profiles": self.profiles,
        }).to_string()
    }
}

fn perf_script_to_speedscope(text: &str) -> Result<String> {
    use std::collections::BTreeMap;

    struct Sample {
        thread: String,
        t_ms: f64,
        stack: Vec<String>, // root → leaf
    }

    let mut samples: Vec<Sample> = Vec::new();
    let mut cur_thread: Option<String> = None;
    let mut cur_t: Option<f64> = None;
    let mut cur_stack: Vec<String> = Vec::new();

    let flush = |samples: &mut Vec<Sample>,
                 thread: &mut Option<String>,
                 t: &mut Option<f64>,
                 stack: &mut Vec<String>| {
        if let (Some(thr), Some(ts)) = (thread.take(), t.take()) {
            if !stack.is_empty() {
                stack.reverse();
                samples.push(Sample {
                    thread: thr,
                    t_ms: ts,
                    stack: std::mem::take(stack),
                });
            } else {
                stack.clear();
            }
        } else {
            stack.clear();
        }
    };

    for raw in text.lines() {
        if raw.is_empty() {
            flush(&mut samples, &mut cur_thread, &mut cur_t, &mut cur_stack);
            continue;
        }
        // Frame line: starts with tab (perf default). Tolerate leading
        // spaces some scripts emit.
        if raw.starts_with('\t') || raw.starts_with("    ") {
            // Typical: "\tIPHEX symbol+0xNN (dso)".
            let mut tokens = raw.trim().splitn(2, char::is_whitespace);
            let _ip = tokens.next();
            let rest = tokens.next().unwrap_or("");
            // Strip trailing " (dso)" if present, and "+0xNN" offset.
            let rest = rest.rsplit_once(" (").map(|(a, _)| a).unwrap_or(rest);
            let sym = rest.rsplit_once('+').map(|(a, _)| a).unwrap_or(rest);
            let sym = sym.trim();
            let name = if sym.is_empty() || sym == "[unknown]" {
                "[unknown]".to_string()
            } else {
                sym.to_string()
            };
            cur_stack.push(name);
            continue;
        }
        // Header line. Shape examples:
        //   swapper     0 [000] 1234.567:    1000 cycles:
        //   myapp   12345/12345 [002] 1234.567:    1000 cycles:
        // The timestamp is always "<secs>.<frac>:". Find it and the
        // leading comm/tid cluster.
        let Some(ts_colon_idx) = raw.find(':') else { continue };
        let before_ts = &raw[..ts_colon_idx];
        // Timestamp is the last whitespace-separated token before `:`
        // that parses as f64.
        let Some(ts_tok) = before_ts.split_whitespace().last() else { continue };
        let Ok(ts_sec) = ts_tok.parse::<f64>() else { continue };
        flush(&mut samples, &mut cur_thread, &mut cur_t, &mut cur_stack);
        // Thread id: prefer "pid/tid" → tid; else the first numeric token.
        let thread = before_ts
            .split_whitespace()
            .find_map(|tok| {
                if let Some((_, tid)) = tok.split_once('/') {
                    tid.parse::<u64>().ok().map(|n| n.to_string())
                } else {
                    tok.parse::<u64>().ok().map(|n| n.to_string())
                }
            })
            .unwrap_or_else(|| "0".to_string());
        cur_thread = Some(thread);
        cur_t = Some(ts_sec * 1000.0);
    }
    flush(&mut samples, &mut cur_thread, &mut cur_t, &mut cur_stack);

    if samples.is_empty() {
        anyhow::bail!("no samples parsed from perf script output");
    }

    // Group by thread, sort by time, then emit evented profile with
    // common-prefix open/close between consecutive samples.
    let mut by_thread: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
    for s in samples {
        by_thread.entry(s.thread.clone()).or_default().push(s);
    }

    let mut builder = SpeedscopeBuilder::new();
    for (thread, mut samples) in by_thread {
        samples.sort_by(|a, b| a.t_ms.partial_cmp(&b.t_ms).unwrap_or(std::cmp::Ordering::Equal));
        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut prev: Vec<usize> = Vec::new();
        let mut prev_time = samples.first().map(|s| s.t_ms).unwrap_or(0.0);
        for s in samples {
            let stack: Vec<usize> = s.stack.into_iter().map(|n| builder.intern(n)).collect();
            let common = prev.iter().zip(stack.iter()).take_while(|(a, b)| a == b).count();
            for j in (common..prev.len()).rev() {
                events.push(serde_json::json!({"type":"C","at":prev_time,"frame":prev[j]}));
            }
            for &idx in &stack[common..] {
                events.push(serde_json::json!({"type":"O","at":s.t_ms,"frame":idx}));
            }
            prev = stack;
            prev_time = s.t_ms;
        }
        // Close any still-open frames at the last timestamp.
        for j in (0..prev.len()).rev() {
            events.push(serde_json::json!({"type":"C","at":prev_time,"frame":prev[j]}));
        }
        builder.add_profile(format!("tid-{thread}"), events);
    }
    Ok(builder.build())
}

/// Parse `go tool pprof -traces` output into speedscope-format JSON.
///
/// Input is a header block followed by sample blocks separated by
/// `-----+---` lines. Each sample block's first line is
/// `<duration>  <innermost-frame>`, followed by indented frame lines
/// (still innermost-first). No real timestamps are available, so we
/// lay samples end-to-end on a synthetic monotonic clock — this makes
/// `top`/`focus`/`ignore` rebaselining work; `window`/`phase` queries
/// have no meaningful anchor for pprof data and will behave as if the
/// full profile were one contiguous span.
fn pprof_traces_to_speedscope(text: &str) -> Result<String> {
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut past_header = false;
    for line in text.lines() {
        if line.contains("-----+-") {
            if past_header && !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
            past_header = true;
            continue;
        }
        if past_header {
            cur.push(line.to_string());
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    if blocks.is_empty() {
        anyhow::bail!("no sample blocks found in pprof -traces output");
    }

    let parse_duration_ms = |tok: &str| -> Option<f64> {
        // pprof duration tokens: 10ms, 500us, 2s, 1.2ms, 3ns.
        let tok = tok.trim();
        let (num, unit) = if let Some(s) = tok.strip_suffix("ms") {
            (s, 1.0)
        } else if let Some(s) = tok.strip_suffix("us") {
            (s, 1.0 / 1000.0)
        } else if let Some(s) = tok.strip_suffix("ns") {
            (s, 1.0 / 1_000_000.0)
        } else if let Some(s) = tok.strip_suffix('s') {
            (s, 1000.0)
        } else {
            return None;
        };
        num.trim().parse::<f64>().ok().map(|v| v * unit)
    };

    // Emit one speedscope profile per pprof sample so that ignore/focus
    // filtering can drop individual samples whose stacks match. A single
    // combined profile would let one ignored frame drag the entire
    // synthetic thread out of the baseline, zeroing everything.
    let mut builder = SpeedscopeBuilder::new();
    let mut now = 0.0f64;

    for (sample_i, block) in blocks.into_iter().enumerate() {
        let mut iter = block.iter().filter(|l| !l.trim().is_empty());
        let Some(first) = iter.next() else { continue };
        let trimmed = first.trim_start();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let dur_tok = parts.next().unwrap_or("");
        let first_frame = parts.next().unwrap_or("").trim();
        let Some(dur_ms) = parse_duration_ms(dur_tok) else { continue };

        let mut stack_innermost_first: Vec<String> = Vec::new();
        if !first_frame.is_empty() {
            stack_innermost_first.push(first_frame.to_string());
        }
        for line in iter {
            let f = line.trim();
            if !f.is_empty() {
                stack_innermost_first.push(f.to_string());
            }
        }
        if stack_innermost_first.is_empty() {
            continue;
        }
        stack_innermost_first.reverse();
        let stack: Vec<usize> = stack_innermost_first.into_iter()
            .map(|n| builder.intern(n)).collect();

        let open_at = now;
        let close_at = now + dur_ms;
        let mut events: Vec<serde_json::Value> = Vec::new();
        for &idx in &stack {
            events.push(serde_json::json!({"type":"O","at":open_at,"frame":idx}));
        }
        for &idx in stack.iter().rev() {
            events.push(serde_json::json!({"type":"C","at":close_at,"frame":idx}));
        }
        builder.add_profile(format!("sample-{sample_i}"), events);
        now = close_at;
    }

    if builder.profiles.is_empty() {
        anyhow::bail!("no sample blocks found in pprof -traces output");
    }

    Ok(builder.build())
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

    /// Two-thread fixture: thread_0 runs `work` for 10ms, thread_1 parks in
    /// LowLevelLifoSemaphore.WaitForSignal for 100ms. Total = 110ms; only
    /// 10ms is real work, which is exactly the shape of the user's bench.
    fn sample_speedscope_with_idle() -> String {
        r#"{
            "shared": {
                "frames": [
                    {"name": "work"},
                    {"name": "hot_inner"},
                    {"name": "ThreadPoolWorker"},
                    {"name": "LowLevelLifoSemaphore.WaitForSignal"}
                ]
            },
            "profiles": [
                {
                    "name": "worker_hot",
                    "events": [
                        {"type": "O", "at": 0.0, "frame": 0},
                        {"type": "O", "at": 0.0, "frame": 1},
                        {"type": "C", "at": 10.0, "frame": 1},
                        {"type": "C", "at": 10.0, "frame": 0}
                    ]
                },
                {
                    "name": "worker_idle",
                    "events": [
                        {"type": "O", "at": 0.0, "frame": 2},
                        {"type": "O", "at": 0.0, "frame": 3},
                        {"type": "C", "at": 100.0, "frame": 3},
                        {"type": "C", "at": 100.0, "frame": 2}
                    ]
                }
            ]
        }"#
        .to_string()
    }

    fn load_idle_sample() -> ProfileData {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dbg-test-profile-idle-{}", id));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.speedscope.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(sample_speedscope_with_idle().as_bytes())
            .unwrap();
        ProfileData::load(&path).unwrap()
    }

    #[test]
    fn filtered_total_drops_idle_thread_under_ignore() {
        let mut p = load_idle_sample();
        assert!((p.total_time - 110.0).abs() < 0.01);
        assert!((p.filtered_total() - 110.0).abs() < 0.01);
        p.handle_command("ignore LowLevelLifoSemaphore");
        // Idle thread contributed 100ms; only the 10ms hot thread remains.
        assert!(
            (p.filtered_total() - 10.0).abs() < 0.01,
            "expected 10ms baseline, got {}",
            p.filtered_total()
        );
    }

    #[test]
    fn top_rebaseline_elevates_hot_frame_after_ignore() {
        let mut p = load_idle_sample();
        let before = p.handle_command("top");
        // hot_inner is 10/110 ≈ 9.1% before filtering
        assert!(before.contains("hot_inner"));
        p.handle_command("ignore LowLevelLifoSemaphore");
        let after = p.handle_command("top");
        // After filtering idle thread out, hot_inner becomes 10/10 = 100%
        assert!(
            after.contains("100.0%"),
            "expected hot_inner at 100% post-filter, got:\n{after}"
        );
    }

    #[test]
    fn top_no_idle_flag_applies_preset_without_mutating_state() {
        let mut p = load_idle_sample();
        let out = p.handle_command("top --no-idle");
        assert!(out.contains("100.0%"));
        // Ignore state should not be persisted by the one-shot flag.
        assert!(p.ignore.is_none(), "ignore should remain unset");
    }

    #[test]
    fn ignore_preset_dotnet_idle_sets_pattern() {
        let mut p = load_idle_sample();
        let out = p.handle_command("ignore preset dotnet-idle");
        assert!(out.contains("LowLevelLifoSemaphore"));
        assert_eq!(p.ignore.as_deref(), Some("LowLevelLifoSemaphore"));
    }

    #[test]
    fn ignore_preset_unknown_name_errors() {
        let mut p = load_idle_sample();
        let out = p.handle_command("ignore preset nope");
        assert!(out.contains("unknown preset"));
    }

    /// Two-phase fixture on a single thread: `work_a` runs 0–10ms,
    /// then a quiet gap until 100ms, then `work_b` runs 100–110ms.
    /// Simulates insert/query phases separated by idle.
    fn sample_speedscope_two_phases() -> String {
        r#"{
            "shared": {
                "frames": [
                    {"name": "main"},
                    {"name": "work_a"},
                    {"name": "work_b"}
                ]
            },
            "profiles": [{
                "name": "t0",
                "events": [
                    {"type": "O", "at": 0.0,   "frame": 0},
                    {"type": "O", "at": 0.0,   "frame": 1},
                    {"type": "C", "at": 10.0,  "frame": 1},
                    {"type": "O", "at": 100.0, "frame": 2},
                    {"type": "C", "at": 110.0, "frame": 2},
                    {"type": "C", "at": 110.0, "frame": 0}
                ]
            }]
        }"#
        .to_string()
    }

    fn load_two_phase_sample() -> ProfileData {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dbg-test-profile-phase-{}", id));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.speedscope.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(sample_speedscope_two_phases().as_bytes())
            .unwrap();
        ProfileData::load(&path).unwrap()
    }

    #[test]
    fn window_restricts_top_to_phase() {
        let mut p = load_two_phase_sample();
        p.handle_command("window 0 50");
        let out = p.handle_command("top");
        assert!(out.contains("work_a"), "work_a should appear in phase A: {out}");
        assert!(!out.contains("work_b"), "work_b must not appear: {out}");
    }

    #[test]
    fn window_clear_restores_full_view() {
        let mut p = load_two_phase_sample();
        p.handle_command("window 0 50");
        p.handle_command("window clear");
        let out = p.handle_command("top");
        assert!(out.contains("work_a"));
        assert!(out.contains("work_b"));
    }

    #[test]
    fn window_rejects_inverted() {
        let mut p = load_two_phase_sample();
        let out = p.handle_command("window 100 50");
        assert!(out.contains("invalid"));
        assert!(p.window.is_none());
    }

    #[test]
    fn phase_add_and_use() {
        let mut p = load_two_phase_sample();
        p.handle_command("phase add insert 0 50");
        p.handle_command("phase add query 100 200");
        let list = p.handle_command("phases");
        assert!(list.contains("insert") && list.contains("query"));
        p.handle_command("phase use query");
        let top = p.handle_command("top");
        assert!(top.contains("work_b"), "expected work_b under query: {top}");
        assert!(!top.contains("work_a"));
    }

    #[test]
    fn phase_use_unknown_errors() {
        let mut p = load_two_phase_sample();
        let out = p.handle_command("phase use nope");
        assert!(out.contains("no phase"));
    }

    #[test]
    fn marks_detects_quiet_gap() {
        let p = load_two_phase_sample();
        let out = p.cmd_marks(50.0);
        // The 90ms gap between work_a (close at 10) and work_b (open at 100)
        // is on thread 0; `main` is still open, though — so the gap is NOT
        // quiet from the root's perspective. We expect no gap here since
        // `main` covers the whole interval. Validate that reality explicitly.
        assert!(
            out.contains("no gaps"),
            "main covers the interval; expected no gaps: {out}"
        );
    }

    #[test]
    fn marks_detects_gap_when_root_closes() {
        // Distinct fixture: two separate root invocations with a gap.
        let json = r#"{
            "shared": {"frames": [{"name": "a"}, {"name": "b"}]},
            "profiles": [{
                "name": "t0",
                "events": [
                    {"type": "O", "at": 0.0, "frame": 0},
                    {"type": "C", "at": 5.0, "frame": 0},
                    {"type": "O", "at": 100.0, "frame": 1},
                    {"type": "C", "at": 105.0, "frame": 1}
                ]
            }]
        }"#;
        let dir = std::env::temp_dir().join("dbg-test-profile-marks-gap");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.speedscope.json");
        std::fs::write(&path, json).unwrap();
        let p = ProfileData::load(&path).unwrap();
        let out = p.cmd_marks(50.0);
        assert!(out.contains("95.00ms"), "expected 95ms gap: {out}");
    }

    // ---- perf script → speedscope converter ----

    #[test]
    fn perf_script_parses_two_samples_same_thread() {
        let text = "\
myapp 12345/12345 123.456: 1000 cycles:
\t0000000000401234 hot_fn+0x10 (/home/x/myapp)
\t0000000000400abc main+0x20 (/home/x/myapp)
\t000000000040100f _start+0xef (/home/x/myapp)

myapp 12345/12345 123.458: 1000 cycles:
\t0000000000401234 hot_fn+0x10 (/home/x/myapp)
\t00000000004012aa cold_fn+0x4 (/home/x/myapp)
\t0000000000400abc main+0x20 (/home/x/myapp)
\t000000000040100f _start+0xef (/home/x/myapp)
";
        let ss = perf_script_to_speedscope(text).unwrap();
        assert!(ss.contains("hot_fn"));
        assert!(ss.contains("cold_fn"));
        assert!(ss.contains("_start"));
        assert!(ss.contains("tid-12345"));
    }

    #[test]
    fn perf_script_loads_through_profile_data() {
        let text = "\
myapp 100/100 0.000: 1000 cycles:
\t1 work (/x/myapp)
\t2 main (/x/myapp)

myapp 100/100 0.010: 1000 cycles:
\t3 work (/x/myapp)
\t4 main (/x/myapp)
";
        let dir = std::env::temp_dir().join("dbg-test-perf-script");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("perf.script.txt");
        std::fs::write(&path, text).unwrap();
        let mut p = ProfileData::load(&path).unwrap();
        let out = p.handle_command("top");
        assert!(out.contains("work"));
        assert!(out.contains("main"));
    }

    #[test]
    fn perf_script_empty_input_errors() {
        let err = perf_script_to_speedscope("").unwrap_err().to_string();
        assert!(err.contains("no samples"));
    }

    #[test]
    fn perf_script_multi_thread_splits_profiles() {
        let text = "\
appA 100/100 0.000: 1 cycles:
\t1 fn_a (/x)

appB 200/200 0.001: 1 cycles:
\t2 fn_b (/x)
";
        let ss = perf_script_to_speedscope(text).unwrap();
        // Two thread profiles
        assert!(ss.contains("tid-100"));
        assert!(ss.contains("tid-200"));
    }

    // ---- pprof -traces → speedscope converter ----

    #[test]
    fn pprof_traces_parses_two_samples() {
        let text = "\
File: myapp
Type: cpu
Duration: 10s, Total samples = 30ms
-----------+-------------------------------------------------------
      10ms   main.compute
             main.main
             runtime.main
-----------+-------------------------------------------------------
      20ms   runtime.gopark
             main.worker
             runtime.goexit
";
        let ss = pprof_traces_to_speedscope(text).unwrap();
        assert!(ss.contains("main.compute"));
        assert!(ss.contains("runtime.gopark"));
        assert!(ss.contains("runtime.main"));
    }

    #[test]
    fn pprof_traces_loads_through_profile_data_and_rebaselines() {
        // Two samples: 10ms in main.work, 100ms in runtime.gopark. Without
        // rebaseline main.work is ~9%. With `ignore gopark` it's 100% of
        // the remaining baseline — proves the pprof path benefits from
        // the same filter plumbing as dotnet-trace.
        let text = "\
File: myapp
Type: cpu
Duration: 1s
-----------+-------------------------------------------------------
      10ms   main.work
             main.main
-----------+-------------------------------------------------------
     100ms   runtime.gopark
             runtime.park_m
";
        let dir = std::env::temp_dir().join("dbg-test-pprof-traces");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pprof.traces.txt");
        std::fs::write(&path, text).unwrap();
        let mut p = ProfileData::load(&path).unwrap();
        let before = p.handle_command("top");
        assert!(before.contains("main.work"));
        // Before filter: main.work ~= 10/110 ≈ 9%
        // pprof has only one profile so ignore drops the whole thing;
        // validate that ignore works at frame level instead:
        p.handle_command("ignore gopark");
        let after = p.handle_command("top");
        // The gopark sample is excluded; main.work is now the whole baseline.
        assert!(
            after.contains("100.0%"),
            "expected main.work at 100% post-filter:\n{after}"
        );
    }

    /// Regression: a frame that lived in an *ignored* sample used to
    /// still show up in `top` output with its pre-filter inclusive
    /// count. The baseline correctly dropped the sample's time, but
    /// the numerator was pulled from the raw un-filtered inclusive[]
    /// vector, so ghost frames from dropped samples kept appearing
    /// with a non-zero percentage. After fix, any filter causes `top`
    /// to recompute inclusive from events with the same drop rule.
    #[test]
    fn ignore_drops_ghost_frames_from_filtered_samples() {
        // Mirror the shape of pprof traces: sample A = real work only,
        // sample B = a timer stack where `park_m` appears together with
        // `timer_clean`. Ignoring `park_m` must make both frames in the
        // timer stack disappear — not just park_m itself.
        let text = "\
File: myapp
Type: cpu
Duration: 1s
-----------+-------------------------------------------------------
      630ms  main.realWork
             main.main
-----------+-------------------------------------------------------
      10ms   runtime.timer_clean
             runtime.park_m
             runtime.mcall
";
        let dir = std::env::temp_dir().join("dbg-test-ghost-frames");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.traces.txt");
        std::fs::write(&path, text).unwrap();
        let mut p = ProfileData::load(&path).unwrap();

        // Baseline sanity: unfiltered `top` shows the timer frame.
        let before = p.handle_command("top");
        assert!(before.contains("timer_clean"), "setup: {before}");

        // Filter out park_m. The timer_clean frame lives in the SAME
        // sample as park_m, so it must also vanish from top output.
        p.handle_command("ignore park_m");
        let after = p.handle_command("top");
        assert!(
            !after.contains("timer_clean"),
            "timer_clean survived as a ghost frame from a dropped sample:\n{after}"
        );
        assert!(
            !after.contains("park_m"),
            "park_m itself must be gone too:\n{after}"
        );
        // realWork is in the unfiltered sample and should remain at 100%
        // of the re-baselined denominator.
        assert!(
            after.contains("realWork") && after.contains("100.0%"),
            "realWork should re-baseline to 100%:\n{after}"
        );
    }

    /// Regression: when an ignore pattern matches every sample the
    /// baseline goes to zero. We must NOT divide numerator/~0 and
    /// print millions-of-percent garbage — instead surface a clear
    /// "nothing matched" message so the user knows to widen the
    /// pattern or reset.
    #[test]
    fn top_empty_baseline_reports_cleanly() {
        let text = "\
File: myapp
Type: cpu
Duration: 1s
-----------+-------------------------------------------------------
      10ms   main.hotWork
             main.main
-----------+-------------------------------------------------------
      20ms   main.hotWork
             main.other
";
        let dir = std::env::temp_dir().join("dbg-test-empty-baseline");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.traces.txt");
        std::fs::write(&path, text).unwrap();
        let mut p = ProfileData::load(&path).unwrap();

        // `hotWork` is in every sample — ignoring it drops everything.
        p.handle_command("ignore hotWork");
        let out = p.handle_command("top");
        assert!(
            out.contains("baseline is empty"),
            "expected empty-baseline hint, got:\n{out}"
        );
        // And crucially: no absurd percentages leak through.
        assert!(
            !out.contains("000000.0%") && !out.contains("0000.0%"),
            "exploded percentages leaked through:\n{out}"
        );
    }

    #[test]
    fn pprof_traces_empty_errors() {
        let err = pprof_traces_to_speedscope("File: x\nType: cpu")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sample"));
    }

    #[test]
    fn stats_surfaces_rebaselined_denominator() {
        let mut p = load_idle_sample();
        p.handle_command("ignore LowLevelLifoSemaphore");
        let out = p.handle_command("stats");
        assert!(
            out.contains("baseline:"),
            "stats should surface new baseline when filter is active: {out}"
        );
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
        assert!(out.contains("profiles: 1"));
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
    fn cmd_ignore_drops_subtree_and_rebaselines() {
        // `ignore` applies at the root-invocation granularity: if any
        // frame in a root's subtree matches, the whole invocation
        // drops (including main/compute/alloc here, even though they
        // don't match the pattern themselves). This is the right
        // semantic for real profiles — dotnet-trace idle threads and
        // pprof samples both collapse cleanly — but it means the
        // sample fixture, which has a single root invocation covering
        // every frame, ends up entirely dropped when any child is
        // ignored. Verify the user-facing signal is the empty-baseline
        // hint rather than a silently-broken percentage.
        let mut p = load_sample();
        p.handle_command("ignore sort");
        let out = p.handle_command("top");
        assert!(!out.contains("sort"));
        assert!(
            out.contains("baseline is empty"),
            "expected empty-baseline hint when ignore drops the sole root: {out}"
        );
    }

    #[test]
    fn cmd_ignore_spares_sibling_roots() {
        // Same data shape but with two separate root invocations: one
        // runs `hot`, the other `idle`. Ignoring `idle` must keep the
        // hot invocation intact at 100% of the rebaselined denominator.
        let json = r#"{
            "shared": {"frames": [{"name": "hot"}, {"name": "idle"}, {"name": "noise"}]},
            "profiles": [{
                "name": "t0",
                "events": [
                    {"type":"O","at":0.0,"frame":0},
                    {"type":"C","at":10.0,"frame":0},
                    {"type":"O","at":10.0,"frame":1},
                    {"type":"O","at":10.0,"frame":2},
                    {"type":"C","at":20.0,"frame":2},
                    {"type":"C","at":20.0,"frame":1}
                ]
            }]
        }"#;
        let dir = std::env::temp_dir().join("dbg-test-sibling-roots");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.speedscope.json");
        std::fs::write(&path, json).unwrap();
        let mut p = ProfileData::load(&path).unwrap();
        p.handle_command("ignore idle");
        let out = p.handle_command("top");
        assert!(out.contains("hot") && out.contains("100.0%"), "{out}");
        assert!(!out.contains("idle"), "{out}");
        assert!(!out.contains("noise"), "noise was under idle's subtree: {out}");
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
            root_subtrees: Vec::new(),
            events: Vec::new(),
            focus: None,
            ignore: None,
            window: None,
            phases: HashMap::new(),
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
