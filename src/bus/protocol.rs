//! WireBus wire protocol.
//!
//! The protocol types and frame I/O now live in the standalone `wirebus-proto`
//! crate, so peers (e.g. RookGuard) can speak WireBus without depending on rev.
//! This module re-exports them so existing `crate::bus::protocol::*` paths keep
//! working, and adds the rev-side conversion from the internal `ServiceInfo`
//! runtime state into the neutral wire `ServiceSnapshot`.

pub use wirebus_proto::*;

use crate::parser::ServiceInfo;

impl From<&ServiceInfo> for ServiceSnapshot {
    fn from(info: &ServiceInfo) -> Self {
        ServiceSnapshot {
            name: info.name.clone(),
            description: info.config.description.clone(),
            exec_start: info.config.exec_start.clone(),
            restart_policy: format!("{:?}", info.config.restart_policy),
            is_running: info.is_running,
            pid: info.pid,
            last_exit_code: info.last_exit_code,
            up_timestamp: info.up_timestamp.map(|ts| ts.timestamp()),
            restart_count: info.restart_count,
            memory_bytes: info.memory_bytes,
            cpu_seconds: info.cpu_seconds,
            tasks: info.tasks,
            config_path: info.config_path.clone(),
        }
    }
}
