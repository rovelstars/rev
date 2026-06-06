//! Tiny timing helper used by every bench binary.
//!
//! We lean on hyperfine for statistical framing (warmup, multiple runs,
//! mean/stddev), so each benchmark just needs to perform its N iterations
//! and print a single result line. We still print the internal elapsed
//! time + per-op latency so you can sanity-check against hyperfine's
//! wall-clock and see the bench's measurement floor.

use std::time::{Duration, Instant};

/// Wrap a closure that does `n` iterations of work, returning elapsed time
/// and printing a one-line summary. Prints to stderr so it doesn't land in
/// output redirections hyperfine uses for stdout-based assertions.
pub fn time_iters<F: FnOnce() -> ()>(name: &str, n: u64, f: F) -> Duration {
    let start = Instant::now();
    f();
    let elapsed = start.elapsed();
    eprintln!(
        "{name}: {n} iters in {:.3} ms  ({:.2} µs/op, {:.0} op/s)",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e6 / n as f64,
        n as f64 / elapsed.as_secs_f64(),
    );
    elapsed
}
