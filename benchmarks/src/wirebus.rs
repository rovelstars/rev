//! Minimal synchronous WireBus client for benchmarks.
//!
//! Rev's real broker (`rev/src/bus/server.rs`) is tokio-async, but async
//! semantics are irrelevant to a single-threaded call-in-a-loop benchmark
//! — a blocking `UnixStream` driving the same wire format gives us cleaner
//! per-op numbers and avoids runtime-start overhead polluting the
//! measurements.
//!
//! We redeclare only the message shapes we send/receive. The on-wire
//! format is length-prefixed MessagePack with an internally-tagged
//! `MessageBody` enum; serde tags must match `rev/src/bus/protocol.rs`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// 16 MB — matches rev's own ceiling in protocol.rs.
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Top-level envelope. Field order matches rev's `Message` struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    pub body: MessageBody,
}

/// Only the subset of variants our benchmarks exercise. Serde's internal
/// tagging means any variant we don't list is still decodable as an error
/// or ignored via `#[serde(other)]` if we add it later — for now, an
/// unexpected server reply surfaces as a deserialize failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum MessageBody {
    /// Cheapest round-trip the broker supports: enumerate registered
    /// services. Response body is `BusServiceList`. No services need to be
    /// registered beforehand — an empty list still costs one frame pair.
    ListBus,
    /// Lookup a specific service (returns `LookupResult` or `Error`).
    Lookup {
        name: String,
    },
    /// Generic success.
    Ok { message: String },
    /// Generic error.
    Error { message: String },
    /// Response to `ListBus`.
    BusServiceList { services: Vec<BusEntry> },
    /// Response to `Lookup`.
    LookupResult {
        name: String,
        socket_path: PathBuf,
        methods: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEntry {
    pub name: String,
    pub socket_path: PathBuf,
    pub methods: HashMap<String, String>,
}

/// Connect to the rev System Highway socket. Honours `$REV_SOCK`, then
/// falls back to the debug-default `./rev.sock` path rev uses in dev
/// builds — both the broker and these benchmarks are expected to run
/// from the same working directory.
pub fn connect() -> io::Result<UnixStream> {
    let path = std::env::var("REV_SOCK").unwrap_or_else(|_| "./rev.sock".into());
    UnixStream::connect(&path).map_err(|e| {
        io::Error::new(e.kind(), format!("connect to {path}: {e}"))
    })
}

/// Send one request and read exactly one response frame. One full
/// round-trip — two writes + one read at the syscall level.
pub fn call(stream: &mut UnixStream, id: u64, body: MessageBody) -> io::Result<Message> {
    let req = Message {
        id,
        sender: "rev-bench".into(),
        auth_token: None,
        body,
    };
    write_msg(stream, &req)?;
    read_msg(stream)
}

fn write_msg(stream: &mut UnixStream, msg: &Message) -> io::Result<()> {
    let bytes = rmp_serde::to_vec_named(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = (bytes.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn read_msg(stream: &mut UnixStream) -> io::Result<Message> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf)?;
    rmp_serde::from_slice(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
