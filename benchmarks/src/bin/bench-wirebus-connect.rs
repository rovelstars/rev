//! WireBus connection-setup cost: N × (connect → one call → disconnect).
//!
//! Mirrors a short-lived CLI-style client. Per-op cost here is dominated
//! by the Unix-socket `connect(2)` path (no auth handshake in rev's
//! current protocol) plus one round-trip. Subtract `bench-wirebus-rtt`'s
//! per-op time to isolate the connect cost.
//!
//! Usage: `bench-wirebus-connect [N]` where N defaults to 2_000.

use clap::Parser;
use rev_benchmarks::timing::time_iters;
use rev_benchmarks::wirebus;

#[derive(Parser)]
struct Args {
    /// Number of connect-call-disconnect cycles.
    #[arg(default_value_t = 2_000)]
    iters: u64,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();

    // Warm-up — one full cycle to page in libs and socket machinery.
    {
        let mut s = wirebus::connect()?;
        wirebus::call(&mut s, 0, wirebus::MessageBody::ListBus)?;
    }

    time_iters("wirebus-connect", args.iters, || {
        for i in 0..args.iters {
            let mut s = wirebus::connect().expect("wirebus connect failed");
            wirebus::call(&mut s, i + 1, wirebus::MessageBody::ListBus)
                .expect("wirebus ListBus on fresh conn failed");
            // `s` drops here, closing the fd — that's the disconnect.
        }
    });
    Ok(())
}
