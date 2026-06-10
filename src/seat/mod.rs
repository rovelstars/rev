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

// DRM master ioctls (asm-generic _IO encoding, type 'd' = 0x64). Becoming DRM
// master is what lets the fd holder modeset. Modern kernels do NOT auto-grant
// master on first open, and SET_MASTER requires CAP_SYS_ADMIN -- which Rev has
// (it is root) but an unprivileged compositor does not. So Rev must become
// master here, while it holds the fd, before passing it on: master lives on the
// open file description and rides the SCM_RIGHTS pass to the compositor, which
// can then drive KMS without any privilege of its own. Validated end to end in
// a virtio-gpu VM (see Rignite/seat-fdpass-smoke).
const DRM_IOCTL_SET_MASTER: libc::c_ulong = 0x0000_641e;
const DRM_IOCTL_DROP_MASTER: libc::c_ulong = 0x0000_641f;

// EVIOCREVOKE (_IOW('E', 0x91, int)) revokes an evdev open file description.
// Closing Rev's own fd does NOT cut off a compositor that holds an SCM_RIGHTS
// dup: the dup is a separate fd on the same struct file. EVIOCREVOKE kills the
// struct file itself, so every holder (the compositor's dup included) gets
// ENODEV. This is the only correct way to take a backgrounded session's input
// away on a VT switch or teardown -- otherwise it keeps reading the keyboard.
// Validated in a VM (Rignite/seat-fdpass-smoke).
const EVIOCREVOKE: libc::c_ulong = 0x4004_4591;

/// True for an evdev input node (/dev/input/event*), the only kind EVIOCREVOKE
/// applies to.
fn is_evdev(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("event"))
        .unwrap_or(false)
}

/// True for a DRM primary node (/dev/dri/card*), which is the one that carries
/// modeset/master rights. Render nodes (/dev/dri/renderD*) never need master.
fn is_drm_primary(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("card"))
        .unwrap_or(false)
}

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

    // For a primary DRM node, become master now (Rev is root) so the modeset
    // authority rides the fd to the unprivileged compositor. Non-fatal: a setup
    // that auto-grants master returns EINVAL/EBUSY here and still works.
    if is_drm_primary(&canon) {
        let r = unsafe { libc::ioctl(fd, DRM_IOCTL_SET_MASTER, 0) };
        if r != 0 {
            crate::logger::write_log(
                "rev",
                &format!(
                    "seat: SET_MASTER on {} failed: {} (continuing; compositor may not be able to modeset)",
                    path,
                    std::io::Error::last_os_error()
                ),
            );
        }
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
            // Revoke before closing so the compositor's SCM_RIGHTS dup dies too.
            if is_evdev(&dev.path) {
                unsafe { libc::ioctl(dev.fd, EVIOCREVOKE, 0); }
            }
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
            if is_evdev(&dev.path) {
                unsafe { libc::ioctl(dev.fd, EVIOCREVOKE, 0); }
            }
            unsafe { libc::close(dev.fd); }
        }
    }
}

/// Revoke all of a session's input fds without closing them, cutting the
/// compositor off from the keyboard/mouse. Called on VT switch away (alongside
/// PauseDevice). The fds stay tracked so a fresh open can be handed back on
/// resume.
#[allow(dead_code)] // wired in with the VT-switch pause/resume path
pub fn revoke_input(session_id: u64) {
    let state = STATE.lock().expect("seat state lock poisoned");
    if let Some(devices) = state.session_devices.get(&session_id) {
        for dev in devices {
            if is_evdev(&dev.path) {
                unsafe { libc::ioctl(dev.fd, EVIOCREVOKE, 0); }
            }
        }
    }
}

/// Drop DRM master on all of a session's primary nodes. Called on VT switch
/// away (alongside PauseDevice) so the incoming session can become master.
#[allow(dead_code)] // wired in with the VT-switch pause/resume path
pub fn drop_master(session_id: u64) {
    master_ioctl_for_session(session_id, DRM_IOCTL_DROP_MASTER);
}

/// Reacquire DRM master on all of a session's primary nodes. Called on VT
/// switch back (alongside ResumeDevice).
#[allow(dead_code)] // wired in with the VT-switch pause/resume path
pub fn set_master(session_id: u64) {
    master_ioctl_for_session(session_id, DRM_IOCTL_SET_MASTER);
}

#[allow(dead_code)]
fn master_ioctl_for_session(session_id: u64, req: libc::c_ulong) {
    let state = STATE.lock().expect("seat state lock poisoned");
    if let Some(devices) = state.session_devices.get(&session_id) {
        for dev in devices {
            if is_drm_primary(&dev.path) {
                unsafe { libc::ioctl(dev.fd, req, 0) };
            }
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
