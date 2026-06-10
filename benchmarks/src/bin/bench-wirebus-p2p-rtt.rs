//! WireBus peer-to-peer round-trip latency: N calls straight to a service
//! socket, with rev out of the data path. This is how WireBus actually carries
//! method calls once a client has looked the service up; the matching DBus
//! number is bench-dbus-rtt, where every call is relayed by the dbus-daemon.
//!
//! Talks to bench-echo-server over `$BENCH_ECHO_SOCK`
//! (default `/tmp/rev-bench-echo.sock`); start that first.
//!
//! Usage: `bench-wirebus-p2p-rtt [N]` where N defaults to 10_000.

use clap::Parser;
use rev_benchmarks::timing::time_iters;
use rev_benchmarks::wirebus::{self, MessageBody};
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Number of round-trips to perform.
    #[arg(default_value_t = 10_000)]
    iters: u64,
}

fn echo_socket() -> PathBuf {
    PathBuf::from(std::env::var("BENCH_ECHO_SOCK").unwrap_or_else(|_| "/tmp/rev-bench-echo.sock".into()))
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let mut stream = wirebus::connect_to(&echo_socket())?;

    // A small, fixed request body, echoed back by the peer. Mirrors DBus's
    // Peer.Ping: tiny payload out, same payload in.
    let body = || MessageBody::Ok { message: "ping".into() };

    // Warm the socket + allocator path before the measured loop.
    wirebus::call(&mut stream, 0, body())?;

    time_iters("wirebus-p2p-rtt", args.iters, || {
        for i in 0..args.iters {
            wirebus::call(&mut stream, i + 1, body())
                .expect("wirebus p2p round-trip failed");
        }
    });
    Ok(())
}
