// Real-world shape: we're deduplicating an event stream before
// writing it to disk. The "dedup" check scans the whole accumulator
// on every insert — fine for 100 events, quadratic for 100k. Profile
// says main thread spends 95 %+ in `dedup_push`.

fn dedup_push(buf: &mut Vec<u64>, x: u64) {
    // Linear scan — O(n) per insert, O(n²) across the batch.
    for &e in buf.iter() {
        if e == x {
            return;
        }
    }
    buf.push(x);
}

fn process(events: &[u64]) -> Vec<u64> {
    let mut out = Vec::new();
    for &e in events {
        dedup_push(&mut out, e);
    }
    out
}

fn main() {
    // Synthetic workload — 80k events, ~50 % duplicates.
    let events: Vec<u64> = (0..80_000).map(|i| i % 40_000).collect();
    let out = process(&events);
    println!("kept {} unique events", out.len());
}
