//! DBus connection-setup cost: N × (connect → one Peer.Ping → disconnect).
//!
//! Matches `bench-wirebus-connect` for fair per-connection comparison.
//! DBus pays noticeably more than WireBus here: the SASL handshake alone
//! is several round-trips (AUTH EXTERNAL → OK → BEGIN) on top of the
//! Hello negotiation before the first method call can land. This is
//! exactly the overhead `zbus::blocking::Connection::session()` hides.
//!
//! Usage: `bench-dbus-connect [N]` where N defaults to 2_000.

use clap::Parser;
use rev_benchmarks::timing::time_iters;
use zbus::blocking::Connection;
use zbus::names::BusName;

#[derive(Parser)]
struct Args {
    /// Number of connect-call-disconnect cycles.
    #[arg(default_value_t = 2_000)]
    iters: u64,
}

fn main() -> zbus::Result<()> {
    let args = Args::parse();
    let bus_name = BusName::try_from("org.freedesktop.DBus")
        .expect("well-known bus name parses");

    // Warm-up.
    {
        let c = Connection::session()?;
        c.call_method(
            Some(&bus_name),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Peer"),
            "Ping",
            &(),
        )?;
    }

    time_iters("dbus-connect", args.iters, || {
        for _ in 0..args.iters {
            let c = Connection::session().expect("dbus connect failed");
            c.call_method(
                Some(&bus_name),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Peer"),
                "Ping",
                &(),
            )
            .expect("dbus Ping on fresh conn failed");
            // `c` drops here.
        }
    });
    Ok(())
}
