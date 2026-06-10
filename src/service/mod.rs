pub mod scheduler;

use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use signal_hook::consts::signal::SIGCHLD;
use signal_hook::iterator::Signals;
use std::thread;

use crate::init::services;
use crate::parser::{RestartPolicy, ServiceConfig, ServiceInfo};

/// Spawn a background thread that reaps zombie processes and handles
/// restart policies when services exit.
pub fn reap_zombies_loop() {
    let mut signals = Signals::new(&[SIGCHLD]).expect("failed to register SIGCHLD handler");

    thread::spawn(move || {
        for _ in signals.forever() {
            loop {
                match waitpid(nix::unistd::Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => break,
                    Ok(WaitStatus::Exited(pid, status)) => {
                        println!("rev: child {} exited with status {}", pid, status);
                        handle_exit(pid.as_raw() as u32, Some(status));
                    }
                    Ok(WaitStatus::Signaled(pid, signal, _core_dumped)) => {
                        println!("rev: child {} killed by signal {:?}", pid, signal);
                        handle_exit(pid.as_raw() as u32, None);
                    }
                    Ok(_) => {}
                    Err(nix::errno::Errno::ECHILD) => break,
                    Err(e) => {
                        eprintln!("rev: waitpid error: {}", e);
                        break;
                    }
                }
            }
        }
    });
}

/// Called when a child process exits. Updates service state and
/// handles restart policy.
fn handle_exit(pid: u32, exit_code: Option<i32>) {
    // Check if this was a session process
    crate::session::handle_session_exit(pid);

    // Get the service info before updating (need it for restart decision)
    let service_info = services::get_service_by_pid(pid);

    // Update the service status to stopped
    services::mark_service_exited(pid, exit_code);

    // Handle restart policy
    if let Some(info) = service_info {
        let should_restart = match info.config.restart_policy {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => exit_code.is_none_or(|c| c != 0),
            RestartPolicy::Never => false,
            RestartPolicy::OnResourceChange => false, // TODO: cgroup monitoring
        };

        if should_restart {
            if let Some(ref config_path) = info.config_path {
                let path = std::path::PathBuf::from(config_path);
                println!(
                    "rev: restarting {} (policy: {:?})",
                    info.name, info.config.restart_policy
                );
                crate::logger::write_log(
                    &info.name,
                    &format!(
                        "Service exited (code: {:?}), restarting per {:?} policy",
                        exit_code, info.config.restart_policy
                    ),
                );
                // Small delay to avoid tight restart loops
                thread::sleep(std::time::Duration::from_millis(500));
                // Deregister so start_service_from_path can re-register
                services::deregister_service(&info.name);
                start_service_from_path(&path);
                // Increment restart count
                services::increment_restart_count(&info.name);
            }
        } else {
            // Run exec-stop-post hook if defined
            if let Some(ref hook) = info.config.exec_stop_post {
                run_hook(hook, &info.config);
            }
        }
    }
}

/// Run a shell command as a hook (exec-start-pre, exec-start-post, etc.)
/// Blocks until the command finishes. Returns true on success.
pub fn run_hook(command: &str, config: &ServiceConfig) -> bool {
    let args = match shell_words::split(command) {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => {
            eprintln!("rev: empty hook command");
            return false;
        }
        Err(e) => {
            eprintln!("rev: failed to parse hook command '{}': {}", command, e);
            return false;
        }
    };

    let mut cmd = std::process::Command::new(&args[0]);
    cmd.args(&args[1..]);

    // Inherit environment
    for (key, value) in &config.env {
        cmd.env(key, value);
    }
    if let Some(ref dir) = config.working_dir {
        cmd.current_dir(dir);
    }

    match cmd.status() {
        Ok(status) => {
            if !status.success() {
                eprintln!(
                    "rev: hook '{}' failed with exit code {:?}",
                    command,
                    status.code()
                );
            }
            status.success()
        }
        Err(e) => {
            eprintln!("rev: failed to run hook '{}': {}", command, e);
            false
        }
    }
}

/// Stop a running service. Uses exec-stop if defined, otherwise SIGTERM
/// with timeout fallback to SIGKILL.
pub fn stop_service(info: &ServiceInfo) {
    let pid = match info.pid {
        Some(p) => p,
        None => return,
    };

    crate::logger::write_log(&info.name, "Stopping service");

    // Run exec-stop-pre hook if defined
    if let Some(ref hook) = info.config.exec_stop_pre {
        run_hook(hook, &info.config);
    }

    if let Some(ref exec_stop) = info.config.exec_stop {
        // Use the defined stop command
        if !run_hook(exec_stop, &info.config) {
            // Stop command failed — fall back to SIGTERM
            eprintln!(
                "rev: exec-stop failed for {}, falling back to SIGTERM",
                info.name
            );
            send_signal_with_timeout(pid, info.config.timeout_stop.unwrap_or(10));
        }
    } else {
        // No exec-stop — use SIGTERM + timeout
        send_signal_with_timeout(pid, info.config.timeout_stop.unwrap_or(10));
    }
}

/// Send SIGTERM, wait up to `timeout_secs`, then SIGKILL if still alive.
fn send_signal_with_timeout(pid: u32, timeout_secs: u64) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    // Poll whether the process is still alive
    while std::time::Instant::now() < deadline {
        // Check if process still exists (kill with signal 0)
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(100));
    }

    // Timeout — force kill
    eprintln!(
        "rev: service PID {} did not stop within {}s, sending SIGKILL",
        pid, timeout_secs
    );
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Start a service from its .rsc config file path.
pub fn start_service_from_path(path: &std::path::Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rev: failed to read {}: {}", path.display(), e);
            return;
        }
    };
    let config = match crate::parser::deserialize_service_config(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rev: failed to parse {}: {}", path.display(), e);
            return;
        }
    };

    let name = config.name.clone();

    if services::get_service(&name).is_some() {
        eprintln!("rev: service {} is already registered", name);
        return;
    }

    services::register_service(
        name.clone(),
        ServiceInfo {
            name: config.name.clone(),
            config_path: Some(path.display().to_string()),
            config: config.clone(),
            ..Default::default()
        },
    );

    // Undo the registration if the service cannot actually be started, so a
    // failed start does not leave a phantom registered-but-dead entry.
    if !spawn_running(&config) {
        services::deregister_service(&name);
    }
}

/// Start a service rev already knows (loaded from a .rsc file) but that is not
/// running, returning whether it is now running. Used by bus-activation, where
/// a Lookup for a name a service `provides` arrives while that service is idle.
/// Unlike `start_service_from_path`, the service must already be registered (it
/// is left registered on failure, since rev knows it independently of the bus).
pub fn start_known_service(name: &str) -> bool {
    let info = match services::get_service(name) {
        Some(i) => i,
        None => return false,
    };
    if info.is_running {
        return true;
    }
    spawn_running(&info.config);
    services::get_service(name).map(|i| i.is_running).unwrap_or(false)
}

/// Start a user's `scope=user` services on their Lane, as the user.
///
/// Gathers OOTB/system-installed user-scope defaults from the system service
/// dirs plus the account's vault-installed services, dependency-orders them, and
/// forks each dropped to (uid, gid) with `WIREBUS_SOCKET` pointed at the lane.
/// Each PID is recorded with the lane so logout tears it down. These run
/// detached from the system service table (names are per-user, so they would
/// collide), so they are reaped but not auto-restarted for now.
pub fn start_user_services(
    uid: u32,
    gid: u32,
    lane_socket: &std::path::Path,
    account_uuid: &str,
) {
    let mut candidates: Vec<(String, ServiceConfig, std::path::PathBuf)> = Vec::new();
    for dir in crate::parser::service_dirs() {
        collect_user_scope(&dir, &mut candidates);
    }
    collect_user_scope(
        &crate::bus::lanes::user_service_dir(account_uuid),
        &mut candidates,
    );
    if candidates.is_empty() {
        return;
    }

    let sortable: Vec<(String, ServiceConfig)> = candidates
        .iter()
        .map(|(name, config, _)| (name.clone(), config.clone()))
        .collect();
    let (order, _forced) = crate::init::ordering::start_order(&sortable);
    for idx in order {
        let (name, config, _) = &candidates[idx];
        if let Some(pid) = spawn_lane_service(name, config, uid, gid, lane_socket) {
            crate::bus::lanes::LANES.record_service(uid, pid);
        }
    }
}

/// Collect parseable `scope=user` services from `dir` into `out`.
fn collect_user_scope(
    dir: &std::path::Path,
    out: &mut Vec<(String, ServiceConfig, std::path::PathBuf)>,
) {
    if !dir.exists() {
        return;
    }
    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().and_then(|s| s.to_str()) == Some("rsc")
            && let Some(config) = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| crate::parser::deserialize_service_config(&t).ok())
            && config.scope == crate::parser::ServiceScope::User
        {
            out.push((config.name.clone(), config, path.to_path_buf()));
        }
    }
}

/// Fork/exec one scope=user service as the lane's user. Returns the child PID,
/// or None on failure. The service's identity is the logged-in user (the lane
/// owner), never the config's `user=` field, so a user-scope service cannot ask
/// to run as someone else.
fn spawn_lane_service(
    name: &str,
    config: &ServiceConfig,
    uid: u32,
    gid: u32,
    lane_socket: &std::path::Path,
) -> Option<u32> {
    let args = match shell_words::split(&config.exec_start) {
        Ok(a) if !a.is_empty() => a,
        _ => {
            eprintln!("rev: user service {}: invalid exec-start", name);
            return None;
        }
    };

    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
            println!("rev: started user service {} (PID {}, uid {})", name, child, uid);
            Some(child.as_raw() as u32)
        }
        #[allow(unreachable_code)]
        Ok(nix::unistd::ForkResult::Child) => {
            unsafe {
                let grp = [gid as libc::gid_t];
                if libc::setgroups(1, grp.as_ptr()) != 0
                    || libc::setgid(gid as libc::gid_t) != 0
                    || libc::setuid(uid as libc::uid_t) != 0
                {
                    eprintln!("rev: user service {}: failed to drop to {}:{}", name, uid, gid);
                    std::process::exit(1);
                }
                // Minimal environment: point the service at its lane bus, then
                // layer the service's own env on top.
                std::env::set_var("WIREBUS_SOCKET", lane_socket);
                std::env::set_var("PATH", "/Core/Bin:/Construct/Bin");
                for (key, value) in &config.env {
                    std::env::set_var(key, value);
                }
            }
            if let Some(ref dir) = config.working_dir {
                let _ = nix::unistd::chdir(dir.as_path());
            }
            use std::ffi::CString;
            let cstr: Vec<CString> = args
                .iter()
                .map(|a| CString::new(a.clone()).expect("invalid argument"))
                .collect();
            let refs: Vec<&std::ffi::CStr> = cstr.iter().map(|s| s.as_c_str()).collect();
            nix::unistd::execv(&cstr[0], &refs).expect("execv failed");
            unreachable!()
        }
        Err(e) => {
            eprintln!("rev: user service {}: fork failed: {}", name, e);
            None
        }
    }
}

/// Resolve the (uid, gid) a service should run as from its `user`/`group`
/// fields. `user` is a numeric uid or a UAC account name; `group` overrides the
/// gid with a numeric value. Returns None when the user cannot be resolved, so
/// the caller can refuse to start it rather than fall back to root.
fn resolve_run_as(user: &str, group: Option<&str>) -> Option<(u32, u32)> {
    let (uid, mut gid) = match user.parse::<u32>() {
        Ok(n) => (n, n),
        Err(_) => match uac_core::Uac::open().and_then(|u| u.get(user)) {
            Ok(acct) => (acct.uid, acct.gid),
            Err(e) => {
                eprintln!("rev: unknown user '{}': {}", user, e);
                return None;
            }
        },
    };
    if let Some(g) = group {
        match g.parse::<u32>() {
            Ok(n) => gid = n,
            Err(_) => eprintln!("rev: group must be a numeric gid, got '{}'; using {}", g, gid),
        }
    }
    Some((uid, gid))
}

/// Fork/exec a service's process from its config, running its start hooks and
/// recording the child PID. Assumes the service is already registered. Returns
/// whether the process was launched (false if a pre-hook or the fork failed).
fn spawn_running(config: &ServiceConfig) -> bool {
    let name = config.name.clone();

    // Resolve the user/group to run as before forking, so UAC is read from the
    // parent. A service that names a user we cannot resolve is refused rather
    // than run with rev's own (root) privileges.
    let run_as = match config.user.as_deref() {
        None => None,
        Some(u) => match resolve_run_as(u, config.group.as_deref()) {
            Some(ids) => Some(ids),
            None => {
                eprintln!(
                    "rev: service {}: cannot resolve user '{}', refusing to start as root",
                    name, u
                );
                return false;
            }
        },
    };

    // Run exec-start-pre hook
    if let Some(ref hook) = config.exec_start_pre {
        if !run_hook(hook, config) {
            eprintln!("rev: exec-start-pre failed for {}, aborting start", name);
            return false;
        }
    }

    println!("rev: starting service {}", name);

    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
            println!("rev: {} started (PID {})", name, child);
            crate::logger::write_log(&name, &format!("Service started (PID {})", child));
            services::update_service_pid(Some(&name), Some(child.as_raw() as i32), None);

            // Run exec-start-post hook
            if let Some(ref hook) = config.exec_start_post {
                run_hook(hook, config);
            }
            true
        }
        #[allow(unreachable_code)]
        Ok(nix::unistd::ForkResult::Child) => {
            // Redirect stdout/stderr to log file
            use std::os::unix::io::AsRawFd;
            match crate::logger::open_log_fds(&config.name) {
                Ok((stdout_file, stderr_file)) => unsafe {
                    libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO);
                    libc::dup2(stderr_file.as_raw_fd(), libc::STDERR_FILENO);
                },
                Err(e) => {
                    eprintln!("rev: failed to open log for {}: {}", config.name, e);
                }
            }

            // Set environment variables
            for (key, value) in &config.env {
                unsafe {
                    std::env::set_var(key, value);
                }
            }

            // Change working directory if specified
            if let Some(ref dir) = config.working_dir {
                if let Err(e) = nix::unistd::chdir(dir.as_path()) {
                    eprintln!("rev: failed to chdir to {}: {}", dir.display(), e);
                    std::process::exit(1);
                }
            }

            // Drop privileges to the configured user/group before exec, in the
            // correct order: supplementary groups, then gid, then uid (once the
            // uid is dropped we can no longer change groups).
            if let Some((uid, gid)) = run_as {
                unsafe {
                    let grp = [gid as libc::gid_t];
                    if libc::setgroups(1, grp.as_ptr()) != 0
                        || libc::setgid(gid as libc::gid_t) != 0
                        || libc::setuid(uid as libc::uid_t) != 0
                    {
                        eprintln!(
                            "rev: failed to drop privileges to {}:{} for {}",
                            uid, gid, config.name
                        );
                        std::process::exit(1);
                    }
                }
            }

            // Parse and execute the command
            use std::ffi::CString;

            let args = match shell_words::split(&config.exec_start) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("rev: failed to parse exec-start: {}", e);
                    std::process::exit(1);
                }
            };

            if args.is_empty() {
                eprintln!("rev: exec-start is empty");
                std::process::exit(1);
            }

            let exec_path = CString::new(args[0].clone()).expect("invalid executable path");
            let args_cstr: Vec<CString> = args
                .iter()
                .map(|arg| CString::new(arg.clone()).expect("invalid argument"))
                .collect();
            let args_ref: Vec<&std::ffi::CStr> = args_cstr.iter().map(|s| s.as_c_str()).collect();

            nix::unistd::execv(&exec_path, &args_ref).expect("execv failed");
            unreachable!()
        }
        Err(e) => {
            eprintln!("rev: fork failed: {}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_user_scope, resolve_run_as};
    use crate::parser::{ServiceConfig, ServiceScope};

    #[test]
    fn collect_user_scope_takes_only_user_services() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, scope: ServiceScope| {
            let cfg = ServiceConfig {
                name: name.to_string(),
                exec_start: "/bin/true".to_string(),
                scope,
                ..Default::default()
            };
            let toml = crate::parser::serialize_service_config(&cfg).unwrap();
            std::fs::write(dir.path().join(format!("{name}.rsc")), toml).unwrap();
        };
        write("a-user", ServiceScope::User);
        write("b-system", ServiceScope::System);

        let mut out = Vec::new();
        collect_user_scope(dir.path(), &mut out);
        let names: Vec<&str> = out.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, ["a-user"]);
    }

    #[test]
    fn numeric_user_resolves_without_uac() {
        // A numeric uid maps uid==gid by default (user-private group).
        assert_eq!(resolve_run_as("1000", None), Some((1000, 1000)));
        // A numeric group overrides the gid.
        assert_eq!(resolve_run_as("1000", Some("2000")), Some((1000, 2000)));
        // A non-numeric group is ignored (kept as the user's gid), not fatal.
        assert_eq!(resolve_run_as("1000", Some("staff")), Some((1000, 1000)));
    }
}
