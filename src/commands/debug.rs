//! Canonical debug command dispatcher.
//!
//! Recognizes a small, stable vocabulary (`break`, `step`, `next`,
//! `continue`, `finish`, `stack`, `frame`, `locals`, `print`, `watch`,
//! `threads`, `thread`, `list`, `run`, `breaks`, `unbreak`, `raw`,
//! `tool`) and routes each through the active backend's
//! `CanonicalOps`. Unknown input falls through so existing native-cmd
//! passthrough keeps working.

use super::Dispatched;
use crate::backend::{Backend, BreakId, BreakLoc, CanonicalOps};

/// Dispatch a user-facing canonical debug command. Unknown input
/// returns `Fallthrough` so the daemon can run the legacy passthrough
/// path.
pub fn dispatch_to(input: &str, backend: &dyn Backend) -> Dispatched {
    let input = input.trim();
    let (verb, rest) = split_verb(input);

    // `tool` is a meta-command that works even when the backend
    // doesn't expose CanonicalOps.
    if verb == "tool" {
        return Dispatched::Immediate(render_tool_info(backend));
    }

    // `raw <text>` is always available, even without CanonicalOps —
    // it's the escape hatch by design.
    if verb == "raw" {
        if rest.is_empty() {
            return Dispatched::Immediate(
                "usage: dbg raw <native-command>  (sends the rest of the line verbatim)".into(),
            );
        }
        return Dispatched::Native {
            canonical_op: "raw",
            native_cmd: rest.to_string(),
            decorate: false,
        };
    }

    let ops = match backend.canonical_ops() {
        Some(o) => o,
        None => return Dispatched::Fallthrough,
    };

    match verb {
        "break" => dispatch_break(ops, rest),
        "unbreak" => dispatch_unbreak(ops, rest),
        "breaks" => one_arg_native(ops.op_breaks(), "breaks"),
        "run" => dispatch_run(ops, rest),
        "continue" => one_arg_native(ops.op_continue(), "continue"),
        "step" => one_arg_native(ops.op_step(), "step"),
        "next" => one_arg_native(ops.op_next(), "next"),
        "finish" => one_arg_native(ops.op_finish(), "finish"),
        "pause" => one_arg_native(ops.op_pause(), "pause"),
        "restart" => one_arg_native(ops.op_restart(), "restart"),
        "stack" => dispatch_stack(ops, rest),
        "frame" => dispatch_frame(ops, rest),
        "locals" => one_arg_native(ops.op_locals(), "locals"),
        "print" => dispatch_print(ops, rest),
        "watch" => dispatch_watch(ops, rest),
        "threads" => one_arg_native(ops.op_threads(), "threads"),
        "thread" => dispatch_thread(ops, rest),
        "list" => dispatch_list(ops, rest),
        "catch" => dispatch_catch(ops, rest),
        _ => Dispatched::Fallthrough,
    }
}

fn split_verb(s: &str) -> (&str, &str) {
    match s.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// Render `dbg tool`'s output — always works, even if the backend has
/// no CanonicalOps (we fall back to the raw backend name).
fn render_tool_info(backend: &dyn Backend) -> String {
    match backend.canonical_ops() {
        Some(ops) => {
            let ver = ops
                .tool_version()
                .unwrap_or_else(|| "(version unavailable)".into());
            format!(
                "tool: {tool}\nversion: {ver}\nbackend: {name} — {desc}\nescape-hatch: `dbg raw <native-command>`",
                tool = ops.tool_name(),
                ver = ver,
                name = backend.name(),
                desc = backend.description(),
            )
        }
        None => format!(
            "tool: {name}\nversion: (not exposed via canonical ops)\nbackend: {name} — {desc}\ncanonical ops not available for this backend — all commands go through raw passthrough.",
            name = backend.name(),
            desc = backend.description(),
        ),
    }
}

fn one_arg_native(
    cmd: anyhow::Result<String>,
    canonical_op: &'static str,
) -> Dispatched {
    match cmd {
        Ok(native_cmd) => Dispatched::Native {
            canonical_op,
            native_cmd,
            decorate: true,
        },
        Err(e) => Dispatched::Immediate(format!("[error: {e}]")),
    }
}

fn dispatch_break(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    if rest.is_empty() {
        return Dispatched::Immediate(
            "usage: dbg break <file:line | symbol | module!method> [if <cond>]".into(),
        );
    }
    // Split off an optional ` if <expr>` suffix. The location is whatever
    // came before; the condition is everything after the delimiter.
    let (loc_str, cond) = match rest.find(" if ") {
        Some(i) => (&rest[..i], &rest[i + 4..]),
        None => (rest, ""),
    };
    let loc = BreakLoc::parse(loc_str.trim());
    let result = if cond.is_empty() {
        ops.op_break(&loc)
    } else {
        ops.op_break_conditional(&loc, cond.trim())
    };
    match result {
        Ok(cmd) => Dispatched::Native {
            canonical_op: "break",
            native_cmd: cmd,
            decorate: true,
        },
        Err(e) => Dispatched::Immediate(format!("[error: {e}]")),
    }
}

fn dispatch_unbreak(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    let id = match rest.parse::<u32>() {
        Ok(n) => BreakId(n),
        Err(_) => {
            return Dispatched::Immediate(
                "usage: dbg unbreak <id>  (id comes from `dbg breaks`)".into(),
            );
        }
    };
    one_arg_native(ops.op_unbreak(id), "unbreak")
}

fn dispatch_run(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    let args: Vec<String> = if rest.is_empty() {
        vec![]
    } else {
        // Naive split: canonical run rarely needs quoted args; agents
        // wanting complex runtime argv should use `dbg raw`.
        rest.split_whitespace().map(str::to_string).collect()
    };
    one_arg_native(ops.op_run(&args), "run")
}

fn dispatch_stack(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    let n = rest.parse::<u32>().ok();
    one_arg_native(ops.op_stack(n), "stack")
}

fn dispatch_frame(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    let n = match rest.parse::<u32>() {
        Ok(n) => n,
        Err(_) => {
            return Dispatched::Immediate("usage: dbg frame <index>".into());
        }
    };
    one_arg_native(ops.op_frame(n), "frame")
}

fn dispatch_print(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    if rest.is_empty() {
        return Dispatched::Immediate("usage: dbg print <expression>".into());
    }
    one_arg_native(ops.op_print(rest), "print")
}

fn dispatch_watch(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    if rest.is_empty() {
        return Dispatched::Immediate("usage: dbg watch <expression>".into());
    }
    one_arg_native(ops.op_watch(rest), "watch")
}

fn dispatch_thread(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    let n = match rest.parse::<u32>() {
        Ok(n) => n,
        Err(_) => {
            return Dispatched::Immediate("usage: dbg thread <index>".into());
        }
    };
    one_arg_native(ops.op_thread(n), "thread")
}

fn dispatch_list(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    let loc = if rest.is_empty() { None } else { Some(rest) };
    one_arg_native(ops.op_list(loc), "list")
}

fn dispatch_catch(ops: &dyn CanonicalOps, rest: &str) -> Dispatched {
    // `dbg catch off` clears; otherwise tokens become filter names.
    let filters: Vec<String> = if rest.is_empty() || rest.trim() == "off" {
        vec![]
    } else {
        rest.split(|c: char| c.is_ascii_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    one_arg_native(ops.op_catch(&filters), "catch")
}

/// Given the canonical-op name the daemon stamped on the response,
/// prepend a `[via <tool> <version>]` header to `output`.
pub fn decorate_output(backend: &dyn Backend, output: &str) -> String {
    let Some(ops) = backend.canonical_ops() else {
        return output.to_string();
    };
    let name = ops.tool_name();
    let header = match ops.tool_version() {
        Some(v) => format!("[via {name} {v}]\n"),
        None => format!("[via {name}]\n"),
    };
    format!("{header}{output}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::lldb::LldbBackend;
    use crate::backend::pdb::PdbBackend;
    use crate::backend::perf::PerfBackend;

    // ---------- canonical routing over lldb ----------

    fn lldb() -> LldbBackend { LldbBackend }

    fn native_of(d: Dispatched) -> (&'static str, String, bool) {
        match d {
            Dispatched::Native { canonical_op, native_cmd, decorate } => {
                (canonical_op, native_cmd, decorate)
            }
            other => panic!("expected Native, got {:?}", describe(&other)),
        }
    }

    fn describe(d: &Dispatched) -> String {
        match d {
            Dispatched::Native { canonical_op, native_cmd, decorate } => {
                format!("Native({canonical_op}, {native_cmd:?}, decorate={decorate})")
            }
            Dispatched::Immediate(s) => format!("Immediate({s:?})"),
            Dispatched::Query(q) => format!("Query({})", q.canonical_op()),
            Dispatched::Lifecycle(l) => format!("Lifecycle({})", l.canonical_op()),
            Dispatched::Fallthrough => "Fallthrough".into(),
        }
    }

    #[test]
    fn break_file_line_routes_to_lldb_syntax() {
        let b = lldb();
        let d = dispatch_to("break src/foo.rs:42", &b);
        let (op, cmd, dec) = native_of(d);
        assert_eq!(op, "break");
        assert_eq!(cmd, "breakpoint set --file src/foo.rs --line 42");
        assert!(dec);
    }

    #[test]
    fn break_fqn_routes_by_name() {
        let d = dispatch_to("break main", &lldb());
        let (_, cmd, _) = native_of(d);
        assert_eq!(cmd, "breakpoint set --name main");
    }

    #[test]
    fn break_module_method_routes_via_shlib() {
        let d = dispatch_to("break libfoo.so!bar", &lldb());
        let (_, cmd, _) = native_of(d);
        assert_eq!(cmd, "breakpoint set --shlib libfoo.so --name bar");
    }

    #[test]
    fn exec_verbs_translate() {
        let b = lldb();
        for (verb, expected) in [
            ("continue", "process continue"),
            ("step", "thread step-in"),
            ("next", "thread step-over"),
            ("finish", "thread step-out"),
        ] {
            let (op, cmd, _) = native_of(dispatch_to(verb, &b));
            assert_eq!(op, verb);
            assert_eq!(cmd, expected, "{verb}");
        }
    }

    #[test]
    fn stack_with_count_passes_arg() {
        let (_, cmd, _) = native_of(dispatch_to("stack 5", &lldb()));
        assert_eq!(cmd, "thread backtrace --count 5");
    }

    #[test]
    fn stack_without_count_plain() {
        let (_, cmd, _) = native_of(dispatch_to("stack", &lldb()));
        assert_eq!(cmd, "thread backtrace");
    }

    #[test]
    fn print_forwards_expression_verbatim() {
        let (_, cmd, _) = native_of(dispatch_to("print a + b * 2", &lldb()));
        assert_eq!(cmd, "expression -- a + b * 2");
    }

    #[test]
    fn unknown_verb_is_fallthrough() {
        match dispatch_to("breakpoint list", &lldb()) {
            Dispatched::Fallthrough => {}
            other => panic!("expected Fallthrough, got {}", describe(&other)),
        }
    }

    #[test]
    fn raw_passthrough_no_decoration() {
        let d = dispatch_to("raw breakpoint set --file main.c --line 1", &lldb());
        let (op, cmd, dec) = native_of(d);
        assert_eq!(op, "raw");
        assert_eq!(cmd, "breakpoint set --file main.c --line 1");
        assert!(!dec, "raw must not decorate");
    }

    #[test]
    fn raw_without_payload_is_usage_hint() {
        let d = dispatch_to("raw", &lldb());
        match d {
            Dispatched::Immediate(s) => assert!(s.contains("usage")),
            other => panic!("expected Immediate, got {}", describe(&other)),
        }
    }

    // ---------- error routing ----------

    #[test]
    fn watch_unsupported_on_pdb_is_immediate_error() {
        let d = dispatch_to("watch x", &PdbBackend);
        match d {
            Dispatched::Immediate(s) => {
                assert!(s.contains("pdb"));
                assert!(s.contains("watchpoints"));
                assert!(s.contains("dbg raw"));
            }
            other => panic!("expected Immediate error, got {}", describe(&other)),
        }
    }

    #[test]
    fn break_usage_hint_when_empty() {
        match dispatch_to("break", &lldb()) {
            Dispatched::Immediate(s) => assert!(s.contains("usage")),
            other => panic!("expected Immediate, got {}", describe(&other)),
        }
    }

    #[test]
    fn unbreak_rejects_non_numeric() {
        match dispatch_to("unbreak foo", &lldb()) {
            Dispatched::Immediate(s) => assert!(s.contains("usage")),
            other => panic!("expected Immediate, got {}", describe(&other)),
        }
    }

    #[test]
    fn frame_rejects_non_numeric() {
        match dispatch_to("frame foo", &lldb()) {
            Dispatched::Immediate(s) => assert!(s.contains("usage")),
            other => panic!("expected Immediate, got {}", describe(&other)),
        }
    }

    // ---------- meta commands ----------

    #[test]
    fn tool_works_on_backend_with_canonical_ops() {
        let d = dispatch_to("tool", &lldb());
        match d {
            Dispatched::Immediate(s) => {
                assert!(s.starts_with("tool: lldb"));
                assert!(s.contains("escape-hatch"));
                assert!(s.contains("dbg raw"));
            }
            other => panic!("expected Immediate, got {}", describe(&other)),
        }
    }

    #[test]
    fn tool_works_even_without_canonical_ops() {
        // perf is a profiler backend; it doesn't opt into CanonicalOps.
        let d = dispatch_to("tool", &PerfBackend);
        match d {
            Dispatched::Immediate(s) => {
                assert!(s.contains("perf"));
                assert!(s.contains("canonical ops not available"));
            }
            other => panic!("expected Immediate, got {}", describe(&other)),
        }
    }

    #[test]
    fn fallthrough_when_backend_has_no_canonical_ops() {
        // Any non-meta verb on perf should fall through to raw.
        match dispatch_to("continue", &PerfBackend) {
            Dispatched::Fallthrough => {}
            other => panic!("expected Fallthrough, got {}", describe(&other)),
        }
    }

    // ---------- decoration ----------

    #[test]
    fn decorate_prepends_via_header_when_ops_available() {
        let out = decorate_output(&lldb(), "hello\n");
        assert!(out.starts_with("[via lldb"));
        assert!(out.contains("\nhello\n"));
    }

    #[test]
    fn decorate_passthrough_on_backend_without_ops() {
        let out = decorate_output(&PerfBackend, "unchanged");
        assert_eq!(out, "unchanged");
    }

    // ---------- cross-backend wiring ----------

    #[test]
    fn pdb_break_routes_to_pdb_syntax() {
        let (_, cmd, _) = native_of(dispatch_to("break app.py:10", &PdbBackend));
        assert_eq!(cmd, "break app.py:10");
    }

    #[test]
    fn pdb_exec_verbs_match_pdb_vocabulary() {
        let b = PdbBackend;
        for (verb, expected) in [
            ("continue", "continue"),
            ("step", "step"),
            ("next", "next"),
            ("finish", "return"),
        ] {
            let (_, cmd, _) = native_of(dispatch_to(verb, &b));
            assert_eq!(cmd, expected);
        }
    }
}
