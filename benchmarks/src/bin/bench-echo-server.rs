//! Trivial WireBus peer for the peer-to-peer benchmark.
//!
//! Binds a Unix socket (`$BENCH_ECHO_SOCK`, default `/tmp/rev-bench-echo.sock`)
//! and echoes every WireBus frame it receives straight back. This stands in for
//! a real registered service: in production a client would `Lookup` the service
//! name on the Highway, get this socket path, and then talk to it directly. The
//! benchmark connects here directly (the one-time Lookup cost is already
//! captured by bench-wirebus-rtt), so this measures the steady-state
//! peer-to-peer round-trip with rev entirely out of the data path.
//!
//! Runs until killed. Serves one connection at a time, which is all the
//! single-threaded bench client needs.

use rev_benchmarks::wirebus::sync;
use std::os::unix::net::UnixListener;

fn echo_socket() -> String {
    std::env::var("BENCH_ECHO_SOCK").unwrap_or_else(|_| "/tmp/rev-bench-echo.sock".into())
}

fn main() -> std::io::Result<()> {
    let path = echo_socket();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!("bench-echo-server: listening on {path}");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Echo frames until the peer hangs up, then wait for the next client.
        while let Ok(msg) = sync::recv_message(&mut stream) {
            if sync::send_message(&mut stream, &msg).is_err() {
                break;
            }
        }
    }
    Ok(())
}
