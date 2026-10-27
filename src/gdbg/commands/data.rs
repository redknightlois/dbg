use std::path::Path;

use super::{GpuDb, escape_regex, fmt_us, ncu_instance_count_for_names, ncu_instance_count_label};

// ---------------------------------------------------------------------------
// layers
// ---------------------------------------------------------------------------

pub fn cmd_layers(db: &GpuDb) {
    let sql = "SELECT id, source, file, collected_at, collection_secs FROM layers ORDER BY id";
    let rows: Vec<(i64, String, String, String, Option<f64>)> = db.query_vec(sql, [], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    });

    if rows.is_empty() {
        println!("no layers loaded");
        return;
    }
    println!("# | Source | File | Collected | Secs");
    for (id, source, file, at, secs) in &rows {
        println!(
            "{} | {} | {} | {} | {}",
            id,
            source,
            file,
            at,
            secs.map(|s| format!("{s:.1}"))
                .unwrap_or_else(|| "?".into())
        );
    }

    let uk = db.unique_kernel_count();
    let wm = db.kernels_with_metrics();
    let wo = db.kernels_with_ops();
    if db.timeline_layer_id().is_none() && wm > 0 {
        let names: Vec<String> = db.query_vec(
            &format!(
                "SELECT kernel_name FROM metrics m WHERE {}",
                db.metric_filter_for("m")
            ),
            [],
            |row| row.get(0),
        );
        let count = ncu_instance_count_for_names(db, &names);
        println!("\n  NCU counter-only kernel names: {wm}");
        println!("  NCU instances: {}", ncu_instance_count_label(db, count));
        println!("  Timeline correlation: N/A (timeline unavailable)");
        if wo > 0 {
            println!("  Kernels with op mapping: {wo}");
        }
        return;
    }
    println!("\n  Correlation: {uk} unique kernels");
    println!("    With metrics:  {wm}/{uk}");
    println!("    With op map:   {wo}/{uk}");
}

// ---------------------------------------------------------------------------
// suggest
// ---------------------------------------------------------------------------

pub fn cmd_suggest(db: &GpuDb) {
    let uk = db.unique_kernel_count();
    let wm = db.kernels_with_metrics();
    let failures = db.failures();
    let has_nsys = db.has_layer("nsys");
    let has_ncu = db.has_layer("ncu");
    let has_torch = db.has_layer("torch");
    let target = db.meta("target");

    if uk == 0 && wm == 0 && failures.is_empty() {
        println!("no profile data");
        return;
    }

    let mut n = 1;

    if !failures.is_empty() {
        println!("  Collection failures:\n");
        for (phase, error) in &failures {
            println!("  {n}. {phase} failed: {error}");
            n += 1;
        }
        println!();
    }

    if db.timeline_layer_id().is_none() && wm > 0 {
        let counts = db.ncu_instance_counts();
        let count = counts.as_ref().map(|counts| counts.values().sum());
        println!("  Counter-only profile: {wm} NCU kernel names.");
        println!(
            "  Selected NCU instances: {}.",
            ncu_instance_count_label(db, count)
        );
        println!(
            "  Timeline unavailable; wall time, launch gaps and timeline correlation are N/A."
        );
        println!(
            "  Use kernels, inspect <kernel> and bound <kernel> to inspect retained counters."
        );
        return;
    }
    if uk == 0 {
        println!("  No kernel data collected.");
        return;
    }
    println!("  Suggestions:\n");

    if !has_nsys {
        println!("  {n}. No timeline data. Run gdbg with your target.");
        println!("     This gives: kernel timeline, memory transfers, GPU idle gaps\n");
        n += 1;
    }

    if !has_ncu {
        let tl = db.timeline_filter();
        let top_sql = format!(
            "SELECT kernel_name, SUM(duration_us) as total
                       FROM launches WHERE {tl} GROUP BY kernel_name ORDER BY total DESC LIMIT 5"
        );
        let top: Vec<(String, f64)> =
            db.query_vec(&top_sql, [], |row| Ok((row.get(0)?, row.get(1)?)));

        if !top.is_empty() {
            let gpu_total = db.total_gpu_time_us();
            let pct: f64 = top
                .iter()
                .map(|t| {
                    if gpu_total > 0.0 {
                        t.1 / gpu_total * 100.0
                    } else {
                        0.0
                    }
                })
                .sum();
            let regex = top
                .iter()
                .map(|t| escape_regex(&db.native_kernel_name(&t.0)))
                .collect::<Vec<_>>()
                .join("|");
            println!(
                "  {n}. Top {} kernels ({pct:.0}% of GPU) lack hardware metrics.",
                top.len()
            );
            if target.ends_with(".cu") {
                println!(
                    "     Re-profile the CUDA source with gdbg {target} to compile it before collection.\n"
                );
            } else {
                let interpreter = if target.ends_with(".py") {
                    "python3 "
                } else {
                    ""
                };
                println!(
                    "     Collect: ncu --set full --kernel-name-base demangled --launch-count 1 --kernel-name \"regex:{regex}\" {interpreter}{target}\n"
                );
            }
            n += 1;
        }
    }

    if !has_torch && target.ends_with(".py") {
        println!("  {n}. No op->kernel mapping. Can't trace kernels back to Python.");
        println!("     Needed for: ops, callers, trace commands\n");
        n += 1;
    }

    let tl2 = db.timeline_filter();
    let var_sql = format!(
        "SELECT kernel_name, COUNT(*) as cnt, AVG(duration_us) as avg,
                   AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us) as var
                   FROM launches WHERE {tl2} GROUP BY kernel_name
                   HAVING cnt > 5 AND var > 0
                   ORDER BY SUM(duration_us) DESC LIMIT 5"
    );
    let vars: Vec<(String, f64, f64)> = db.query_vec(&var_sql, [], |row| {
        Ok((row.get(0)?, row.get(2)?, row.get(3)?))
    });

    for (name, avg, var) in &vars {
        let stddev = var.max(0.0).sqrt();
        let cv = if *avg > 0.0 { stddev / avg } else { 0.0 };
        if cv > 0.3 {
            println!("  {n}. '{}' has high variance (CV={cv:.2}).", name);
            println!(
                "     May indicate: data-dependent paths, cache effects, or varying input sizes.\n"
            );
            n += 1;
        }
    }

    if has_nsys {
        let gpu_us = db.total_gpu_time_us();
        let transfer_filter = db.timeline_filter_for("transfers");
        let xfer_us: f64 = db.scalar_f64(&format!(
            "SELECT COALESCE(SUM(duration_us),0) FROM transfers WHERE {transfer_filter}"
        ));

        if gpu_us > 0.0 && xfer_us > 0.0 {
            let ratio = xfer_us / gpu_us;
            if ratio > 5.0 {
                println!("  {n}. Transfer:compute ratio is {ratio:.1}:1 — PCIe dominates.");
                println!(
                    "     Try: cudaMallocHost (pinned memory), overlap via CUDA streams, or increase batch size.\n"
                );
                n += 1;
            }
        }

        let tl = db.timeline_filter();
        let tiny_sql = format!(
            "SELECT COUNT(*) FROM (
                SELECT kernel_name FROM launches WHERE {tl}
                GROUP BY kernel_name HAVING AVG(duration_us) < 10.0
             )"
        );
        let tiny_count: i64 = db.scalar_f64(&tiny_sql) as i64;
        if tiny_count > 10 {
            println!(
                "  {n}. {tiny_count} distinct kernels average under 10us; duration alone does not establish launch overhead."
            );
            println!(
                "     Inspect measured gaps, launch provenance, and graph replay boundaries with 'small' and 'fuse'.\n"
            );
            n += 1;
        }

        let dom_sql = format!(
            "SELECT kernel_name, SUM(duration_us) as t FROM launches WHERE {tl}
             GROUP BY kernel_name ORDER BY t DESC LIMIT 1"
        );
        if let Ok((dom_name, dom_time)) = db.conn.query_row(&dom_sql, [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        }) {
            if gpu_us > 0.0 && dom_time / gpu_us > 0.5 {
                let pct = dom_time / gpu_us * 100.0;
                println!(
                    "  {n}. One kernel accounts for {pct:.0}% of GPU time: {}",
                    &dom_name
                );
                println!(
                    "     Try: bound '{}' — optimize the hotspot directly.\n",
                    &dom_name
                );
                n += 1;
            }
        }
    }

    if has_nsys && has_ncu && (has_torch || !target.ends_with(".py")) {
        println!("  All layers loaded — full analysis available.");
    }

    let _ = n;
}

// ---------------------------------------------------------------------------
// save / list / diff
// ---------------------------------------------------------------------------

pub fn cmd_save(db: &GpuDb, args: &[&str]) {
    let name = match args.first() {
        Some(n) => *n,
        None => {
            println!("usage: save <name>");
            return;
        }
    };
    match db.save(name) {
        Ok(path) => println!("saved to {}", path.display()),
        Err(e) => println!("save failed: {e}"),
    }
}

pub fn cmd_list() {
    match GpuDb::list_saved() {
        Ok(sessions) => {
            if sessions.is_empty() {
                println!("no saved sessions");
                return;
            }
            println!("Name | Device | Kernels | Layers | Created");
            for s in &sessions {
                let dev = if s.device.is_empty() { "?" } else { &s.device };
                println!(
                    "{} | {} | {} | {} | {}",
                    &s.name,
                    dev,
                    s.kernel_count,
                    s.layers.join("+"),
                    s.created
                );
            }
        }
        Err(e) => println!("list failed: {e}"),
    }
}

pub fn cmd_diff(db: &GpuDb, args: &[&str]) {
    let name = match args.first() {
        Some(n) => *n,
        None => {
            println!("usage: diff <saved_session>");
            return;
        }
    };

    let other = if name.ends_with(".gpu.db") || name.contains('/') {
        match GpuDb::open(Path::new(name)) {
            Ok(saved) => saved,
            Err(error) => {
                println!("cannot load '{name}': {error}");
                return;
            }
        }
    } else {
        match GpuDb::load(name) {
            Ok(saved) => saved,
            Err(error) => {
                println!("cannot load '{name}': {error}");
                return;
            }
        }
    };

    if let Err(e) = db.attach_db(&other, "other") {
        println!("cannot load '{name}': {e}");
        return;
    }

    let current_tl = db.timeline_filter();
    let other_tl = "other.launches.layer_id = COALESCE(
        (SELECT id FROM other.layers WHERE source = 'nsys' ORDER BY id LIMIT 1),
        (SELECT id FROM other.layers WHERE source = 'torch' ORDER BY id LIMIT 1),
        (SELECT id FROM other.layers WHERE source = 'proton' ORDER BY id LIMIT 1), -1)";
    let sql = format!("SELECT
        COALESCE(c.kernel_name, o.kernel_name) as name,
        COALESCE(o.total, 0) as before,
        COALESCE(c.total, 0) as after
       FROM
        (SELECT kernel_name, SUM(duration_us) as total FROM launches WHERE {current_tl} GROUP BY kernel_name) c
       FULL OUTER JOIN
        (SELECT kernel_name, SUM(duration_us) as total FROM other.launches WHERE {other_tl} GROUP BY kernel_name) o
       ON c.kernel_name = o.kernel_name
       ORDER BY ABS(COALESCE(c.total,0) - COALESCE(o.total,0)) DESC
       LIMIT 15");

    let rows: Vec<(String, f64, f64)> =
        match db.query_vec_result(&sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))) {
            Ok(rows) => rows,
            Err(error) => {
                println!("diff query failed for '{name}': {error}");
                let _ = db.detach("other");
                return;
            }
        };

    println!("  Diff: current vs {name}\n");
    println!("Kernel | Before | After | Delta");
    for (kname, before, after) in &rows {
        let delta = if *before > 0.0 {
            let pct = (after - before) / before * 100.0;
            format!("{}{pct:.1}%", if pct >= 0.0 { "+" } else { "" })
        } else {
            "new".into()
        };
        println!(
            "{} | {} | {} | {}",
            kname,
            fmt_us(*before),
            fmt_us(*after),
            delta
        );
    }

    let _ = db.detach("other");
}
