use super::{GpuDb, fmt_us};

// ---------------------------------------------------------------------------
// focus / ignore / region / reset
// ---------------------------------------------------------------------------

pub fn cmd_focus(db: &mut GpuDb, args: &[&str]) {
    match args.first() {
        Some(p) => { db.focus = Some(p.to_string()); println!("focus set to '{p}'"); }
        None => println!("usage: focus <pattern>"),
    }
}

pub fn cmd_ignore(db: &mut GpuDb, args: &[&str]) {
    match args.first() {
        Some(p) => { db.ignore = Some(p.to_string()); println!("ignoring '{p}'"); }
        None => println!("usage: ignore <pattern>"),
    }
}

pub fn cmd_region(db: &mut GpuDb, args: &[&str]) {
    match args.first() {
        Some(p) => { db.region_filter = Some(p.to_string()); println!("region filter set to '{p}'"); }
        None => {
            let rows: Vec<(String, f64)> = db.query_vec(
                "SELECT name, duration_us FROM regions ORDER BY start_us", [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            );
            if rows.is_empty() { println!("no NVTX regions"); }
            else { for (n, d) in &rows { println!("  {} ({})", n, fmt_us(*d)); } }
        }
    }
}

pub fn cmd_reset(db: &mut GpuDb) {
    db.focus = None;
    db.ignore = None;
    db.region_filter = None;
    println!("all filters cleared");
}
