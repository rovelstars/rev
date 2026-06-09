//! WireBus socket server (System Highway).
//!
//! Listens on a single Unix socket and handles service management,
//! bus registry operations, and signal fan-out.
//!
//! Wire format: length-prefixed MessagePack frames (see protocol.rs).

use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use super::protocol::{self, Message, MessageBody};
use super::registry;

/// Set socket file permissions so unprivileged compositors can connect.
///
/// The socket is set to mode 0770 (rwxrwx---) so root and the socket's
/// group can connect. The group is set to "video" — compositors should
/// run with this supplementary group.
///
/// If the "video" group doesn't exist (e.g. debug builds), falls back
/// to mode 0777 so any user can connect during development.
fn set_socket_permissions(socket_path: &str) {
    use std::ffi::CString;

    let c_path = match CString::new(socket_path) {
        Ok(p) => p,
        Err(_) => return,
    };

    if cfg!(debug_assertions) {
        // Debug mode: world-accessible so any user can connect without
        // needing group membership. Safe for development only.
        unsafe {
            libc::chmod(c_path.as_ptr(), 0o777);
        }
        return;
    }

    // Production: socket is root:video 0770 — compositors need the video
    // group (or a dedicated "seat" group) to connect.
    let group_name = CString::new("video").unwrap();
    let grp = unsafe { libc::getgrnam(group_name.as_ptr()) };

    if !grp.is_null() {
        let gid = unsafe { (*grp).gr_gid };
        unsafe {
            libc::chown(c_path.as_ptr(), 0, gid);
            libc::chmod(c_path.as_ptr(), 0o770);
        }
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            if let Ok(parent_c) = CString::new(parent.to_string_lossy().as_bytes()) {
                unsafe {
                    libc::chown(parent_c.as_ptr(), 0, gid);
                    libc::chmod(parent_c.as_ptr(), 0o770);
                }
            }
        }
    } else {
        eprintln!(
            "rev: WARNING: 'video' group not found, socket at {} may not be accessible",
            socket_path
        );
        unsafe {
            libc::chmod(c_path.as_ptr(), 0o770);
        }
    }
}

/// Result of handling a message: the response frame, plus an optional FD
/// to pass via SCM_RIGHTS immediately after the response (for OpenDevice).
struct HandleResult {
    response: Message,
    pass_fd: Option<RawFd>,
}

static NEXT_MSG_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed)
}

fn make_msg(request_id: u64, body: MessageBody) -> Message {
    Message {
        id: request_id,
        sender: "rev".to_string(),
        auth_token: None,
        body,
    }
}

fn reply(request_id: u64, body: MessageBody) -> (Message, Option<RawFd>) {
    (make_msg(request_id, body), None)
}

fn ok_reply(request_id: u64, message: impl Into<String>) -> (Message, Option<RawFd>) {
    reply(
        request_id,
        MessageBody::Ok {
            message: message.into(),
        },
    )
}

fn err_reply(request_id: u64, message: impl Into<String>) -> (Message, Option<RawFd>) {
    reply(
        request_id,
        MessageBody::Error {
            message: message.into(),
        },
    )
}

/// Connected clients keyed by their self-declared sender name.
/// Used for signal delivery — when a signal fires, we look up subscribers
/// and write to their writer halves.
type ClientWriters =
    Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<Message>>>>;

pub async fn run(socket_path: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);

    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(socket_path)?;

    // Set socket permissions so unprivileged users can connect.
    // Mode 0770: owner (root) + group (video/seat) can read/write/connect.
    // The parent directory should also be group-accessible.
    //
    // On RunixOS, the compositor user is expected to be in the "video" group
    // (or a dedicated "seat" group). The socket group is set accordingly.
    set_socket_permissions(socket_path);

    println!("rev: wirebus listening on {}", socket_path);

    let clients: ClientWriters = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let (stream, _) = listener.accept().await?;
        let clients = clients.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, clients).await {
                // Silence expected disconnects and garbage connections
                // (e.g. desktop services probing the socket file)
                match e.kind() {
                    std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe => {}
                    std::io::ErrorKind::InvalidData => {
                        eprintln!("rev: wirebus: rejected non-WireBus connection (bad data)");
                    }
                    _ => {
                        eprintln!("rev: wirebus client error: {}", e);
                    }
                }
            }
        });
    }
}

async fn handle_client(
    stream: UnixStream,
    clients: ClientWriters,
) -> std::io::Result<()> {
    // The connecting peer's uid, straight from the kernel (SO_PEERCRED). Used to
    // authorize privilege-granting requests; a client cannot forge it.
    let peer_uid = nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
        .ok()
        .map(|c| c.uid());
    let (mut reader, mut writer) = stream.into_split();

    // Each client gets a channel for receiving signal deliveries.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(64);
    let mut client_name: Option<String> = None;

    // Track devices opened by this client for cleanup on disconnect.
    let mut opened_devices: Vec<String> = Vec::new();

    let cleanup = |name: &Option<String>, devices: &[String]| {
        // Close all devices opened by this client
        if !devices.is_empty() {
            let session_id = crate::seat::active_session().unwrap_or(0);
            for path in devices {
                let _ = crate::seat::close_device(session_id, path);
            }
            println!(
                "rev: client {:?} disconnected, closed {} devices",
                name.as_deref().unwrap_or("unknown"),
                devices.len()
            );
        }
    };

    // Track whether the last request involved synchronous I/O (FD passing).
    // If so, we need to re-arm tokio's readability interest before the next
    // recv_message, because the synchronous sendmsg may have caused tokio's
    // edge-triggered epoll to miss the client's next request arriving.
    let mut needs_read_rearm = false;

    loop {
        // If the previous iteration did synchronous I/O on the raw fd,
        // force tokio to re-check readability. Without this, edge-triggered
        // epoll may have already consumed the EPOLLIN notification while we
        // were in sendmsg, and recv_message would hang waiting for an edge
        // that never fires (data is already in the kernel buffer).
        if needs_read_rearm {
            // readable() re-registers read interest with the reactor.
            // If data is already available, it returns immediately.
            reader.as_ref().readable().await?;
            needs_read_rearm = false;
        }

        tokio::select! {
            // Incoming request from client
            result = protocol::recv_message(&mut reader) => {
                let msg = match result {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        if let Some(ref name) = client_name {
                            clients.lock().await.remove(name);
                        }
                        cleanup(&client_name, &opened_devices);
                        return Ok(());
                    }
                    Err(e) => {
                        if let Some(ref name) = client_name {
                            clients.lock().await.remove(name);
                        }
                        cleanup(&client_name, &opened_devices);
                        return Err(e);
                    }
                };

                // Track client by sender name for signal delivery
                if client_name.is_none() && !msg.sender.is_empty() {
                    client_name = Some(msg.sender.clone());
                    clients.lock().await.insert(msg.sender.clone(), tx.clone());
                }

                // Track if this is an OpenDevice request (for cleanup)
                let device_path = if let MessageBody::OpenDevice { ref path } = msg.body {
                    Some(path.clone())
                } else {
                    None
                };

                let result = handle_message(&msg, &clients, peer_uid).await;

                // Send the response frame via tokio's async writer.
                protocol::send_message(&mut writer, &result.response).await?;

                // If this was an OpenDevice, send the FD via SCM_RIGHTS.
                // This is synchronous sendmsg on the raw fd — unavoidable
                // because SCM_RIGHTS requires sendmsg(). Retry on EAGAIN.
                if let Some(fd) = result.pass_fd {
                    let raw_fd = writer.as_ref().as_raw_fd();
                    loop {
                        match crate::seat::fd_passing::send_fd(raw_fd, fd) {
                            Ok(()) => break,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                writer.as_ref().writable().await?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    if let Some(path) = device_path {
                        opened_devices.push(path);
                    }
                    // Flag that we did sync I/O — next iteration must re-arm
                    // tokio's read interest before calling recv_message.
                    needs_read_rearm = true;
                }
            }

            // Signal delivery from another service
            Some(signal_msg) = rx.recv() => {
                protocol::send_message(&mut writer, &signal_msg).await?;
            }
        }
    }
}

async fn handle_message(msg: &Message, clients: &ClientWriters, peer_uid: Option<u32>) -> HandleResult {
    let id = msg.id;

    // Privilege-granting requests (ExecAs, StartSession) authorize against
    // RookGuard + UAC below, keyed on the unforgeable peer uid. Other requests
    // are non-privileged.
    // Privileged operations that will require auth:
    //   - Register on System Highway
    //   - Cross-lane access (User Lane -> System Highway)
    //   - Starting/stopping system services (non-user)

    let (response, pass_fd) = match &msg.body {
        // ----- Service management -----
        MessageBody::StartService { service } => handle_start_service(id, service),
        MessageBody::StopService { service } => handle_stop_service(id, service),
        MessageBody::ReloadService { service } => handle_reload_service(id, service),
        MessageBody::ListServices => {
            let services = crate::init::services::list_services();
            reply(id, MessageBody::ServiceList { services })
        }
        MessageBody::Rescan => {
            let found = rescan_services();
            ok_reply(id, format!("Rescanned: found {} new services", found))
        }

        // ----- Bus registry -----
        MessageBody::Register {
            name,
            socket_path,
            methods,
        } => match registry::register(name.clone(), socket_path.clone(), methods.clone()) {
            Ok(()) => ok_reply(id, "Registered"),
            Err(e) => err_reply(id, e),
        },
        MessageBody::Unregister { name } => match registry::unregister(name) {
            Ok(()) => ok_reply(id, "Unregistered"),
            Err(e) => err_reply(id, e),
        },
        MessageBody::Lookup { name } => match registry::lookup(name) {
            Some(reg) => reply(
                id,
                MessageBody::LookupResult {
                    name: reg.name,
                    socket_path: reg.socket_path,
                    methods: reg.methods,
                },
            ),
            None => err_reply(id, format!("service '{}' not found on bus", name)),
        },
        MessageBody::ListBus => {
            let services = registry::list();
            reply(id, MessageBody::BusServiceList { services })
        }

        // ----- Signals -----
        MessageBody::Subscribe { service, signal } => {
            match registry::subscribe(&msg.sender, service, signal) {
                Ok(()) => ok_reply(id, format!("Subscribed to {}:{}", service, signal)),
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::Unsubscribe { service, signal } => {
            match registry::unsubscribe(&msg.sender, service, signal) {
                Ok(()) => ok_reply(id, format!("Unsubscribed from {}:{}", service, signal)),
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::EmitSignal { signal, payload } => {
            // Find all subscribers and deliver the signal
            let subscribers = registry::get_signal_subscribers(&msg.sender, signal);
            let delivery = Message {
                id: next_id(),
                sender: "rev".to_string(),
                auth_token: None,
                body: MessageBody::SignalDelivery {
                    source: msg.sender.clone(),
                    signal: signal.clone(),
                    payload: payload.clone(),
                },
            };

            let clients_map = clients.lock().await;
            let mut delivered = 0u32;
            for sub_name in &subscribers {
                if let Some(tx) = clients_map.get(sub_name) {
                    if tx.try_send(delivery.clone()).is_ok() {
                        delivered += 1;
                    }
                }
            }
            ok_reply(
                id,
                format!(
                    "Signal '{}' delivered to {}/{} subscribers",
                    signal,
                    delivered,
                    subscribers.len()
                ),
            )
        }

        // ----- Seat (device arbitration) -----
        MessageBody::OpenDevice { path } => {
            // TODO: validate that the requesting session owns the active seat
            // For now, allow any connected client to open devices.
            // In production, this must check session ownership + Rook Guard auth.
            let session_id = crate::seat::active_session().unwrap_or(0);
            match crate::seat::open_device(session_id, path) {
                Ok(fd) => {
                    // Return the Ok response AND the FD to pass.
                    // handle_client writes the response frame first, then
                    // sends the raw FD via SCM_RIGHTS immediately after.
                    // The client reads the Ok, then calls recv_fd().
                    let response = make_msg(id, MessageBody::Ok {
                        message: format!("Opened device: {}", path),
                    });
                    (response, Some(fd))
                }
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::CloseDevice { path } => {
            let session_id = crate::seat::active_session().unwrap_or(0);
            match crate::seat::close_device(session_id, path) {
                Ok(()) => ok_reply(id, format!("Closed device: {}", path)),
                Err(e) => err_reply(id, e),
            }
        }
        // PauseDevice and ResumeDevice are server->client only
        MessageBody::PauseDevice { .. } | MessageBody::ResumeDevice { .. } => {
            err_reply(id, "PauseDevice/ResumeDevice are server-initiated only")
        }

        // VT restoration — compositor requests this on shutdown
        MessageBody::RestoreVt => {
            restore_vt();
            ok_reply(id, "VT restored to text mode")
        }

        // ----- Privilege escalation -----
        MessageBody::ExecAs {
            uid,
            command,
            env,
            working_dir,
        } => {
            // Authorize against RookGuard + UAC, keyed on the unforgeable peer
            // uid. Fail closed: no proof, no elevation.
            match crate::auth::authorize_elevation(peer_uid, msg.auth_token.as_deref()) {
                Ok(caller) => {
                    crate::logger::write_log(
                        "rev",
                        &format!("ExecAs authorized for {} -> run as uid {}", caller, uid),
                    );
                    match crate::session::exec_as(*uid, command, env, working_dir.as_ref()) {
                        Ok(pid) => reply(id, MessageBody::ExecAsResult { pid }),
                        Err(e) => err_reply(id, e),
                    }
                }
                Err(e) => {
                    eprintln!("rev: ExecAs from '{}' denied: {}", msg.sender, e);
                    err_reply(id, format!("ExecAs denied: {e}"))
                }
            }
        }

        // ----- Session management -----
        MessageBody::StartSession {
            uid,
            gid,
            username,
            command,
            env,
        } => {
            // Spawning a session as an arbitrary user is privileged: only a root
            // caller (the login manager / greeter, which has already
            // authenticated the user via RookGuard) may do it. A session for
            // uid 0 would otherwise run a root shell for anyone.
            if peer_uid != Some(0) {
                eprintln!(
                    "rev: StartSession from '{}' denied: caller is not privileged",
                    msg.sender
                );
                err_reply(id, "StartSession denied: caller is not privileged".to_string())
            } else {
                match crate::session::start_session(*uid, *gid, username, command, env) {
                    Ok(session) => {
                        crate::seat::set_active_session(session.session_id);
                        reply(
                            id,
                            MessageBody::SessionStarted {
                                session_id: session.session_id,
                                pid: session.pid,
                                lane_socket: session.lane_socket,
                            },
                        )
                    }
                    Err(e) => err_reply(id, e),
                }
            }
        }
        MessageBody::EndSession { session_id } => {
            match crate::session::end_session(*session_id) {
                Ok(()) => ok_reply(id, format!("Session {} ended", session_id)),
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::ListSessions => {
            let sessions = crate::session::list_sessions();
            let entries: Vec<protocol::SessionEntry> = sessions
                .iter()
                .map(|s| protocol::SessionEntry {
                    session_id: s.session_id,
                    uid: s.uid,
                    username: s.username.clone(),
                    pid: s.pid,
                    lane_socket: s.lane_socket.clone(),
                })
                .collect();
            reply(id, MessageBody::SessionList { sessions: entries })
        }

        // ExecAsResult / SessionStarted / SessionList are response-only
        _ => err_reply(id, "unexpected message type"),
    };

    HandleResult { response, pass_fd }
}

fn handle_start_service(id: u64, name: &str) -> (Message, Option<RawFd>) {
    let (app_id, _service, file) = match crate::cli::parse_service::parse_service(name) {
        Ok(v) => v,
        Err(e) => return err_reply(id, format!("invalid service name: {}", e)),
    };
    let service_dir = std::path::PathBuf::from(format!("./Services/{}", app_id));
    match file.file_name() {
        Some(filename) => {
            crate::service::start_service_from_path(&service_dir.join(filename));
            ok_reply(id, format!("Started service: {}", name))
        }
        None => err_reply(id, "invalid service file path"),
    }
}

fn handle_stop_service(id: u64, name: &str) -> (Message, Option<RawFd>) {
    match crate::init::services::get_service(name) {
        Some(info) => {
            crate::service::stop_service(&info);
            ok_reply(id, format!("Stopped service: {}", name))
        }
        None => err_reply(id, format!("service '{}' not found", name)),
    }
}

fn handle_reload_service(id: u64, name: &str) -> (Message, Option<RawFd>) {
    match crate::init::services::get_service(name) {
        Some(info) => {
            if let Some(ref reload_cmd) = info.config.exec_reload {
                if info.pid.is_some() {
                    crate::service::run_hook(reload_cmd, &info.config);
                    ok_reply(id, format!("Reloaded service: {}", name))
                } else {
                    err_reply(id, format!("service '{}' is not running", name))
                }
            } else if let Some(pid) = info.pid {
                // Fallback: send SIGHUP
                unsafe {
                    libc::kill(pid as i32, libc::SIGHUP);
                }
                ok_reply(id, format!("Sent SIGHUP to service: {}", name))
            } else {
                err_reply(id, format!("service '{}' is not running", name))
            }
        }
        None => err_reply(id, format!("service '{}' not found", name)),
    }
}

/// Restore the VT to text mode. Called on behalf of unprivileged compositors.
/// Rev runs as root so it can access /dev/tty0.
fn restore_vt() {
    const KDSETMODE: libc::c_ulong = 0x4B3A;
    const KD_TEXT: libc::c_int = 0x00;
    const VT_SETMODE: libc::c_ulong = 0x5602;
    const VT_AUTO: libc::c_char = 0x00;
    const VT_GETSTATE: libc::c_ulong = 0x5603;
    const VT_ACTIVATE: libc::c_ulong = 0x5606;

    #[repr(C)]
    struct VtMode {
        mode: libc::c_char,
        waitv: libc::c_char,
        relsig: libc::c_short,
        acqsig: libc::c_short,
        frsig: libc::c_short,
    }

    #[repr(C)]
    struct VtStat {
        v_active: libc::c_ushort,
        v_signal: libc::c_ushort,
        v_state: libc::c_ushort,
    }

    let tty = unsafe { libc::open(c"/dev/tty0".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if tty < 0 {
        eprintln!("rev: restore_vt: could not open /dev/tty0");
        return;
    }

    unsafe {
        let vtm = VtMode { mode: VT_AUTO, waitv: 0, relsig: 0, acqsig: 0, frsig: 0 };
        libc::ioctl(tty, VT_SETMODE, &vtm);
        libc::ioctl(tty, KDSETMODE, KD_TEXT);

        let mut vt = VtStat { v_active: 0, v_signal: 0, v_state: 0 };
        if libc::ioctl(tty, VT_GETSTATE, &mut vt) == 0 && vt.v_active > 0 {
            libc::ioctl(tty, VT_ACTIVATE, vt.v_active as libc::c_int);
        }

        libc::close(tty);
    }

    println!("rev: VT restored to text mode");
}

fn rescan_services() -> u32 {
    let dirs = crate::parser::service_dirs();
    let mut found = 0u32;
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rsc") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                if crate::init::services::get_service(name).is_none() {
                    if let Ok(text) = std::fs::read_to_string(path) {
                        if let Ok(config) =
                            toml::from_str::<crate::parser::ServiceConfig>(&text)
                        {
                            let svc_name = config.name.clone();
                            crate::init::services::register_service(
                                svc_name,
                                crate::parser::ServiceInfo {
                                    name: config.name.clone(),
                                    config_path: Some(path.display().to_string()),
                                    config,
                                    ..Default::default()
                                },
                            );
                            found += 1;
                        }
                    }
                }
            }
        }
    }
    found
}
