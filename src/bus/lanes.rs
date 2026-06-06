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
use std::sync::Arc;
use tokio::sync::Mutex;

/// Tracks active User Lanes.
pub struct LaneManager {
    /// uid -> LaneHandle (join handle + socket path)
    active: Arc<Mutex<HashMap<u32, LaneHandle>>>,
}

struct LaneHandle {
    socket_path: PathBuf,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

/// Returns the socket path for a user lane.
pub fn user_lane_path(uid: u32) -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(format!("./rev-user-{}.sock", uid))
    } else {
        PathBuf::from(format!("/Transit/Ephemeral/rev/user/{}/bus.sock", uid))
    }
}

/// Returns the service directory for a user's personal services.
pub fn user_service_dir(username: &str) -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(format!("./UserServices/{}", username))
    } else {
        PathBuf::from(format!("/Space/{}/.Services", username))
    }
}

impl LaneManager {
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start a User Lane for the given uid.
    /// Spawns a WireBus server on the user's lane socket.
    /// Returns the socket path, or an error if the lane is already active.
    pub async fn start_lane(&self, uid: u32) -> Result<PathBuf, String> {
        let mut active = self.active.lock().await;
        if active.contains_key(&uid) {
            return Err(format!("user lane for uid {} is already active", uid));
        }

        let socket_path = user_lane_path(uid);
        let socket_str = socket_path.to_string_lossy().to_string();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = super::server::run(&socket_str) => {
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
            },
        );

        println!("rev: started user lane for uid {} at {}", uid, socket_path.display());
        Ok(socket_path)
    }

    /// Stop a User Lane for the given uid.
    pub async fn stop_lane(&self, uid: u32) -> Result<(), String> {
        let mut active = self.active.lock().await;
        match active.remove(&uid) {
            Some(handle) => {
                let _ = handle.shutdown_tx.send(());
                let _ = std::fs::remove_file(&handle.socket_path);
                Ok(())
            }
            None => Err(format!("no active lane for uid {}", uid)),
        }
    }

    /// Check if a user lane is active.
    pub async fn is_active(&self, uid: u32) -> bool {
        let active = self.active.lock().await;
        active.contains_key(&uid)
    }

    /// List all active user lanes.
    pub async fn list_lanes(&self) -> Vec<(u32, PathBuf)> {
        let active = self.active.lock().await;
        active
            .iter()
            .map(|(uid, handle)| (*uid, handle.socket_path.clone()))
            .collect()
    }
}
