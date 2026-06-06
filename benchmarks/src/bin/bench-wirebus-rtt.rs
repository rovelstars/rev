//! WireBus round-trip latency: N `ListBus` calls on ONE persistent
//! connection to the rev System Highway.
//!
//! `ListBus` is the cheapest broker-handled request — no services need to
//! exist, the response is a tiny (possibly empty) list. Measures
//! pure wire + broker dispatch cost, excluding connection setup.
//!
//! Usage: `bench-wirebus-rtt [N]` where N defaults to 10_000.

use clap::Parser;
use rev_benchmarks::timing::time_iters;
use rev_benchmarks::wirebus;

#[derive(Parser)]
struct Args {
    /// Number of round-trips to perform.
    #[arg(default_value_t = 10_000)]
    iters: u64,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let mut stream = wirebus::connect()?;

    // Warm the socket with one call so allocator / kernel path caches
    // are hot before the measured loop.
    wirebus::call(&mut stream, 0, wirebus::MessageBody::ListBus)?;

    time_iters("wirebus-rtt", args.iters, || {
        for i in 0..args.iters {
            wirebus::call(&mut stream, i + 1, wirebus::MessageBody::ListBus)
                .expect("wirebus ListBus round-trip failed");
        }
    });
    Ok(())
}
