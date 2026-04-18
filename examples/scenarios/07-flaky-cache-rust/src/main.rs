// A small TTL cache. We insert with a TTL of 100ms, sleep 50ms,
// and expect to read the value back. Sometimes we get None.
//
// Repro: cargo run --bin broken
//
// The flake rate depends on platform clock resolution. On most
// systems you'll see a few misses per 100 lookups; on some you'll
// see 100/100. Either way, *one* miss is a bug.

use std::collections::HashMap;
use std::time::{Duration, Instant};

struct TtlCache {
    map: HashMap<String, (Instant, String)>,
}

impl TtlCache {
    fn new() -> Self {
        Self { map: HashMap::new() }
    }

    fn insert(&mut self, key: String, val: String, ttl: Duration) {
        let expires_at = Instant::now() + ttl;
        self.map.insert(key, (expires_at, val));
    }

    fn get(&self, key: &str) -> Option<&str> {
        let (expires_at, val) = self.map.get(key)?;
        let now = Instant::now();
        // expired?
        if now >= *expires_at {
            return None;
        }
        // Bug: subtle. The intent is "if there are at least 10ms
        // of TTL remaining, return it; otherwise treat as expired
        // to avoid races." See if you spot it.
        let remaining = expires_at.duration_since(now);
        if remaining < Duration::from_millis(10) {
            return None;
        }
        Some(val.as_str())
    }
}

fn main() {
    let mut cache = TtlCache::new();
    let mut misses = 0;
    for i in 0..100 {
        let k = format!("k{i}");
        cache.insert(k.clone(), format!("v{i}"), Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(50));
        if cache.get(&k).is_none() {
            misses += 1;
        }
    }
    println!("misses: {misses}/100");
    if misses > 0 {
        eprintln!("BUG: cache returned None for entries that should be live");
        std::process::exit(1);
    }
}
