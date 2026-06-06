//! DBus round-trip latency: N `org.freedesktop.DBus.Peer.Ping` calls on
//! ONE persistent session-bus connection. Ping is the cheapest method in
//! the standard DBus interfaces — empty args, empty reply, no policy
//! hops — so this is the closest apples-to-apples to our WireBus
//! `ListBus` bench.
//!
//! Runs against the user's session bus (`$DBUS_SESSION_BUS_ADDRESS`). The
//! Ping is served by the dbus-daemon itself, not a user service, so this
//! measures the broker's dispatch cost.
//!
//! Usage: `bench-dbus-rtt [N]` where N defaults to 10_000.

use clap::Parser;
use rev_benchmarks::timing::time_iters;
use zbus::blocking::Connection;
use zbus::names::BusName;

#[derive(Parser)]
struct Args {
    /// Number of round-trips to perform.
    #[arg(default_value_t = 10_000)]
    iters: u64,
}

fn main() -> zbus::Result<()> {
    let args = Args::parse();
    let conn = Connection::session()?;

    // Borrow the inner blocking proxy at the lowest level we can reach —
    // no generated proxy traits, just a raw method call. This keeps the
    // bench symmetric with the WireBus version (one request frame, one
    // reply frame, no client-side deserialisation beyond the envelope).
    let bus_name = BusName::try_from("org.freedesktop.DBus")
        .expect("well-known bus name parses");

    // Warm-up call — same rationale as the WireBus bench.
    conn.call_method(
        Some(&bus_name),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Peer"),
        "Ping",
        &(),
    )?;

    time_iters("dbus-rtt", args.iters, || {
        for _ in 0..args.iters {
            conn.call_method(
                Some(&bus_name),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Peer"),
                "Ping",
                &(),
            )
            .expect("dbus Ping round-trip failed");
        }
    });
    Ok(())
}
