//! Thin benchmark client over the real `wirebus-proto` crate.
//!
//! The benchmarks use rev's actual wire types and codec rather than a
//! hand-copied subset, so the numbers reflect the code that ships. Rev's server
//! is tokio-async, but a blocking `UnixStream` driving the same frames gives
//! cleaner per-op latency without runtime-start overhead, and the
//! `wirebus_proto::sync` codec is exactly what the blocking peers (RookGuard)
//! use in production.

pub use wirebus_proto::{sync, BusEntry, Message, MessageBody};

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Connect to the rev System Highway. Uses `wirebus_proto::highway_socket()`,
/// the same resolver (and `REV_BUS_SOCK` override) rev itself uses, so the
/// benchmark and the server always agree on the path.
pub fn connect() -> io::Result<UnixStream> {
    let path = wirebus_proto::highway_socket();
    UnixStream::connect(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("connect to {}: {e}", path.display())))
}

/// Connect directly to a peer service's own socket (the post-Lookup path).
pub fn connect_to(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
        .map_err(|e| io::Error::new(e.kind(), format!("connect to {}: {e}", path.display())))
}

/// Send one request and read exactly one response frame: one full round-trip.
pub fn call(stream: &mut UnixStream, id: u64, body: MessageBody) -> io::Result<Message> {
    let req = Message {
        id,
        sender: "rev-bench".into(),
        auth_token: None,
        body,
    };
    sync::send_message(stream, &req)?;
    sync::recv_message(stream)
}
