pub mod console;
pub mod mounts;
pub mod services;

/// Mount the config overlay before anything else.
fn mount_config_overlay() {
    let lower = "/Core/Config";
    let upper = "/Construct/Config";
    let work = "/Construct/.config-work";

    for dir in &[upper, work] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("rev: failed to create {}: {}", dir, e);
            return;
        }
    }

    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        if mounts.lines().any(|l| l.contains(lower) && l.contains("overlay")) {
            return;
        }
    }

    let opts = format!("lowerdir={},upperdir={},workdir={}", lower, upper, work);
    // Mount via the syscall directly - there is no `mount` binary in a minimal
    // RunixOS image.
    match nix::mount::mount(
        Some("overlay"),
        lower,
        Some("overlay"),
        nix::mount::MsFlags::empty(),
        Some(opts.as_str()),
    ) {
        Ok(()) => println!("rev: config overlay mounted (lower={}, upper={})", lower, upper),
        Err(e) => eprintln!("rev: config overlay mount failed: {}", e),
    }
}

/// Graceful shutdown — stop all services in reverse registration order,
/// then clean up.
async fn shutdown() {
    println!("rev: initiating graceful shutdown");
    crate::logger::write_log("rev", "Graceful shutdown initiated");

    // End all active sessions first
    let sessions = crate::session::list_sessions();
    for session in &sessions {
        println!("rev: ending session {} ({})", session.session_id, session.username);
        let _ = crate::session::end_session(session.session_id);
    }

    // Stop services in reverse order (last started = first stopped)
    let all_services = services::list_services();
    for (_name, info) in all_services.iter().rev() {
        if info.is_running {
            println!("rev: stopping {}", info.name);
            crate::service::stop_service(info);
        }
    }

    // Clean up the bus socket
    let socket = crate::bus::socket_path();
    let _ = std::fs::remove_file(&socket);

    println!("rev: shutdown complete");
}

pub async fn run(auto_start: bool) {
    // Production (real PID 1): mount the kernel pseudo-filesystems first, then
    // the config overlay. Skipped in debug so a dev run never touches host mounts.
    if !cfg!(debug_assertions) {
        mounts::early_mounts();
        mount_config_overlay();
    }

    crate::service::reap_zombies_loop();

    let directories = crate::parser::service_dirs();

    if auto_start {
        for dir in &directories {
            if !dir.exists() {
                let _ = std::fs::create_dir_all(dir);
            }
            if !dir.exists() {
                println!("rev: directory {} does not exist, skipping", dir.display());
                continue;
            }
            println!("rev: scanning {}", dir.display());
            for entry in walkdir::WalkDir::new(dir) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("rev: walk error: {}", e);
                        continue;
                    }
                };
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rsc") {
                    let service_name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");
                    println!("rev: found service {} at {}", service_name, path.display());
                    crate::service::start_service_from_path(path);
                }
            }
        }
    }

    // Start the cron scheduler
    crate::service::scheduler::start_scheduler();

    // Bring up an interactive console (dev login; real per-user login lands with
    // the greeter + Rook Guard flow). Skipped in debug.
    if !cfg!(debug_assertions) {
        console::spawn_console();
    }

    // Start the WireBus server (System Highway).
    // Run it with graceful shutdown support via SIGTERM/SIGINT.
    let socket = crate::bus::socket_path();
    let socket_str = socket.to_string_lossy().to_string();

    let shutdown_signal = async {
        // Listen for SIGTERM and SIGINT for graceful shutdown.
        // As PID 1, we don't get killed by SIGTERM unless we handle it.
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("failed to register SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )
        .expect("failed to register SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => println!("rev: received SIGTERM"),
            _ = sigint.recv() => println!("rev: received SIGINT"),
        }
    };

    tokio::select! {
        result = crate::bus::server::run(&socket_str) => {
            if let Err(e) = result {
                eprintln!("rev: wirebus server error: {}", e);
            }
        }
        _ = shutdown_signal => {
            shutdown().await;
        }
    }
}
