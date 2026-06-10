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

use super::policy::{self, Access, Operation, Principal, Scope, Tier};
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
    /// For a successful OpenDevice, the session the device was opened under, so
    /// the connection can close it under the *same* session on disconnect rather
    /// than guessing the (possibly since-changed) active session.
    device_session: Option<u64>,
}

/// The seat session a principal may act under. The policy has already confirmed
/// the principal is allowed to touch seat devices (active-session owner or root);
/// this only names the session: the owner acts under its own session, root acts
/// under whatever is currently active. Keeps the device-fd handout bound to the
/// foreground session so a background or cross-user process can never grab the
/// logged-in user's keyboard or screen.
fn seat_session(principal: &Principal) -> Option<u64> {
    match principal {
        Principal::SessionOwner { session_id, .. } => Some(*session_id),
        Principal::System => crate::seat::active_session(),
        _ => None,
    }
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

/// Run a WireBus server on `socket_path`. `tier` says whether this is the system
/// Highway or a user Lane; it is threaded into every request so the policy can
/// keep privileged and cross-scope operations off user lanes.
pub async fn run(socket_path: &str, tier: Tier) -> std::io::Result<()> {
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
            if let Err(e) = handle_client(stream, clients, tier).await {
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
    tier: Tier,
) -> std::io::Result<()> {
    // The connecting peer's uid, straight from the kernel (SO_PEERCRED). A client
    // cannot forge it; it is the root of the principal we authorize against.
    let peer_uid = nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
        .ok()
        .map(|c| c.uid());

    // On a user Lane, the only legitimate peer is the lane's owner (or root).
    // The filesystem perms enforce this too (uid:0700), but assert it here so a
    // perms mistake cannot silently open one user's lane to another.
    if let Tier::Lane { uid: lane_uid } = tier {
        if !matches!(peer_uid, Some(u) if u == lane_uid || u == 0) {
            return Ok(()); // drop: not the lane owner
        }
    }

    // Resolve who this is, once, from unforgeable inputs.
    let principal = resolve_principal(peer_uid);

    let (mut reader, mut writer) = stream.into_split();

    // Each client gets a channel for receiving signal deliveries.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(64);
    let mut client_name: Option<String> = None;

    // Track devices opened by this client, each with the session it was opened
    // under, so disconnect cleanup closes it under the right session even if the
    // active session has since changed.
    let mut opened_devices: Vec<(u64, String)> = Vec::new();

    let cleanup = |name: &Option<String>, devices: &[(u64, String)]| {
        // Close all devices opened by this client
        if !devices.is_empty() {
            for (session_id, path) in devices {
                let _ = crate::seat::close_device(*session_id, path);
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

                let result = handle_message(&msg, &clients, principal, tier).await;

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
                    if let (Some(path), Some(sid)) = (device_path, result.device_session) {
                        opened_devices.push((sid, path));
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

/// Resolve the connecting peer into a [`Principal`] from unforgeable inputs: the
/// socket peer uid, rev's active-session table, and the UAC admin bit. Done once
/// per connection.
fn resolve_principal(peer_uid: Option<u32>) -> Principal {
    match peer_uid {
        None => Principal::Anonymous,
        Some(0) => Principal::System,
        Some(uid) => {
            // The active seat session's owner is the foreground compositor; mark
            // it so only it may touch seat devices.
            if let Some(sid) = crate::seat::active_session() {
                if crate::session::owner_uid(sid) == Some(uid) {
                    return Principal::SessionOwner { uid, session_id: sid };
                }
            }
            Principal::User { uid, admin: uac_is_admin(uid) }
        }
    }
}

/// Whether `uid` is a UAC administrator. A failure to resolve is treated as
/// not-admin (fail closed).
fn uac_is_admin(uid: u32) -> bool {
    let Ok(uac) = uac_core::Uac::open() else { return false };
    match uac.name_by_uid(uid) {
        Ok(Some(name)) => uac.is_admin(&name).unwrap_or(false),
        _ => false,
    }
}

/// Describe a request as the internal [`Operation`] the policy understands.
/// Returns `None` for response-only or server-initiated message bodies, which
/// are not valid client requests and fall through to an error reply without an
/// authorization decision. Service control is scoped by tier: on a Lane it
/// targets that user's own services; on the Highway it targets system services.
fn classify(body: &MessageBody, tier: Tier) -> Option<Operation> {
    let service_scope = match tier {
        Tier::Lane { uid } => Scope::OwnUser(uid),
        Tier::Highway => Scope::SystemOrOtherUser,
    };
    Some(match body {
        MessageBody::Lookup { .. }
        | MessageBody::ListBus
        | MessageBody::ListServices
        | MessageBody::ListSessions => Operation::Read,

        MessageBody::Subscribe { .. } | MessageBody::Unsubscribe { .. } => {
            Operation::SignalSubscribe
        }
        // Ownership of the name is enforced in phase 4 (registry owner tracking);
        // until then these route through the policy as owned (current behavior).
        MessageBody::EmitSignal { .. } => Operation::SignalEmit { owns_name: true },
        MessageBody::Register { .. } => Operation::Register { owns_namespace: true },
        MessageBody::Unregister { .. } => Operation::Unregister { owns_name: true },

        MessageBody::StartService { .. }
        | MessageBody::StopService { .. }
        | MessageBody::ReloadService { .. }
        | MessageBody::Rescan => Operation::ServiceControl { scope: service_scope },

        MessageBody::OpenDevice { .. }
        | MessageBody::CloseDevice { .. }
        | MessageBody::RestoreVt => Operation::Seat,

        MessageBody::ExecAs { .. } => Operation::ExecAs,
        MessageBody::StartSession { .. } => Operation::StartSession,
        MessageBody::EndSession { session_id } => Operation::EndSession {
            owner_uid: crate::session::owner_uid(*session_id),
        },

        // Responses and server-initiated notifications: not client requests.
        _ => return None,
    })
}

async fn handle_message(
    msg: &Message,
    clients: &ClientWriters,
    principal: Principal,
    tier: Tier,
) -> HandleResult {
    let id = msg.id;

    // The authorization choke point: every classifiable request passes through
    // policy::authorize before it is dispatched. This is the single place that
    // grants or denies; the handler arms below assume they are already
    // authorized and never re-check.
    if let Some(op) = classify(&msg.body, tier) {
        match policy::authorize(&principal, tier, &op) {
            Access::Allow => {}
            Access::Deny(reason) => {
                let (response, pass_fd) = err_reply(id, format!("denied: {reason}"));
                return HandleResult { response, pass_fd, device_session: None };
            }
            Access::Elevated(purpose) => {
                if let Err(e) = crate::auth::verify_for(principal.uid(), msg.auth_token.as_deref(), purpose) {
                    crate::logger::write_log(
                        "rev",
                        &format!("{op:?} denied for uid {:?}: {e}", principal.uid()),
                    );
                    let (response, pass_fd) = err_reply(id, format!("denied: {e}"));
                    return HandleResult { response, pass_fd, device_session: None };
                }
            }
        }
    }

    // Set by the OpenDevice arm to the session a device was opened under.
    let mut device_session: Option<u64> = None;

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
        // The choke point already confirmed the principal is the active session
        // owner or root; here we only resolve which session to act under.
        MessageBody::OpenDevice { path } => match seat_session(&principal) {
            Some(session_id) => match crate::seat::open_device(session_id, path) {
                Ok(fd) => {
                    // Return the Ok response AND the FD to pass.
                    // handle_client writes the response frame first, then
                    // sends the raw FD via SCM_RIGHTS immediately after.
                    // The client reads the Ok, then calls recv_fd().
                    device_session = Some(session_id);
                    let response = make_msg(id, MessageBody::Ok {
                        message: format!("Opened device: {}", path),
                    });
                    (response, Some(fd))
                }
                Err(e) => err_reply(id, e),
            },
            None => err_reply(id, "no active seat session to act on"),
        },
        MessageBody::CloseDevice { path } => match seat_session(&principal) {
            Some(session_id) => match crate::seat::close_device(session_id, path) {
                Ok(()) => ok_reply(id, format!("Closed device: {}", path)),
                Err(e) => err_reply(id, e),
            },
            None => err_reply(id, "no active seat session to act on"),
        },
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
        // The choke point verified the ElevateRoot token (or a root caller);
        // here we only run the command as the target.
        MessageBody::ExecAs {
            uid,
            command,
            env,
            working_dir,
        } => {
            crate::logger::write_log(
                "rev",
                &format!("ExecAs by uid {:?} -> run as uid {}", principal.uid(), uid),
            );
            match crate::session::exec_as(*uid, command, env, working_dir.as_ref()) {
                Ok(pid) => reply(id, MessageBody::ExecAsResult { pid }),
                Err(e) => err_reply(id, e),
            }
        }

        // ----- Session management -----
        // The choke point confirmed this is the system greeter (root).
        MessageBody::StartSession {
            uid,
            gid,
            username,
            command,
            env,
        } => match crate::session::start_session(*uid, *gid, username, command, env) {
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
        },
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

    HandleResult { response, pass_fd, device_session }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_resolution_and_seat_binding() {
        // A unique active session id for this test so it does not collide with
        // any other test touching the shared seat/session state.
        let sid = 90_001u64;
        let owner = 50_001u32;
        let other = 50_002u32;

        crate::session::register_test_session(sid, owner);
        crate::seat::set_active_session(sid);

        // The active session's owner resolves to SessionOwner with that session;
        // the policy then allows seat access (covered in bus::policy tests).
        let p = resolve_principal(Some(owner));
        assert_eq!(p, Principal::SessionOwner { uid: owner, session_id: sid });
        assert_eq!(seat_session(&p), Some(sid));

        // Root is System and acts under whatever session is active.
        assert_eq!(resolve_principal(Some(0)), Principal::System);
        assert_eq!(seat_session(&Principal::System), Some(sid));

        // A different user is a plain User (not the active compositor); the
        // policy denies its seat access, and it has no seat session to act under.
        assert!(matches!(resolve_principal(Some(other)), Principal::User { uid, .. } if uid == other));
        assert_eq!(seat_session(&Principal::User { uid: other, admin: false }), None);

        // No peer credential: Anonymous, denied everything by the policy.
        assert_eq!(resolve_principal(None), Principal::Anonymous);
    }
}
