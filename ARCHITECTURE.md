# Rev — RunixOS Init System & Service Manager

Rev is the PID 1 process for RunixOS. It replaces systemd, D-Bus, seatd, logind, and sudo with a single unified process that owns process lifecycle, IPC, device arbitration, session management, and privilege escalation.

## Table of Contents

- [Project Structure](#project-structure)
- [Boot Sequence](#boot-sequence)
- [Service Configuration (.rsc files)](#service-configuration-rsc-files)
- [Service Lifecycle](#service-lifecycle)
- [WireBus — IPC Protocol](#wirebus--ipc-protocol)
  - [Wire Format](#wire-format)
  - [Message Envelope](#message-envelope)
  - [Message Types Reference](#message-types-reference)
  - [System Highway](#system-highway)
  - [User Lanes](#user-lanes)
  - [Signal Pub/Sub](#signal-pubsub)
  - [Service Registry](#service-registry)
- [Seat Management — Device Arbitration](#seat-management--device-arbitration)
  - [FD Passing (SCM_RIGHTS)](#fd-passing-scm_rights)
  - [VT Switching](#vt-switching)
- [Session Management](#session-management)
  - [Login Flow](#login-flow)
  - [Session Environment](#session-environment)
- [Privilege Escalation (sudo Replacement)](#privilege-escalation-sudo-replacement)
  - [ExecAs Flow](#execas-flow)
  - [Environment Sanitization](#environment-sanitization)
- [Graceful Shutdown](#graceful-shutdown)
- [PID 1 Boundary — What's In vs Out](#pid-1-boundary--whats-in-vs-out)
- [Filesystem Paths](#filesystem-paths)
- [Future Work / Rook Guard Integration](#future-work--rook-guard-integration)

---

## Project Structure

```
src/
├── main.rs                      Entry point. PID 1 detection (name == "init" || pid == 1).
├── parser/mod.rs                ServiceConfig (TOML), ServiceInfo (MessagePack), service_dirs().
├── logger/mod.rs                Per-service log files with size-based rotation.
├── bus/
│   ├── mod.rs                   WireBus module root, socket_path() helper.
│   ├── protocol.rs              Wire format, Message envelope, all 30+ message types.
│   ├── registry.rs              Service name -> socket path registry + signal subscriptions.
│   ├── server.rs                Unified async server (tokio). Handles everything.
│   └── lanes.rs                 User Lane lifecycle (per-user bus scopes).
├── init/
│   ├── mod.rs                   Boot sequence, overlay mount, scheduler start, graceful shutdown.
│   └── services.rs              In-memory service state (HashMap behind Mutex).
├── service/
│   ├── mod.rs                   fork/execve, zombie reaping (SIGCHLD), restart policies, hooks.
│   └── scheduler.rs             Cron-based periodic service execution.
├── seat/
│   ├── mod.rs                   Device arbitration. Open/close/track /dev/dri/* and /dev/input/*.
│   └── fd_passing.rs            SCM_RIGHTS send_fd/recv_fd over Unix sockets.
├── session/
│   └── mod.rs                   Session spawning (fork/setuid/exec), privilege escalation (ExecAs).
├── cli/
│   ├── mod.rs                   CLI routing. No args = TUI dashboard, otherwise clap subcommands.
│   ├── parse_service.rs         Service name parser (com.vendor.app/service) + tests.
│   └── commands/                start, stop, create, read subcommands.
└── dashboard/
    └── mod.rs                   Ratatui TUI. Service list, detail view, log tail, help.
```

---

## Boot Sequence

When Rev starts as PID 1:

1. **Config overlay mount** (production only) — overlayfs layers `/Construct/Config` over `/Core/Config` so system configs are writable via copy-on-write while `/Core/Config` stays pristine on disk.

2. **Zombie reaper** — spawns a background thread listening for `SIGCHLD`. On child exit, calls `waitpid(-1, WNOHANG)` in a loop. Updates service state and handles restart policies.

3. **Service discovery** — walks service directories for `.rsc` files. For each file found, deserializes the TOML config and calls `start_service_from_path()`.

4. **Cron scheduler** — spawns a tokio task that checks service `schedule` fields every 60 seconds.

5. **WireBus server** — binds the System Highway socket and enters the main accept loop. This is the last step — Rev blocks here until shutdown.

6. **Shutdown signal handler** — `tokio::select!` between the WireBus server and SIGTERM/SIGINT. On signal, initiates graceful shutdown.

---

## Service Configuration (.rsc files)

Service configs are TOML files with the `.rsc` extension (Rev Service Config). Serde uses `kebab-case` field names.

### Full example

```toml
name = "com.rovelstars.files/indexer"
description = "File indexing service for the file manager"
exec-start = "/Core/Bin/indexer --watch /Space"
exec-stop = "/Core/Bin/indexer --stop"
exec-reload = "/Core/Bin/indexer --reload-config"
exec-start-pre = "/Core/Bin/indexer --check-db"
exec-start-post = "/Core/Bin/notify-ready indexer"
exec-stop-pre = "/Core/Bin/indexer --flush"
exec-stop-post = "/Core/Bin/cleanup-indexer-tmp"
restart-policy = "on-failure"
timeout-stop = 30
schedule = "0 */6 * * *"
force-restart-on-schedule = false
working-dir = "/Vault/State/indexer"

[env]
INDEXER_DB = "/Vault/State/indexer/db"
RUST_LOG = "info"
```

### Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | — | Reverse-DNS service identifier (`com.vendor.app/service`) |
| `description` | string | no | — | Human-readable description |
| `exec-start` | string | yes | — | Command to run (shell-word parsed) |
| `exec-stop` | string | no | — | Graceful stop command. Falls back to SIGTERM if unset |
| `exec-reload` | string | no | — | Reload command. Falls back to SIGHUP if unset |
| `exec-start-pre` | string | no | — | Runs before exec-start. Start aborts if this fails |
| `exec-start-post` | string | no | — | Runs after successful fork (in parent) |
| `exec-stop-pre` | string | no | — | Runs before stop |
| `exec-stop-post` | string | no | — | Runs after natural exit (not restart) |
| `env` | table | no | `{}` | Environment variables as key-value pairs |
| `working-dir` | string | no | — | Working directory for the service process |
| `restart-policy` | enum | no | `"never"` | One of: `"always"`, `"on-failure"`, `"never"`, `"on-resource-change"` |
| `timeout-stop` | integer | no | `10` | Seconds to wait after SIGTERM before SIGKILL |
| `schedule` | string | no | — | Cron expression (5-field). Service starts on schedule |
| `force-restart-on-schedule` | bool | no | `false` | If true, restart even if already running on cron tick |

### Service name format

```
<app-id>/<service-name>

app-id:       3+ dot-separated segments (e.g. com.rovelstars.files)
service-name: can contain slashes (e.g. backup/cloud-service-a)
file path:    <app-id>/<service-name>.rsc
```

### Service directories

| Path | Scope |
|------|-------|
| `/Core/Services/` | System services (immutable, SIP-protected) |
| `/Core/UserServices/` | System-wide user service definitions |
| `/Construct/Services/` | Third-party service definitions |
| `/Space/<user>/.Services/` | Per-user services |

Debug mode uses `./Services/` only.

---

## Service Lifecycle

### Start

```
exec-start-pre (if defined, abort on failure)
  └─> fork()
       ├─ Parent: log PID, update state, run exec-start-post
       └─ Child: redirect stdout/stderr to log, set env, chdir, execve()
```

### Stop

```
exec-stop-pre (if defined)
  └─> exec-stop (if defined, falls back to SIGTERM)
       └─> wait up to timeout-stop seconds
            └─> SIGKILL if still alive
```

### Restart policies

When a service exits, the zombie reaper checks the restart policy:

| Policy | Behavior |
|--------|----------|
| `always` | Restart regardless of exit code |
| `on-failure` | Restart only if exit code != 0 or killed by signal |
| `never` | Don't restart. Run `exec-stop-post` if defined |
| `on-resource-change` | (Planned) Restart when cgroup limits change |

Restarts have a 500ms debounce to prevent tight loops. The `restart_count` field in `ServiceInfo` is incremented on each restart.

### Cron scheduling

If `schedule` is set, a background task checks every 60 seconds whether the cron expression matches. Behavior:

- **Service not running**: start it
- **Service running + `force-restart-on-schedule = false`**: leave it alone
- **Service running + `force-restart-on-schedule = true`**: stop and restart

---

## WireBus — IPC Protocol

WireBus is RunixOS's D-Bus replacement. It's a registry + peer-to-peer model built into Rev.

### Wire Format

All communication uses **length-prefixed MessagePack** frames:

```
┌──────────────────────┬─────────────────────────────┐
│  4 bytes (BE uint32) │  MessagePack payload         │
│  payload length      │  (Message struct)            │
└──────────────────────┴─────────────────────────────┘
```

- Maximum frame size: 16 MB
- Transport: Unix domain sockets (SOCK_STREAM)
- Serialization: MessagePack via `rmp-serde` using **named/map format** (`to_vec_named`)
- Language-agnostic: any language with Unix sockets + MessagePack can participate (Rust, C, Python, Go, etc.)

**Important**: MessagePack payloads MUST use map-based serialization (field names as keys), NOT array-based. This is because `MessageBody` uses `#[serde(tag = "type")]` internally-tagged representation, which embeds a `"type"` key in the map to identify the variant. Array-based serialization (e.g. `rmp_serde::to_vec()`) has no map keys and will fail to deserialize. In Rust, always use `rmp_serde::to_vec_named()`. In other languages, serialize MessagePack as a map/dict, not an array.

### Message Envelope

Every frame is a `Message`:

```
Message {
    id:         u64      // Unique ID for request/response correlation
    sender:     String   // Who sent this ("rev", "rev-cli", service name, etc.)
    auth_token: String?  // Optional Rook Guard authentication token
    body:       MessageBody  // The actual request/response/signal
}
```

Responses echo back the request's `id` for correlation. The `auth_token` field is checked for privileged operations (ExecAs, StartSession, Register on Highway) once Rook Guard is implemented.

### Message Types Reference

#### Service Management

| Type | Direction | Fields | Response |
|------|-----------|--------|----------|
| `start-service` | client -> rev | `service: String` | `ok` |
| `stop-service` | client -> rev | `service: String` | `ok` |
| `reload-service` | client -> rev | `service: String` | `ok` |
| `list-services` | client -> rev | — | `service-list { services }` |
| `rescan` | client -> rev | — | `ok` |

#### Bus Registry

| Type | Direction | Fields | Response |
|------|-----------|--------|----------|
| `register` | client -> rev | `name, socket_path, methods` | `ok` |
| `unregister` | client -> rev | `name` | `ok` |
| `lookup` | client -> rev | `name` | `lookup-result { name, socket_path, methods }` |
| `list-bus` | client -> rev | — | `bus-service-list { services }` |

#### Signals

| Type | Direction | Fields | Response |
|------|-----------|--------|----------|
| `subscribe` | client -> rev | `service, signal` | `ok` |
| `unsubscribe` | client -> rev | `service, signal` | `ok` |
| `emit-signal` | client -> rev | `signal, payload` | `ok` (with delivery count) |
| `signal-delivery` | rev -> client | `source, signal, payload` | — (one-way) |

#### Seat (Device Arbitration)

| Type | Direction | Fields | Response |
|------|-----------|--------|----------|
| `open-device` | client -> rev | `path` | `ok` + FD via SCM_RIGHTS |
| `close-device` | client -> rev | `path` | `ok` |
| `pause-device` | rev -> client | `path` | — (notification) |
| `resume-device` | rev -> client | `path` | — (notification) |

#### Privilege Escalation

| Type | Direction | Fields | Response |
|------|-----------|--------|----------|
| `exec-as` | client -> rev | `uid, command, env, working_dir` | `exec-as-result { pid }` |

#### Session Management

| Type | Direction | Fields | Response |
|------|-----------|--------|----------|
| `start-session` | client -> rev | `uid, gid, username, command, env` | `session-started { session_id, pid, lane_socket }` |
| `end-session` | client -> rev | `session_id` | `ok` |
| `list-sessions` | client -> rev | — | `session-list { sessions }` |

#### Generic Responses

| Type | Fields |
|------|--------|
| `ok` | `message: String` |
| `error` | `message: String` |

### System Highway

The System Highway is the root-level bus. It's always running as long as Rev is alive.

- **Socket**: `/Transit/Ephemeral/rev/bus.sock` (debug: `./rev.sock`)
- **Scope**: System services from `/Core/Services/`, `/Core/UserServices/`, `/Construct/Services/`
- **Access**: All processes can connect. Privileged operations (ExecAs, StartSession, Register) will require `auth_token` verification via Rook Guard

The server uses `tokio::select!` per client to multiplex:
- Incoming requests from the client socket
- Outgoing signal deliveries via an `mpsc` channel

Clients are tracked by their `sender` name. When a signal is emitted, Rev looks up all subscribers and pushes `SignalDelivery` messages to their channels.

### User Lanes

Each user session gets its own isolated WireBus scope:

- **Socket**: `/Transit/Ephemeral/rev/user/<uid>/bus.sock` (debug: `./rev-user-<uid>.sock`)
- **Service directory**: `/Space/<username>/.Services/`
- **Lifecycle**: started on user login, torn down on logout
- **Isolation**: services on one User Lane cannot see services on other User Lanes

A user service can request opt-in access to the System Highway. This requires Rook Guard authorization (not yet implemented).

The `LaneManager` tracks active lanes and provides:
- `start_lane(uid)` — spawns a WireBus server on the user's lane socket
- `stop_lane(uid)` — sends shutdown signal, removes socket
- `is_active(uid)` — check if a lane exists
- `list_lanes()` — enumerate all active lanes

### Signal Pub/Sub

Signals are one-way fan-out messages from a service to all subscribers.

**Subscribe**: `Subscribe { service: "com.rovelstars.files/indexer", signal: "index-complete" }`
- Use `signal: "*"` to subscribe to all signals from a service
- Subscriptions are tracked per-client in the registry

**Emit**: `EmitSignal { signal: "index-complete", payload: <bytes> }`
- Only the registered owner of a service should emit its signals
- `payload` is opaque bytes — emitter and subscribers agree on format

**Delivery**: Rev pushes `SignalDelivery { source, signal, payload }` to all matching subscribers that are currently connected. If a subscriber isn't connected, the signal is dropped (no queuing).

**Cleanup**: when a service unregisters, all subscriptions to its signals are removed. When a subscriber disconnects, its subscriptions are cleaned up.

### Service Registry

The registry is the "phone book" that enables peer-to-peer IPC. Rev is not in the data path — it only brokers introductions.

**Flow**:
```
1. Service B starts, creates its own Unix socket listener
2. B sends: Register { name: "com.rovelstars.files/indexer",
                        socket_path: "/Transit/Ephemeral/rev/services/indexer.sock",
                        methods: { "search": "Full-text search", "reindex": "Force reindex" } }
3. Rev stores the registration

4. Service A wants to talk to B
5. A sends: Lookup { name: "com.rovelstars.files/indexer" }
6. Rev responds: LookupResult { name, socket_path, methods }
7. A connects directly to B's socket_path — Rev is NOT in the data path
```

---

## Seat Management — Device Arbitration

Rev replaces `seatd`/`logind` for device access. Since Rev runs as root (PID 1), it can open restricted device nodes and pass file descriptors to unprivileged compositors.

### Allowed devices

Only paths under these prefixes are permitted (validated via `canonicalize()` to prevent symlink attacks):

- `/dev/dri/*` — GPU render/display nodes
- `/dev/input/*` — keyboards, mice, touchpads, etc.

### FD Passing (SCM_RIGHTS)

Unix sockets support passing file descriptors between processes via `SCM_RIGHTS` ancillary data. This is the same mechanism used by Wayland compositors with `seatd`.

**Flow**:
```
1. Compositor sends:  OpenDevice { path: "/dev/dri/card0" }
2. Rev opens the device with O_RDWR | O_CLOEXEC
3. Rev sends Ok response via WireBus frame
4. Rev sends the FD via SCM_RIGHTS (sendmsg with cmsg)
5. Compositor calls recv_fd() to receive the raw file descriptor
6. Compositor can now use the device without root privileges
```

**API** (in `seat/fd_passing.rs`, uses `nix::sys::socket` safe wrappers):
- `send_fd(socket_fd, fd)` — send a single FD via SCM_RIGHTS
- `send_fds(socket_fd, &[fd1, fd2])` — send multiple FDs in one message
- `recv_fd(socket_fd)` — receive a single FD
- `recv_fds(socket_fd)` — receive up to 8 FDs, returns `Vec<RawFd>`
- `send_fd_over_stream(stream, fd)` / `recv_fd_from_stream(stream)` — wrappers accepting any `AsRawFd` type

Built on `nix::sys::socket::{sendmsg, recvmsg, ControlMessage, ControlMessageOwned}` — no raw libc CMSG macros. Language-agnostic — any language with `cmsg` support can interop.

### VT Switching

When the active session changes (planned):
1. Rev sends `PauseDevice { path }` to the old session's compositor for each open device
2. Old compositor should release devices and stop rendering
3. Rev sends `ResumeDevice { path }` to the new session's compositor
4. New compositor re-acquires devices and resumes

Device tracking is per-session. When a session ends, all its devices are closed automatically.

---

## Session Management

Rev handles the full session lifecycle: login, session creation, and teardown.

### Login Flow

```
┌─────────┐    auth request     ┌────────────┐   verify    ┌─────────────┐
│ Greeter │ ──────────────────> │ Rook Guard │ ──────────> │ Auth Daemon │
│ (UI)    │ <────────────────── │ (WireBus)  │ <────────── │ (crypto)    │
└─────────┘    auth success     └────────────┘   result    └─────────────┘
     │
     │  StartSession { uid, gid, username, command, env }
     v
┌─────────┐
│   Rev   │ ── fork() ──> child: setgid() -> setuid() -> setup env -> execv(compositor)
│ (PID 1) │ ── parent: track session, set active seat, return SessionStarted
└─────────┘
```

The greeter is an unprivileged process. It authenticates via Rook Guard (which delegates crypto to a sandboxed auth daemon), then tells Rev to start the session. Rev does the actual privilege management.

### Session Environment

When Rev spawns a session, the child process gets:

| Variable | Value |
|----------|-------|
| `HOME` | `/Space/<username>` |
| `USER` | `<username>` |
| `LOGNAME` | `<username>` |
| `SHELL` | `/Core/Bin/nushell` |
| `PATH` | `/Core/Bin:/Construct/Bin` |
| `XDG_RUNTIME_DIR` | `/Transit/Ephemeral/user/<uid>` |
| `WIREBUS_SOCKET` | Path to User Lane socket |

Rev also:
- Creates `XDG_RUNTIME_DIR` with mode `0700`, owned by the user
- Sets `gid` before `uid` (must drop group first while still root)
- Changes to the user's home directory

### Session Teardown

When a session ends (via `EndSession` or process exit):
1. SIGTERM sent to session process
2. All seat devices for the session are closed
3. Session removed from tracking
4. (Planned) User Lane shut down

The zombie reaper calls `handle_session_exit(pid)` so sessions are cleaned up even if the process exits unexpectedly.

---

## Privilege Escalation (sudo Replacement)

Rev eliminates the need for setuid binaries. Instead of `sudo`, processes send an `ExecAs` message via WireBus.

### ExecAs Flow

```
┌──────────┐  ExecAs { uid: 0, command: ["pacman", "-Syu"], auth_token: "..." }
│ User App │ ──────────────────────────────────────────────────────────────────>
└──────────┘                                                                    │
                                                                                v
                                                              ┌─────────┐
                                                              │   Rev   │
                                                              │ (PID 1) │
                                                              └────┬────┘
                                                                   │
                                        1. Verify auth_token with Rook Guard
                                        2. Validate command path exists
                                        3. fork()
                                           ├── child: setuid(0), sanitize env, execv()
                                           └── parent: return ExecAsResult { pid }
```

### Environment Sanitization

When executing as root (uid=0), Rev:
- Sets `PATH` to `/Core/Bin:/Construct/Bin`
- Sets `HOME` to `/Space/root`
- Removes `LD_PRELOAD` and `LD_LIBRARY_PATH`
- **Silently drops** any caller-provided env var starting with `LD_` (prevents library injection)

When executing as a non-root user, Rev:
- Calls `setgid(gid)` then `setuid(uid)` to drop privileges
- Passes through caller-provided environment

---

## Graceful Shutdown

On SIGTERM or SIGINT:

1. End all active sessions (SIGTERM to session processes, close seat devices)
2. Stop all services in reverse registration order (last started = first stopped)
3. Each service stop follows the full stop sequence (exec-stop-pre, exec-stop/SIGTERM, timeout, SIGKILL)
4. Remove the WireBus socket file
5. Exit

---

## PID 1 Boundary — What's In vs Out

### Inside Rev (PID 1)

These need root privileges AND process lifecycle control:

| Feature | Replaces | Why PID 1 |
|---------|----------|-----------|
| Service management | systemd | Process lifecycle, zombie reaping |
| WireBus IPC | D-Bus | Central registry, always available |
| Device FD passing | seatd/logind | Needs root to open /dev/*, avoids setuid |
| Privilege escalation | sudo | Needs root to setuid, avoids setuid binaries |
| Session spawning | logind | Needs root for setuid, manages process lifecycle |

### Outside Rev (separate daemons)

| Component | Why separate |
|-----------|-------------|
| Cryptographic password hashing | A panic in Argon2/bcrypt must not crash PID 1 |
| Device enumeration (udev) | Parsing uevent netlink is complex, bugs must not crash PID 1 |
| Greeter / Login UI | Font parsing, pixel drawing — must be unprivileged and isolated |
| Rook Guard auth daemon | Complex security logic, separate failure domain |

**Rule of thumb**: does it need root privileges AND process control? If yes, it belongs in Rev. If it just needs root, make it a separate daemon. If neither, definitely separate.

---

## Filesystem Paths

| Path | Purpose |
|------|---------|
| `/Transit/Ephemeral/rev/bus.sock` | WireBus System Highway socket |
| `/Transit/Ephemeral/rev/user/<uid>/bus.sock` | User Lane socket |
| `/Transit/Ephemeral/user/<uid>/` | XDG_RUNTIME_DIR per user |
| `/Vault/Chronicle/rev/` | Rev log files |
| `/Vault/Chronicle/rev/<service>.log` | Per-service log files |
| `/Core/Services/` | System service definitions (immutable) |
| `/Core/UserServices/` | System-wide user service definitions |
| `/Construct/Services/` | Third-party service definitions |
| `/Space/<user>/.Services/` | Per-user service definitions |
| `/Core/Config/` | System config (overlayfs lower) |
| `/Construct/Config/` | Writable config overlay (overlayfs upper) |

Debug mode equivalents: `./rev.sock`, `./logs/`, `./Services/`, etc.

---

## Future Work / Rook Guard Integration

The following features are structurally complete in Rev but require Rook Guard (separate project) to be fully functional:

1. **`auth_token` verification** — every `Message` has an `auth_token` field. Currently Rev logs warnings when privileged operations (ExecAs, StartSession, Register) arrive without tokens. Once Rook Guard is running on the System Highway, Rev will call it to verify tokens before executing.

2. **User Lane cross-highway access** — a user service can request access to the System Highway. The protocol supports this, but the authorization check needs Rook Guard.

3. **Lockdown mode** — when enabled, only signed/official services can register on the System Highway. Requires Rook Guard's signature verification.

4. **`on-resource-change` restart policy** — restart when cgroup resource limits change. Requires cgroup monitoring integration.

5. **Service dependency graph** — DAG-based startup ordering, deadlock detection. The boot sequence currently starts services in filesystem walk order.
