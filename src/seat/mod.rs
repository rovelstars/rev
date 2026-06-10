//! Seat management — device arbitration and FD passing.
//!
//! Replaces seatd/logind device access. Since Rev runs as PID 1 (root),
//! it can open restricted device nodes (/dev/dri/*, /dev/input/*) and pass
//! the file descriptors to unprivileged compositors via SCM_RIGHTS over
//! the WireBus Unix socket.
//!
//! # Flow
//!
//! 1. Compositor connects to WireBus, sends `OpenDevice { path: "/dev/dri/card0" }`
//! 2. Rev validates the request (session must own the active seat)
//! 3. Rev opens the device with O_RDWR | O_CLOEXEC
//! 4. Rev sends an `Ok` response, then passes the FD via SCM_RIGHTS ancillary data
//! 5. On VT switch, Rev sends `PauseDevice` and revokes access
//! 6. On VT switch back, Rev sends `ResumeDevice` with a fresh FD

pub mod fd_passing;

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;

/// Tracks which session owns which device FDs.
struct SeatState {
    /// session_id -> list of (device_path, fd)
    session_devices: HashMap<u64, Vec<DeviceRef>>,
    /// The currently active session (gets device access).
    active_session: Option<u64>,
}

struct DeviceRef {
    path: PathBuf,
    fd: RawFd,
}

static STATE: Lazy<Mutex<SeatState>> = Lazy::new(|| {
    Mutex::new(SeatState {
        session_devices: HashMap::new(),
        active_session: None,
    })
});

/// Allowlist of device path prefixes that Rev will open for compositors.
const ALLOWED_PREFIXES: &[&str] = &[
    "/dev/dri/",
    "/dev/input/",
];

/// Resolve `path` to its real location and confirm it is an allowed device
/// node, returning the canonical path. Returns None if it resolves outside the
/// allowlist (e.g. a `..` escape or a symlink to /dev/mem). We open this
/// canonical path rather than the caller's string so the check and the open
/// cannot disagree across a symlink swap (TOCTOU).
fn allowed_device_path(path: &str) -> Option<PathBuf> {
    let canon = std::fs::canonicalize(path).ok()?;
    let canon_str = canon.to_string_lossy();
    if ALLOWED_PREFIXES.iter().any(|prefix| canon_str.starts_with(prefix)) {
        Some(canon)
    } else {
        None
    }
}

/// Open a device node and return its file descriptor.
/// Only works for allowed device paths.
pub fn open_device(session_id: u64, path: &str) -> Result<RawFd, String> {
    let canon = allowed_device_path(path).ok_or_else(|| {
        format!(
            "device '{}' is not in the allowed list (only /dev/dri/* and /dev/input/*)",
            path
        )
    })?;

    let fd = unsafe {
        libc::open(
            std::ffi::CString::new(canon.as_os_str().as_encoded_bytes())
                .map_err(|_| "invalid device path")?
                .as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC,
        )
    };

    if fd < 0 {
        return Err(format!(
            "failed to open '{}': {}",
            path,
            std::io::Error::last_os_error()
        ));
    }

    // Track the FD
    let mut state = STATE.lock().expect("seat state lock poisoned");
    state
        .session_devices
        .entry(session_id)
        .or_default()
        .push(DeviceRef {
            path: PathBuf::from(path),
            fd,
        });

    Ok(fd)
}

/// Close a device previously opened for a session.
pub fn close_device(session_id: u64, path: &str) -> Result<(), String> {
    let mut state = STATE.lock().expect("seat state lock poisoned");
    if let Some(devices) = state.session_devices.get_mut(&session_id) {
        if let Some(idx) = devices.iter().position(|d| d.path.as_os_str() == path) {
            let dev = devices.remove(idx);
            unsafe { libc::close(dev.fd); }
            Ok(())
        } else {
            Err(format!("device '{}' not open for session {}", path, session_id))
        }
    } else {
        Err(format!("no devices for session {}", session_id))
    }
}

/// Close all devices for a session (on session teardown).
pub fn close_all_devices(session_id: u64) {
    let mut state = STATE.lock().expect("seat state lock poisoned");
    if let Some(devices) = state.session_devices.remove(&session_id) {
        for dev in devices {
            unsafe { libc::close(dev.fd); }
        }
    }
}

/// Set the active session. Only the active session can open new devices
/// and gets ResumeDevice notifications.
pub fn set_active_session(session_id: u64) {
    let mut state = STATE.lock().expect("seat state lock poisoned");
    state.active_session = Some(session_id);
}

/// Get all device paths currently open for a session.
#[allow(dead_code)]
pub fn get_session_devices(session_id: u64) -> Vec<PathBuf> {
    let state = STATE.lock().expect("seat state lock poisoned");
    state
        .session_devices
        .get(&session_id)
        .map(|devs| devs.iter().map(|d| d.path.clone()).collect())
        .unwrap_or_default()
}

/// Get the active session ID.
pub fn active_session() -> Option<u64> {
    let state = STATE.lock().expect("seat state lock poisoned");
    state.active_session
}
