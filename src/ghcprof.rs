//! GHC .prof to callgrind format converter.
//!
//! Parses the standard GHC profiling report (text format from `+RTS -p`)
//! and writes callgrind-compatible output that the profile REPL can read.

use std::io::Write;

/// A parsed cost centre from GHC's .prof output.
struct CostCentre {
    name: String,
    module: String,
    src: String,
    entries: u64,
    individual_time: f64,
    individual_alloc: f64,
    inherited_time: f64,
    inherited_alloc: f64,
    /// Indentation level (number of leading spaces) to reconstruct the tree.
    depth: usize,
}

/// Convert a GHC .prof file to callgrind format, writing to the given path.
pub fn convert(prof_path: &str, out_path: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(prof_path)?;

    // Parse the total time/alloc from the summary section
    let mut total_ticks: u64 = 0;
    let mut total_alloc: u64 = 0;

    for line in text.lines() {
        if line.trim_start().starts_with("total time") {
            // "	total time  =        0.21 secs   (214 ticks @ 1000 us, 1 processor)"
            if let Some(paren) = line.find('(') {
                let after = &line[paren + 1..];
                if let Some(ticks_str) = after.split_whitespace().next() {
                    total_ticks = ticks_str.replace(',', "").parse().unwrap_or(0);
                }
            }
        }
        if line.trim_start().starts_with("total alloc") {
            // "	total alloc = 328,241,824 bytes  (excludes profiling overheads)"
            if let Some(eq) = line.find('=') {
                let after = &line[eq + 1..];
                let num_str: String = after
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == ',')
                    .collect();
                total_alloc = num_str.replace(',', "").parse().unwrap_or(0);
            }
        }
    }

    // Parse the detailed cost centre tree.
    // The tree section starts after the header line containing "individual" and "inherited".
    let ccs = parse_tree(&text);
    if ccs.is_empty() {
        anyhow::bail!("no cost centres found in {prof_path}");
    }

    // Build the call tree structure: for each CC, find its children by depth.
    // CCs are listed in pre-order (DFS), so children of node at depth D are
    // subsequent nodes at depth D+1 before the next node at depth <= D.
    let mut out = std::fs::File::create(out_path)?;

    // Callgrind header
    writeln!(out, "version: 1")?;
    writeln!(out, "creator: dbg ghcprof-convert")?;
    writeln!(out, "cmd: ghc-profile")?;
    writeln!(out, "positions: line")?;
    writeln!(out, "events: Ticks Bytes")?;
    writeln!(out, "summary: {} {}", total_ticks, total_alloc)?;
    writeln!(out)?;

    // Assign file IDs
    let mut file_ids: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut next_fid: u32 = 1;
    for cc in &ccs {
        let key = format!("{} ({})", cc.src, cc.module);
        if !file_ids.contains_key(&key) {
            file_ids.insert(key.clone(), next_fid);
            next_fid += 1;
        }
    }

    // Write each cost centre as a callgrind function block
    for (i, cc) in ccs.iter().enumerate() {
        let fn_name = format!("{}.{}", cc.module, cc.name);
        let file_key = format!("{} ({})", cc.src, cc.module);
        let fid = file_ids[&file_key];

        writeln!(out, "fl=({}) {}", fid, file_key)?;
        writeln!(out, "fn={}", fn_name)?;

        // Self cost: individual_time as fraction of total_ticks, individual_alloc as fraction of total_alloc
        let self_ticks =
            ((cc.individual_time / 100.0) * total_ticks as f64).round() as u64;
        let self_bytes =
            ((cc.individual_alloc / 100.0) * total_alloc as f64).round() as u64;

        writeln!(out, "1 {} {}", self_ticks, self_bytes)?;

        // Find children (next entries at depth+1 before depth <= current)
        let child_depth = cc.depth + 1;
        let mut j = i + 1;
        while j < ccs.len() && ccs[j].depth > cc.depth {
            if ccs[j].depth == child_depth {
                let child = &ccs[j];
                let child_fn = format!("{}.{}", child.module, child.name);
                let child_file_key = format!("{} ({})", child.src, child.module);
                let child_fid = file_ids[&child_file_key];

                let child_incl_ticks =
                    ((child.inherited_time / 100.0) * total_ticks as f64).round() as u64;
                let child_incl_bytes =
                    ((child.inherited_alloc / 100.0) * total_alloc as f64).round() as u64;

                writeln!(out, "cfl=({}) {}", child_fid, child_file_key)?;
                writeln!(out, "cfn={}", child_fn)?;
                writeln!(out, "calls={} 1", child.entries.max(1))?;
                writeln!(out, "1 {} {}", child_incl_ticks, child_incl_bytes)?;
            }
            j += 1;
        }

        writeln!(out)?;
    }

    Ok(())
}

fn parse_tree(text: &str) -> Vec<CostCentre> {
    let mut ccs = Vec::new();
    let mut in_tree = false;
    let mut header_seen = 0;

    for line in text.lines() {
        // Detect the tree section: two header lines with "individual" then "inherited"
        if line.contains("individual") && line.contains("inherited") {
            header_seen += 1;
            continue;
        }

        // Skip the COST CENTRE / MODULE header line
        if header_seen >= 1 && !in_tree {
            if line.trim_start().starts_with("COST CENTRE") {
                continue;
            }
            // Skip blank or separator lines
            if line.trim().is_empty() {
                if header_seen >= 1 {
                    in_tree = true;
                }
                continue;
            }
            in_tree = true;
        }

        if !in_tree {
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse: "  COST_CENTRE  MODULE  SRC  no.  entries  %time  %alloc  %time  %alloc"
        // The leading whitespace indicates depth.
        let depth = line.len() - line.trim_start().len();

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 8 {
            continue;
        }

        // Find the source location — it contains a colon or is "<built-in>" / "<entire-module>"
        // Format: NAME MODULE SRC NO ENTRIES %TIME %ALLOC %TIME %ALLOC
        // But NAME and SRC can contain spaces... The reliable approach:
        // Work backwards from the end (numbers) to find the boundary.

        // Last 6 fields are always: no. entries %time %alloc %time %alloc
        let n = parts.len();
        if n < 8 {
            continue;
        }

        let inherited_alloc: f64 = match parts[n - 1].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let inherited_time: f64 = match parts[n - 2].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let individual_alloc: f64 = match parts[n - 3].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let individual_time: f64 = match parts[n - 4].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let entries: u64 = parts[n - 5].parse().unwrap_or(0);
        let _no: u64 = match parts[n - 6].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Everything before the last 6 numeric fields is: NAME MODULE SRC
        // SRC is the field just before no., MODULE before SRC, NAME before MODULE
        let text_parts = &parts[..n - 6];
        if text_parts.len() < 3 {
            continue;
        }

        // SRC is the last text part, MODULE is second-to-last, NAME is everything before
        let src = text_parts[text_parts.len() - 1].to_string();
        let module = text_parts[text_parts.len() - 2].to_string();
        let name = text_parts[..text_parts.len() - 2].join(" ");

        ccs.push(CostCentre {
            name,
            module,
            src,
            entries,
            individual_time,
            individual_alloc,
            inherited_time,
            inherited_alloc,
            depth,
        });
    }

    ccs
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PROF: &str = r#"
	Sun Apr 12 02:10 2026 Time and Allocation Profiling Report  (Final)

	   test_prof +RTS -p -RTS

	total time  =        0.21 secs   (214 ticks @ 1000 us, 1 processor)
	total alloc = 328,241,824 bytes  (excludes profiling overheads)

COST CENTRE MODULE    SRC                      %time %alloc

fib         Main      /tmp/test_prof.hs:8:1-3  100.0  100.0


                                                                               individual      inherited
COST CENTRE  MODULE                SRC                      no.     entries  %time %alloc   %time %alloc

MAIN         MAIN                  <built-in>               136           0    0.0    0.0   100.0  100.0
 CAF         Main                  <entire-module>          143           0    0.0    0.0   100.0  100.0
  main       Main                  /tmp/test_prof.hs:13:1-4 274           1    0.0    0.0   100.0  100.0
   fib       Main                  /tmp/test_prof.hs:8:1-3  277     2692537  100.0  100.0   100.0  100.0
   factorial Main                  /tmp/test_prof.hs:4:1-9  276          21    0.0    0.0     0.0    0.0
"#;

    #[test]
    fn parse_tree_extracts_cost_centres() {
        let ccs = parse_tree(SAMPLE_PROF);
        assert!(!ccs.is_empty());
        let names: Vec<&str> = ccs.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"MAIN"));
        assert!(names.contains(&"main"));
        assert!(names.contains(&"fib"));
        assert!(names.contains(&"factorial"));
    }

    #[test]
    fn parse_tree_preserves_depth() {
        let ccs = parse_tree(SAMPLE_PROF);
        let main_cc = ccs.iter().find(|c| c.name == "MAIN").unwrap();
        let caf_cc = ccs.iter().find(|c| c.name == "CAF").unwrap();
        assert!(caf_cc.depth > main_cc.depth);
    }

    #[test]
    fn parse_tree_numeric_fields() {
        let ccs = parse_tree(SAMPLE_PROF);
        let fib = ccs.iter().find(|c| c.name == "fib").unwrap();
        assert_eq!(fib.entries, 2692537);
        assert!((fib.individual_time - 100.0).abs() < 0.01);
        assert_eq!(fib.module, "Main");
        assert_eq!(fib.src, "/tmp/test_prof.hs:8:1-3");
    }

    #[test]
    fn convert_roundtrip() {
        let prof_path = "/tmp/dbg_test_ghcprof.prof";
        let cg_path = "/tmp/dbg_test_ghcprof.callgrind";
        std::fs::write(prof_path, SAMPLE_PROF).unwrap();
        convert(prof_path, cg_path).unwrap();
        let output = std::fs::read_to_string(cg_path).unwrap();
        assert!(output.contains("version: 1"));
        assert!(output.contains("fn=Main.fib"));
        assert!(output.contains("fn=Main.main"));
        assert!(output.contains("fn=Main.factorial"));
        // Cleanup
        let _ = std::fs::remove_file(prof_path);
        let _ = std::fs::remove_file(cg_path);
    }
}
