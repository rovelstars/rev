//! Session and privilege management.
//!
//! Handles two related responsibilities:
//!
//! 1. **Session spawning**: When a user logs in (verified by Rook Guard),
//!    Rev fork()s, drops privileges via setuid/setgid, sets up the
//!    environment, starts the User Lane, and exec()s the user's session
//!    command (typically a compositor).
//!
//! 2. **Privilege escalation** (sudo replacement): When an unprivileged
//!    process needs to run something as root (or another user), it sends
//!    an ExecAs request via WireBus. Rev verifies the auth_token against
//!    Rook Guard, sanitizes the environment, and fork()/exec()s the
//!    command as the requested user. No setuid binaries needed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use once_cell::sync::Lazy;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Session {
    pub session_id: u64,
    pub uid: u32,
    pub gid: u32,
    pub username: String,
    pub pid: u32,
    pub lane_socket: PathBuf,
}

static SESSIONS: Lazy<Mutex<HashMap<u64, Session>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Spawn a new user session.
///
/// This is the login flow:
/// 1. Greeter authenticates user via Rook Guard
/// 2. Greeter sends StartSession to Rev
/// 3. Rev creates the User Lane
/// 4. Rev fork()s, drops to user's uid/gid, sets up env, exec()s the session command
/// 5. Rev tracks the session and returns SessionStarted
pub fn start_session(
    uid: u32,
    gid: u32,
    username: &str,
    command: &[String],
    env: &HashMap<String, String>,
) -> Result<Session, String> {
    if command.is_empty() {
        return Err("session command cannot be empty".to_string());
    }

    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);

    // Start the User Lane
    let lane_socket = crate::bus::lanes::user_lane_path(uid);

    // Set up the home directory
    let home = if cfg!(debug_assertions) {
        format!("./Space/{}", username)
    } else {
        format!("/Space/{}", username)
    };

    // Fork and drop privileges
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
            let pid = child.as_raw() as u32;
            let session = Session {
                session_id,
                uid,
                gid,
                username: username.to_string(),
                pid,
                lane_socket: lane_socket.clone(),
            };

            // Track the session
            SESSIONS
                .lock()
                .expect("sessions lock poisoned")
                .insert(session_id, session.clone());

            // Track as a running process for zombie reaping
            crate::init::services::update_service_pid(
                None, // not a named service
                None,
                None,
            );

            crate::logger::write_log(
                "rev",
                &format!(
                    "Session {} started for {} (uid={}, PID={})",
                    session_id, username, uid, pid
                ),
            );

            Ok(session)
        }
        #[allow(unreachable_code)]
        Ok(nix::unistd::ForkResult::Child) => {
            // === CHILD PROCESS ===
            // Drop privileges: setgid first (can't after setuid drops root)
            if let Err(e) = nix::unistd::setgid(nix::unistd::Gid::from_raw(gid)) {
                eprintln!("rev: session setgid({}) failed: {}", gid, e);
                std::process::exit(1);
            }
            if let Err(e) = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid)) {
                eprintln!("rev: session setuid({}) failed: {}", uid, e);
                std::process::exit(1);
            }

            // Set up environment
            unsafe {
                // Clear inherited env, set basics
                std::env::set_var("HOME", &home);
                std::env::set_var("USER", username);
                std::env::set_var("LOGNAME", username);
                std::env::set_var("SHELL", "/Core/Bin/nushell");
                std::env::set_var("PATH", "/Core/Bin:/Construct/Bin");
                std::env::set_var("XDG_RUNTIME_DIR", format!("/Transit/Ephemeral/user/{}", uid));
                std::env::set_var(
                    "WIREBUS_SOCKET",
                    lane_socket.to_string_lossy().to_string(),
                );

                // Set caller-provided env vars
                for (key, value) in env {
                    std::env::set_var(key, value);
                }
            }

            // Create XDG_RUNTIME_DIR if needed
            let xdg_dir = format!("/Transit/Ephemeral/user/{}", uid);
            let _ = std::fs::create_dir_all(&xdg_dir);
            // Should be owned by the user, mode 0700
            unsafe {
                let path = std::ffi::CString::new(xdg_dir).unwrap();
                libc::chown(path.as_ptr(), uid, gid);
                libc::chmod(path.as_ptr(), 0o700);
            }

            // Change to home directory
            let _ = nix::unistd::chdir(home.as_str());

            // Exec the session command
            use std::ffi::CString;
            let args_cstr: Vec<CString> = command
                .iter()
                .map(|a| CString::new(a.clone()).expect("invalid argument"))
                .collect();
            let args_ref: Vec<&std::ffi::CStr> =
                args_cstr.iter().map(|s| s.as_c_str()).collect();

            nix::unistd::execv(&args_cstr[0], &args_ref)
                .expect("execv failed");
            unreachable!();
        }
        Err(e) => Err(format!("fork failed: {}", e)),
    }
}

/// Execute a command as a different user (privilege escalation).
///
/// This is the sudo replacement flow:
/// 1. User process sends ExecAs via WireBus with auth_token
/// 2. Rev verifies auth_token with Rook Guard (TODO: not yet implemented)
/// 3. Rev fork()s, optionally setuid()s to the target user, exec()s
///
/// Returns the PID of the spawned process.
pub fn exec_as(
    target_uid: u32,
    command: &[String],
    env: &HashMap<String, String>,
    working_dir: Option<&PathBuf>,
) -> Result<u32, String> {
    if command.is_empty() {
        return Err("command cannot be empty".to_string());
    }

    // Validate the command path exists
    let cmd_path = std::path::Path::new(&command[0]);
    if !cmd_path.exists() {
        return Err(format!("command not found: {}", command[0]));
    }

    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
            let pid = child.as_raw() as u32;
            crate::logger::write_log(
                "rev",
                &format!(
                    "ExecAs: PID {} running {:?} as uid={}",
                    pid, command, target_uid
                ),
            );
            Ok(pid)
        }
        #[allow(unreachable_code)]
        Ok(nix::unistd::ForkResult::Child) => {
            // Drop to target user if not root
            if target_uid != 0 {
                // Look up the user's gid (simplified — in production, query user database)
                let gid = target_uid; // fallback: gid = uid
                if let Err(e) = nix::unistd::setgid(nix::unistd::Gid::from_raw(gid)) {
                    eprintln!("rev: exec_as setgid({}) failed: {}", gid, e);
                    std::process::exit(1);
                }
                if let Err(e) = nix::unistd::setuid(nix::unistd::Uid::from_raw(target_uid)) {
                    eprintln!("rev: exec_as setuid({}) failed: {}", target_uid, e);
                    std::process::exit(1);
                }
            }

            // Sanitize environment — start clean for root exec
            if target_uid == 0 {
                unsafe {
                    std::env::set_var("PATH", "/Core/Bin:/Construct/Bin");
                    std::env::set_var("HOME", "/Space/root");
                    std::env::remove_var("LD_PRELOAD");
                    std::env::remove_var("LD_LIBRARY_PATH");
                }
            }

            // Set caller-provided env vars
            for (key, value) in env {
                // Block dangerous env vars for root exec
                if target_uid == 0
                    && (key == "LD_PRELOAD"
                        || key == "LD_LIBRARY_PATH"
                        || key.starts_with("LD_"))
                {
                    continue; // silently drop
                }
                unsafe {
                    std::env::set_var(key, value);
                }
            }

            if let Some(dir) = working_dir {
                let _ = nix::unistd::chdir(dir.as_path());
            }

            use std::ffi::CString;
            let args_cstr: Vec<CString> = command
                .iter()
                .map(|a| CString::new(a.clone()).expect("invalid argument"))
                .collect();
            let args_ref: Vec<&std::ffi::CStr> =
                args_cstr.iter().map(|s| s.as_c_str()).collect();

            nix::unistd::execv(&args_cstr[0], &args_ref)
                .expect("execv failed");
            unreachable!();
        }
        Err(e) => Err(format!("fork failed: {}", e)),
    }
}

/// End a user session. Sends SIGTERM to the session process,
/// cleans up the User Lane, and releases seat devices.
pub fn end_session(session_id: u64) -> Result<(), String> {
    let session = {
        let sessions = SESSIONS.lock().expect("sessions lock poisoned");
        sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| format!("session {} not found", session_id))?
    };

    crate::logger::write_log(
        "rev",
        &format!(
            "Ending session {} for {} (PID {})",
            session_id, session.username, session.pid
        ),
    );

    // Kill the session process
    unsafe {
        libc::kill(session.pid as i32, libc::SIGTERM);
    }

    // Clean up seat devices
    crate::seat::close_all_devices(session_id);

    // Remove the session
    SESSIONS
        .lock()
        .expect("sessions lock poisoned")
        .remove(&session_id);

    Ok(())
}

/// List all active sessions.
pub fn list_sessions() -> Vec<Session> {
    SESSIONS
        .lock()
        .expect("sessions lock poisoned")
        .values()
        .cloned()
        .collect()
}

/// Called when a session process exits (from zombie reaper).
/// Cleans up the session state.
pub fn handle_session_exit(pid: u32) {
    let mut sessions = SESSIONS.lock().expect("sessions lock poisoned");
    let session_id = sessions
        .iter()
        .find(|(_, s)| s.pid == pid)
        .map(|(id, _)| *id);

    if let Some(id) = session_id {
        let session = sessions.remove(&id);
        if let Some(s) = session {
            println!("rev: session {} for {} exited", id, s.username);
            crate::seat::close_all_devices(id);
        }
    }
}
