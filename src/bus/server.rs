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

/// Make the System Highway socket reachable by any local process.
///
/// The Highway is world-connectable by design: a normal user's compositor must
/// reach it for seat/session, and every request is individually authorized by
/// the policy (peer-credential principal + capability tokens), so connectivity
/// grants nothing on its own. This replaces the earlier root:video 0770 gate,
/// which was an incidental hack from the Epiclese bring-up and only ever a
/// coarse stand-in for the per-method checks now in place. The socket is mode
/// 0666 (connect needs read+write) and its directory 0755 so the path is
/// traversable; both stay root-owned so no user can replace them.
fn set_socket_permissions(socket_path: &str) {
    use std::ffi::CString;

    let Ok(c_path) = CString::new(socket_path) else { return };
    unsafe {
        libc::chmod(c_path.as_ptr(), 0o666);
    }
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        if let Ok(parent_c) = CString::new(parent.to_string_lossy().as_bytes()) {
            unsafe {
                libc::chown(parent_c.as_ptr(), 0, 0);
                libc::chmod(parent_c.as_ptr(), 0o755);
            }
        }
    }
}

/// The primary gid of a uid via UAC, falling back to the uid itself. With a
/// 0700 socket the group has no access anyway, so this is only cosmetic; we set
/// it so a lane socket is owned by the user's real group rather than root's.
fn lane_gid(uid: u32) -> u32 {
    let Ok(uac) = uac_core::Uac::open() else { return uid };
    match uac.name_by_uid(uid) {
        Ok(Some(name)) => uac.get(&name).map(|a| a.gid).unwrap_or(uid),
        _ => uid,
    }
}

/// Lock a user Lane socket (and its directory) to its owner: chown to the user,
/// chmod 0700. After this the kernel rejects any other user's connect() outright
/// -- the filesystem is the primary isolation boundary between lanes, with rev's
/// peer-uid check in handle_client as a backstop. The owning user must already
/// hold the per-uid directory; rev (root) created it. chown is best-effort
/// (it fails when rev is not root, e.g. dev builds); the 0700 mode is always set.
fn set_lane_permissions(socket_path: &str, uid: u32) {
    use std::ffi::CString;
    let gid = lane_gid(uid);
    let mode: libc::mode_t = 0o700;

    let chown_chmod = |path: &str| {
        if let Ok(c) = CString::new(path) {
            unsafe {
                // Ignore chown failure (non-root dev builds); enforce the mode.
                libc::chown(c.as_ptr(), uid, gid);
                libc::chmod(c.as_ptr(), mode);
            }
        }
    };

    // The dedicated per-uid directory first, then the socket inside it. We only
    // touch the parent when it is the lane's own directory (named for the uid);
    // this skips the flat dev-build layout, where the parent is the cwd. The
    // grandparent (.../user) stays root-owned so no user can squat a lane path.
    let path = std::path::Path::new(socket_path);
    if let Some(parent) = path.parent() {
        if parent.file_name().map(|n| n.to_string_lossy()) == Some(uid.to_string().into()) {
            chown_chmod(&parent.to_string_lossy());
        }
    }
    chown_chmod(socket_path);
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
    /// For a successful Register, the name claimed, so the connection can drop it
    /// from the registry when it disconnects instead of leaving a stale entry.
    registered: Option<String>,
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

/// Maximum concurrent connections to a single bus. Each open connection holds
/// one permit until it disconnects; long-lived peers (registered services,
/// signal subscribers, a compositor's seat connection) each consume one, so the
/// ceiling is set high enough not to starve a real system while still bounding a
/// local connection flood on the world-connectable Highway.
const MAX_BUS_CONNECTIONS: usize = 1024;

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

    // Lock down the socket. A user Lane is one person's private bus: the socket
    // and its directory are owned by that user, mode 0700, so the kernel itself
    // refuses any other user's connect() before rev sees it. The Highway is the
    // shared system bus: world-connectable, with every request authorized
    // per-method by the policy (see set_socket_permissions).
    match tier {
        Tier::Lane { uid } => set_lane_permissions(socket_path, uid),
        Tier::Highway => set_socket_permissions(socket_path),
    }

    println!("rev: wirebus listening on {}", socket_path);

    let clients: ClientWriters = Arc::new(Mutex::new(HashMap::new()));
    // One registry per bus: the Highway's names and each Lane's names are
    // separate, so lanes cannot see one another's registrations.
    let registry = Arc::new(registry::Registry::new());

    // Bound concurrent connections. The Highway is world-connectable, so without
    // a cap any local process could open connections without limit and exhaust
    // rev's memory and task scheduler. Each connection holds one permit for its
    // whole lifetime (a registered service or signal subscriber is long-lived),
    // so the limit is generous; excess connections are refused and the peer
    // retries. Applies to Lanes too, where it only bounds a single user.
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(MAX_BUS_CONNECTIONS));
    let mut at_capacity = false;

    loop {
        let (stream, _) = listener.accept().await?;
        let permit = match conn_limit.clone().try_acquire_owned() {
            Ok(p) => {
                at_capacity = false;
                p
            }
            Err(_) => {
                // Log once per saturation episode, not once per refused connect,
                // so a flood cannot also flood the log.
                if !at_capacity {
                    eprintln!(
                        "rev: wirebus at connection limit ({}); refusing new connections",
                        MAX_BUS_CONNECTIONS
                    );
                    at_capacity = true;
                }
                continue; // drop `stream`: the connection is closed
            }
        };
        let clients = clients.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            // Hold the permit for the connection's lifetime; released on return.
            let _permit = permit;
            if let Err(e) = handle_client(stream, clients, registry, tier).await {
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
    registry: Arc<registry::Registry>,
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

    // Track devices opened, and names registered, by this client, so a
    // disconnect releases both: devices closed under the session they were
    // opened on, registrations dropped from the bus so they do not linger.
    let mut opened_devices: Vec<(u64, String)> = Vec::new();
    let mut registered_names: Vec<String> = Vec::new();

    let cleanup_registry = registry.clone();
    let cleanup = |name: &Option<String>, devices: &[(u64, String)], names: &[String]| {
        for (session_id, path) in devices {
            let _ = crate::seat::close_device(*session_id, path);
        }
        for n in names {
            let _ = cleanup_registry.unregister(n);
        }
        if !devices.is_empty() || !names.is_empty() {
            println!(
                "rev: client {:?} disconnected, released {} devices, {} names",
                name.as_deref().unwrap_or("unknown"),
                devices.len(),
                names.len()
            );
        }
    };

    loop {
        tokio::select! {
            // Incoming request from client
            result = protocol::recv_message(&mut reader) => {
                let msg = match result {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        if let Some(ref name) = client_name {
                            clients.lock().await.remove(name);
                        }
                        cleanup(&client_name, &opened_devices, &registered_names);
                        return Ok(());
                    }
                    Err(e) => {
                        if let Some(ref name) = client_name {
                            clients.lock().await.remove(name);
                        }
                        cleanup(&client_name, &opened_devices, &registered_names);
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

                let result = handle_message(&msg, &clients, &registry, principal, tier).await;

                // Remember a name this connection registered, to drop on disconnect.
                if let Some(name) = result.registered.clone() {
                    registered_names.push(name);
                }

                // Send the response frame via tokio's async writer.
                protocol::send_message(&mut writer, &result.response).await?;

                // If this was an OpenDevice, hand the fd over via SCM_RIGHTS.
                // SCM_RIGHTS requires sendmsg(); async_io runs it under the
                // reactor's writability, retrying on EAGAIN, so tokio keeps the
                // shared fd's readiness coherent and the next recv_message sees
                // its read edge -- no manual re-arm needed.
                if let Some(fd) = result.pass_fd {
                    let raw_fd = writer.as_ref().as_raw_fd();
                    writer
                        .as_ref()
                        .async_io(tokio::io::Interest::WRITABLE, || {
                            crate::seat::fd_passing::send_fd(raw_fd, fd)
                        })
                        .await?;
                    if let (Some(path), Some(sid)) = (device_path, result.device_session) {
                        opened_devices.push((sid, path));
                    }
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
fn classify(
    body: &MessageBody,
    tier: Tier,
    principal: &Principal,
    registry: &registry::Registry,
    sender: &str,
) -> Option<Operation> {
    let service_scope = match tier {
        Tier::Lane { uid } => Scope::OwnUser(uid),
        Tier::Highway => Scope::SystemOrOtherUser,
    };
    // Whether `principal` owns the registered name `n`. A System principal (uid
    // 0) matches system registrations; a user matches its own.
    let owns = |n: &str| matches!(registry.owner_of(n), Some(o) if Some(o) == principal.uid());
    Some(match body {
        MessageBody::Lookup { .. }
        | MessageBody::ListBus
        | MessageBody::ListServices
        | MessageBody::ListSessions => Operation::Read,

        MessageBody::Subscribe { .. } | MessageBody::Unsubscribe { .. } => {
            Operation::SignalSubscribe
        }
        // A client may emit only as a name it has registered (source is its own
        // sender label, which must resolve to one of its registrations).
        MessageBody::EmitSignal { .. } => Operation::SignalEmit { owns_name: owns(sender) },
        // A name may be claimed on your own lane; on the Highway only the system
        // (root) may register, so user services live on their lanes.
        MessageBody::Register { .. } => Operation::Register {
            owns_namespace: matches!(tier, Tier::Lane { .. })
                || matches!(principal, Principal::System),
        },
        // Unregister requires ownership; a name that is not registered falls
        // through so the handler returns an accurate "not registered" error.
        MessageBody::Unregister { name } => Operation::Unregister {
            owns_name: registry.owner_of(name).is_none() || owns(name),
        },

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
    registry: &registry::Registry,
    principal: Principal,
    tier: Tier,
) -> HandleResult {
    let id = msg.id;

    // The authorization choke point: every classifiable request passes through
    // policy::authorize before it is dispatched. This is the single place that
    // grants or denies; the handler arms below assume they are already
    // authorized and never re-check.
    if let Some(op) = classify(&msg.body, tier, &principal, registry, &msg.sender) {
        match policy::authorize(&principal, tier, &op) {
            Access::Allow => {}
            Access::Deny(reason) => {
                let (response, pass_fd) = err_reply(id, format!("denied: {reason}"));
                return HandleResult { response, pass_fd, device_session: None, registered: None };
            }
            Access::Elevated(purpose) => {
                if let Err(e) = crate::auth::verify_for(principal.uid(), msg.auth_token.as_deref(), purpose) {
                    crate::logger::write_log(
                        "rev",
                        &format!("{op:?} denied for uid {:?}: {e}", principal.uid()),
                    );
                    let (response, pass_fd) = err_reply(id, format!("denied: {e}"));
                    return HandleResult { response, pass_fd, device_session: None, registered: None };
                }
            }
        }
    }

    // Set by the OpenDevice arm to the session a device was opened under, and by
    // the Register arm to a successfully claimed name.
    let mut device_session: Option<u64> = None;
    let mut registered: Option<String> = None;

    let (response, pass_fd) = match &msg.body {
        // ----- Service management -----
        MessageBody::StartService { service } => handle_start_service(id, service),
        MessageBody::StopService { service } => handle_stop_service(id, service),
        MessageBody::ReloadService { service } => handle_reload_service(id, service),
        MessageBody::ListServices => {
            let services = crate::init::services::list_services()
                .iter()
                .map(|(name, info)| (name.clone(), protocol::ServiceSnapshot::from(info)))
                .collect();
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
        } => {
            let owner = principal.uid().unwrap_or(0);
            match registry.register(name.clone(), socket_path.clone(), methods.clone(), owner) {
                Ok(()) => {
                    registered = Some(name.clone());
                    ok_reply(id, "Registered")
                }
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::Unregister { name } => match registry.unregister(name) {
            Ok(()) => ok_reply(id, "Unregistered"),
            Err(e) => err_reply(id, e),
        },
        MessageBody::Lookup { name } => {
            // On a miss on the Highway, try to bus-activate a service that
            // declares it provides this name, then look up once more. Lanes
            // carry only their own user's services and are not activatable.
            if registry.lookup(name).is_none() && matches!(tier, Tier::Highway) {
                let _ = crate::bus::activation::activate(name, registry).await;
            }
            match registry.lookup(name) {
                Some(reg) => reply(
                    id,
                    MessageBody::LookupResult {
                        name: reg.name,
                        socket_path: reg.socket_path,
                        methods: reg.methods,
                    },
                ),
                None => err_reply(id, format!("service '{}' not found on bus", name)),
            }
        }
        MessageBody::ListBus => {
            let services = registry.list();
            reply(id, MessageBody::BusServiceList { services })
        }

        // ----- Signals -----
        MessageBody::Subscribe { service, signal } => {
            match registry.subscribe(&msg.sender, service, signal) {
                Ok(()) => ok_reply(id, format!("Subscribed to {}:{}", service, signal)),
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::Unsubscribe { service, signal } => {
            match registry.unsubscribe(&msg.sender, service, signal) {
                Ok(()) => ok_reply(id, format!("Unsubscribed from {}:{}", service, signal)),
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::EmitSignal { signal, payload } => {
            // The choke point verified the sender owns this source name. Deliver
            // to subscribers of that source.
            let subscribers = registry.get_signal_subscribers(&msg.sender, signal);
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
        } => {
            // Bring the user's Lane bus up before launching the session, so the
            // session command finds it listening at $WIREBUS_SOCKET.
            if let Err(e) = crate::bus::lanes::LANES.start_lane(*uid) {
                eprintln!("rev: could not start lane for uid {}: {}", uid, e);
            }
            match crate::session::start_session(*uid, *gid, username, command, env) {
            Ok(session) => {
                crate::seat::set_active_session(session.session_id);
                // Start the user's scope=user services on their lane. Keyed by
                // the account UUID so vault-installed services are found.
                if let Some(uuid) = uac_core::Uac::open()
                    .ok()
                    .and_then(|u| u.get(username).ok())
                    .map(|a| a.uuid)
                {
                    crate::service::start_user_services(*uid, *gid, &session.lane_socket, &uuid);
                }
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
        MessageBody::EndSession { session_id } => {
            // Capture the owner before ending the session removes the record, so
            // we can tear the user's Lane down afterwards.
            let owner = crate::session::owner_uid(*session_id);
            match crate::session::end_session(*session_id) {
                Ok(()) => {
                    if let Some(uid) = owner {
                        let _ = crate::bus::lanes::LANES.stop_lane(uid);
                    }
                    ok_reply(id, format!("Session {} ended", session_id))
                }
                Err(e) => err_reply(id, e),
            }
        }
        MessageBody::ListSessions => {
            // Enumerating other users' sessions is cross-scope: the system sees
            // all, a user sees only their own. This keeps the Highway being
            // world-connectable from leaking who else is logged in.
            let all = matches!(principal, Principal::System);
            let viewer = principal.uid();
            let sessions = crate::session::list_sessions();
            let entries: Vec<protocol::SessionEntry> = sessions
                .iter()
                .filter(|s| all || Some(s.uid) == viewer)
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

    HandleResult { response, pass_fd, device_session, registered }
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

    // End-to-end: a peer registers a name through the blocking sync client (the
    // path rookd/rexecd use to announce themselves) and a second connection looks
    // it up, exercising the real server, registry, and wire codec. Run on a Lane
    // for this uid because a Lane lets its owner claim a name; Highway
    // registration is root-only and is covered by the policy unit tests.
    #[tokio::test]
    async fn register_and_lookup_roundtrip_over_sync_client() {
        use std::path::PathBuf;
        use std::time::Duration;

        let uid = nix::unistd::getuid().as_raw();
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("bus.sock");
        let sock_str = sock.to_string_lossy().to_string();

        let server_sock = sock_str.clone();
        tokio::spawn(async move {
            let _ = run(&server_sock, Tier::Lane { uid }).await;
        });
        for _ in 0..200 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let target = PathBuf::from("/Transit/Ephemeral/RookGuard/rookd.sock");
        let target_for_blk = target.clone();
        let looked = tokio::task::spawn_blocking(move || {
            // Point the sync helpers at this bus. No other test reads
            // REV_BUS_SOCK, so this process-global override is safe here.
            unsafe {
                std::env::set_var("REV_BUS_SOCK", &sock_str);
            }
            let _held = wirebus_proto::sync::register(
                "rookguard",
                &target_for_blk,
                std::collections::HashMap::new(),
            )
            .expect("register on the bus");
            // The registration is held by `_held`; rev drops a peer's names when
            // its connection closes, so the lookup must happen before the drop.
            let found = wirebus_proto::sync::lookup("rookguard").expect("lookup transport ok");
            drop(_held);
            found
        })
        .await
        .unwrap();

        assert_eq!(looked, Some(target));
    }
}
