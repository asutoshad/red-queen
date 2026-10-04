//! Measures what one telemetry sample costs on this machine.
//!
//! Run with `cargo run --release -p rq-hardware --example sample_cost`.

use std::time::{Duration, Instant};

use rq_hardware::{Sampler, SystemRoot, SystemSnapshot};

fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the closing parenthesis of the command name; utime and
    // stime are the 12th and 13th of those.
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<f64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    (f.get(11).copied().unwrap_or(0.0) + f.get(12).copied().unwrap_or(0.0)) / 100.0
}

fn main() {
    let root = SystemRoot::host();
    let snap = SystemSnapshot::discover(&root);
    let n = 200u32;

    for (label, refresh) in [
        ("slow values every sample", Duration::ZERO),
        ("slow values every 5 s", Duration::from_secs(5)),
    ] {
        let mut sampler = Sampler::new(root.clone(), &snap).with_slow_refresh(refresh);
        sampler.sample(0);
        let (wall, cpu) = (Instant::now(), cpu_seconds());
        for i in 0..n {
            sampler.sample(u64::from(i));
        }
        let wall = wall.elapsed().as_secs_f64() / f64::from(n) * 1000.0;
        let cpu = (cpu_seconds() - cpu) / f64::from(n) * 1000.0;
        println!("{label:24} wall {wall:6.2} ms/sample   cpu {cpu:6.2} ms/sample");
    }

    let (wall, cpu) = (Instant::now(), cpu_seconds());
    for _ in 0..20 {
        let _ = SystemSnapshot::discover(&root);
    }
    println!(
        "{:24} wall {:6.2} ms/scan     cpu {:6.2} ms/scan",
        "full hardware discovery",
        wall.elapsed().as_secs_f64() / 20.0 * 1000.0,
        (cpu_seconds() - cpu) / 20.0 * 1000.0
    );
}
