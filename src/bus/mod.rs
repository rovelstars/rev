//! WireBus — RunixOS IPC protocol.
//!
//! Replaces D-Bus with a registry + peer-to-peer model built into Rev.
//! Services register on the bus with their socket path and methods.
//! Clients look up services through Rev, then connect directly (peer-to-peer).
//!
//! # Architecture
//!
//! - **System Highway**: root-level bus for system services, always running.
//!   Socket: /Transit/Ephemeral/rev/bus.sock (debug: ./rev.sock)
//!
//! - **User Lanes**: per-user bus scopes started on login at
//!   /Transit/Ephemeral/rev/user/<uid>/bus.sock — isolated from other users.
//!   Managed by LaneManager. A user service can request opt-in access to
//!   the System Highway (requires Rook Guard authorization).
//!
//! # Wire format
//!
//! Length-prefixed MessagePack: [4 bytes BE length][MessagePack payload]
//! Language-agnostic — any language with Unix sockets + MessagePack can participate.
//!
//! # Signals
//!
//! Services can emit signals and others can subscribe. Signals are fan-out:
//! Rev delivers to all connected subscribers. Payload is opaque bytes —
//! subscribers and emitters agree on format out-of-band.

pub mod lanes;
pub mod protocol;
pub mod registry;
pub mod server;

use std::path::PathBuf;

/// Returns the WireBus socket path for the System Highway.
pub fn socket_path() -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from("./rev.sock")
    } else {
        PathBuf::from("/Transit/Ephemeral/rev/bus.sock")
    }
}
