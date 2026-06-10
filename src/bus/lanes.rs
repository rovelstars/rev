//! User Lanes — per-user WireBus scopes.
//!
//! Each user session gets its own isolated bus at:
//!   Debug:      ./rev-user-<uid>.sock
//!   Production: /Transit/Ephemeral/rev/user/<uid>/bus.sock
//!
//! User Lanes are started when a user logs in and torn down on logout.
//! Services on a User Lane cannot see services on other User Lanes.
//! A user service can request opt-in access to the System Highway,
//! which must be authorized by Rook Guard.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

/// The process-wide set of active User Lanes. A single instance so the session
/// handlers (async) and the zombie reaper (a plain thread) can both bring lanes
/// up and tear them down.
pub static LANES: Lazy<LaneManager> = Lazy::new(LaneManager::new);

/// Tracks active User Lanes.
pub struct LaneManager {
    /// uid -> LaneHandle (join handle + socket path)
    active: Arc<Mutex<HashMap<u32, LaneHandle>>>,
}

struct LaneHandle {
    socket_path: PathBuf,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    /// PIDs of the user's scope=user services started on this lane, killed when
    /// the lane is torn down (logout).
    service_pids: Vec<u32>,
}

/// Returns the socket path for a user lane.
pub fn user_lane_path(uid: u32) -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(format!("./rev-user-{}.sock", uid))
    } else {
        PathBuf::from(format!("/Transit/Ephemeral/rev/user/{}/bus.sock", uid))
    }
}

/// Returns the directory holding a user's personally-installed services, keyed
/// by the account's stable UUID.
///
/// These live in the account's vault, not the user's home: a home dotfolder is
/// cluttered, casually findable, and freely editable, whereas the vault tree is
/// root-owned and managed through rev's tooling. The UUID (rather than the
/// username) keys it so a rename does not strand the services.
pub fn user_service_dir(account_uuid: &str) -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(format!("./Vault/Services/{}", account_uuid))
    } else {
        PathBuf::from(format!("/Vault/Services/{}", account_uuid))
    }
}

impl LaneManager {
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start a User Lane for the given uid. Spawns a WireBus server on the
    /// user's lane socket and returns its path. Idempotent: if the lane is
    /// already active its existing socket path is returned. Must be called from
    /// within the tokio runtime (it spawns the lane server task).
    pub fn start_lane(&self, uid: u32) -> Result<PathBuf, String> {
        let mut active = self.active.lock().expect("lanes lock poisoned");
        if let Some(handle) = active.get(&uid) {
            return Ok(handle.socket_path.clone());
        }

        let socket_path = user_lane_path(uid);
        let socket_str = socket_path.to_string_lossy().to_string();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = super::server::run(&socket_str, super::policy::Tier::Lane { uid }) => {
                    if let Err(e) = result {
                        eprintln!("rev: user lane {} failed: {}", uid, e);
                    }
                }
                _ = shutdown_rx => {
                    // Clean shutdown — remove socket file
                    let _ = std::fs::remove_file(&path_clone);
                    println!("rev: user lane {} shut down", uid);
                }
            }
        });

        active.insert(
            uid,
            LaneHandle {
                socket_path: socket_path.clone(),
                shutdown_tx,
                service_pids: Vec::new(),
            },
        );

        println!("rev: started user lane for uid {} at {}", uid, socket_path.display());
        Ok(socket_path)
    }

    /// Record a scope=user service started on `uid`'s lane, so it is killed when
    /// the lane is torn down. No-op if the lane is not active.
    pub fn record_service(&self, uid: u32, pid: u32) {
        if let Some(handle) = self.active.lock().expect("lanes lock poisoned").get_mut(&uid) {
            handle.service_pids.push(pid);
        }
    }

    /// Stop a User Lane for the given uid. Sync, so the zombie reaper thread can
    /// call it on unexpected session exit: it SIGTERMs the user's services,
    /// signals the lane server task (running on the runtime) to shut down, and
    /// removes the socket file.
    pub fn stop_lane(&self, uid: u32) -> Result<(), String> {
        let mut active = self.active.lock().expect("lanes lock poisoned");
        match active.remove(&uid) {
            Some(handle) => {
                for pid in handle.service_pids {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
                let _ = handle.shutdown_tx.send(());
                let _ = std::fs::remove_file(&handle.socket_path);
                Ok(())
            }
            None => Err(format!("no active lane for uid {}", uid)),
        }
    }

    /// Check if a user lane is active.
    pub fn is_active(&self, uid: u32) -> bool {
        self.active.lock().expect("lanes lock poisoned").contains_key(&uid)
    }

    /// List all active user lanes.
    pub fn list_lanes(&self) -> Vec<(u32, PathBuf)> {
        self.active
            .lock()
            .expect("lanes lock poisoned")
            .iter()
            .map(|(uid, handle)| (*uid, handle.socket_path.clone()))
            .collect()
    }
}
