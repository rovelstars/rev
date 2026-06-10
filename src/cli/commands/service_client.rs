//! Shared client path for system-service control (start/stop).
//!
//! `rev start`/`rev stop` act on the system Highway, where controlling a system
//! service is a cross-scope action. A root caller is authorized by its
//! credentials; a non-root caller must elevate, so we obtain a
//! SystemServiceControl token from RookGuard (the same handshake `sudo` uses)
//! and attach it to the request. The rev daemon verifies it at the choke point.

use crate::bus::protocol::{self, Message, MessageBody};
use tokio::net::UnixStream;

/// Send a service-control request, elevating first when the caller is not root.
pub async fn send_elevated(body: MessageBody) {
    let auth_token = match obtain_token().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rev: {e}");
            std::process::exit(1);
        }
    };

    let socket_path = crate::bus::socket_path();
    let stream = match UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rev: cannot connect to wirebus ({}): {}", socket_path.display(), e);
            std::process::exit(1);
        }
    };
    let (mut reader, mut writer) = stream.into_split();

    let msg = Message {
        id: 1,
        sender: "rev-cli".to_string(),
        auth_token,
        body,
    };
    if let Err(e) = protocol::send_message(&mut writer, &msg).await {
        eprintln!("rev: failed to send command: {}", e);
        std::process::exit(1);
    }

    match protocol::recv_message(&mut reader).await {
        Ok(response) => match response.body {
            MessageBody::Ok { message } => println!("{}", message),
            MessageBody::Error { message } => {
                eprintln!("rev: {}", message);
                std::process::exit(1);
            }
            _ => eprintln!("rev: unexpected response"),
        },
        Err(e) => {
            eprintln!("rev: failed to read response: {}", e);
            std::process::exit(1);
        }
    }
}

/// A SystemServiceControl token for the caller, or `None` if the caller is root
/// (root authorizes by its socket credentials and needs no token). The RookGuard
/// handshake is blocking (it may prompt for a password), so it runs off the
/// async runtime.
async fn obtain_token() -> Result<Option<String>, String> {
    let uid = nix::unistd::getuid().as_raw();
    if uid == 0 {
        return Ok(None);
    }
    tokio::task::spawn_blocking(move || {
        let uac = uac_core::Uac::open().map_err(|e| format!("open UAC: {e}"))?;
        let name = uac
            .name_by_uid(uid)
            .map_err(|e| format!("{e}"))?
            .ok_or_else(|| format!("uid {uid} is not a UAC account"))?;
        rook_elevate::client::acquire_token(
            &uac,
            &name,
            uid,
            rook_core::policy::Purpose::SystemServiceControl,
        )
        .map(Some)
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("elevation task failed: {e}"))?
}
