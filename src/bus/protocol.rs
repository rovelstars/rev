//! WireBus wire protocol.
//!
//! All communication over WireBus uses length-prefixed MessagePack frames:
//!
//!   [4 bytes BE length] [MessagePack payload]
//!
//! The payload is always a `Message`, which covers requests, responses,
//! service management, bus registry operations, and inter-service signals.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// Frame I/O — language-agnostic length-prefixed MessagePack
// ---------------------------------------------------------------------------

/// Maximum frame size (16 MB).
const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Read a single length-prefixed MessagePack frame from an async reader.
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {} bytes (max {})", len, MAX_FRAME_SIZE),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write a single length-prefixed MessagePack frame to an async writer.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    data: &[u8],
) -> std::io::Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(data).await?;
    writer.flush().await?;
    Ok(())
}

/// Serialize a message to a MessagePack frame and write it.
pub async fn send_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> std::io::Result<()> {
    // Must use named/map serialization (not array) because MessageBody uses
    // #[serde(tag = "type")] internally-tagged representation, which requires
    // map keys to embed the discriminant.
    let data = rmp_serde::to_vec_named(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    write_frame(writer, &data).await
}

/// Read a frame and deserialize it as a Message.
pub async fn recv_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> std::io::Result<Message> {
    let data = read_frame(reader).await?;
    rmp_serde::from_slice(&data).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Top-level message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique message ID for request/response correlation.
    pub id: u64,
    /// A client-supplied label used for signal-delivery addressing. It is NOT an
    /// identity: a client can set it to anything, so it is never used for
    /// authorization. Who a peer *is* comes from the kernel socket credential
    /// (SO_PEERCRED), and what names it owns comes from the bus registry; to emit
    /// a signal as a name, the peer must actually hold that registration.
    pub sender: String,
    /// A RookGuard capability token, verified server-side and bound to the
    /// connecting peer. Present only for operations the policy escalates
    /// (Elevated): running as another user, or administering system / other
    /// users' services. Ambient (own-scope) operations carry no token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    pub body: MessageBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum MessageBody {
    // ----- Service management -----
    /// Request to start a service by name.
    StartService { service: String },
    /// Request to stop a service by name.
    StopService { service: String },
    /// Request to reload a service by name.
    ReloadService { service: String },
    /// Request to list all registered services.
    ListServices,
    /// Request to rescan service directories for new .rsc files.
    Rescan,

    // ----- Bus registry -----
    /// Register a service on the bus with its methods and socket path.
    Register {
        name: String,
        socket_path: PathBuf,
        /// Methods this service exposes (method name -> description).
        methods: HashMap<String, String>,
    },
    /// Unregister a service from the bus.
    Unregister { name: String },
    /// Look up a service's socket path and available methods.
    Lookup { name: String },
    /// List all services registered on the bus.
    ListBus,

    // ----- Signals -----
    /// Subscribe to signals emitted by a service.
    /// Use signal = "*" to subscribe to all signals from that service.
    Subscribe { service: String, signal: String },
    /// Unsubscribe from a signal.
    Unsubscribe { service: String, signal: String },
    /// Emit a signal to all subscribers.
    /// Only the registered owner of `service` can emit signals for it.
    EmitSignal {
        signal: String,
        /// Opaque payload — subscribers interpret this themselves.
        payload: Vec<u8>,
    },
    /// Delivered to subscribers when a signal fires.
    /// This is sent server -> client, not client -> server.
    SignalDelivery {
        source: String,
        signal: String,
        payload: Vec<u8>,
    },

    // ----- Seat (device arbitration & FD passing) -----
    /// Request a seat for the calling session. Rev opens the device nodes
    /// and passes FDs back over the socket using SCM_RIGHTS.
    /// The response is an `Ok` — the FDs arrive as ancillary data on the
    /// same connection immediately after.
    OpenDevice {
        /// Device path, e.g. "/dev/dri/card0" or "/dev/input/event3".
        path: String,
    },
    /// Release a previously opened device. Rev closes its FD and revokes
    /// access (useful on VT switch / session deactivation).
    CloseDevice {
        path: String,
    },
    /// Notification from Rev that a session is being switched away.
    /// The compositor should release its devices and stop rendering.
    /// Sent server -> client.
    PauseDevice {
        path: String,
    },
    /// Notification from Rev that a session is being switched to.
    /// The compositor can re-acquire devices and resume.
    /// Sent server -> client.
    ResumeDevice {
        path: String,
    },
    /// Request Rev to restore the VT to text mode.
    /// Compositor sends this on shutdown since it may not have root
    /// access to open /dev/tty0 and call KDSETMODE/VT_ACTIVATE.
    RestoreVt,

    // ----- Privilege escalation (sudo replacement) -----
    /// Request to execute a command as a different user (typically root).
    /// Requires a valid auth_token in the message envelope.
    /// Rev verifies via Rook Guard, sanitizes the environment, then
    /// fork()/setuid()/exec()s the requested process.
    ExecAs {
        /// Target user to run as (0 = root).
        uid: u32,
        /// Command and arguments.
        command: Vec<String>,
        /// Extra environment variables to set.
        env: HashMap<String, String>,
        /// Working directory. None = inherit.
        working_dir: Option<PathBuf>,
    },
    /// Response to ExecAs — the PID of the spawned process.
    ExecAsResult {
        pid: u32,
    },

    // ----- Session management -----
    /// Request to start a new user session (login).
    /// Rev will: start the User Lane, fork/setuid/exec the session command
    /// (typically a compositor), and track the session.
    StartSession {
        /// User ID to create session for.
        uid: u32,
        /// Group ID.
        gid: u32,
        /// Username (for home dir, env setup).
        username: String,
        /// Command to launch (e.g. ["/Core/Bin/sway"]).
        command: Vec<String>,
        /// Extra environment variables.
        env: HashMap<String, String>,
    },
    /// Response to StartSession.
    SessionStarted {
        /// Session ID (unique per active session).
        session_id: u64,
        /// PID of the session's main process.
        pid: u32,
        /// Path to the User Lane socket for this session.
        lane_socket: PathBuf,
    },
    /// Request to terminate a user session.
    EndSession {
        session_id: u64,
    },
    /// List active sessions.
    ListSessions,
    /// Response to ListSessions.
    SessionList {
        sessions: Vec<SessionEntry>,
    },

    // ----- Responses -----
    /// Generic success with optional text.
    Ok { message: String },
    /// Generic error.
    Error { message: String },
    /// Response to ListServices — full service info.
    ServiceList {
        services: Vec<(String, crate::parser::ServiceInfo)>,
    },
    /// Response to Lookup.
    LookupResult {
        name: String,
        socket_path: PathBuf,
        methods: HashMap<String, String>,
    },
    /// Response to ListBus — names and socket paths of all bus registrants.
    BusServiceList { services: Vec<BusEntry> },
}

/// An active session entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_id: u64,
    pub uid: u32,
    pub username: String,
    pub pid: u32,
    pub lane_socket: PathBuf,
}

/// A bus registry entry returned in ListBus responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEntry {
    pub name: String,
    pub socket_path: PathBuf,
    pub methods: HashMap<String, String>,
}
