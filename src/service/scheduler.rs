//! Cron scheduler for periodic service execution.
//!
//! Spawns a background tokio task that checks service schedules every minute.
//! When a service's cron schedule matches the current time, it either starts
//! the service (if not running) or restarts it (if force_restart_on_schedule).

use chrono::Utc;
use croner::Cron;
use std::str::FromStr;

/// Start the cron scheduler background task.
pub fn start_scheduler() {
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

        loop {
            interval.tick().await;
            check_schedules();
        }
    });
}

fn check_schedules() {
    let services = crate::init::services::list_services();
    let now = Utc::now();

    for (_name, info) in services {
        let schedule = match &info.config.schedule {
            Some(cron_str) => cron_str,
            None => continue,
        };

        let cron = match Cron::from_str(&schedule.0) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "rev: invalid cron schedule for {}: {}",
                    info.name, e
                );
                continue;
            }
        };

        // Check if the cron expression matches the current minute.
        // We check if there's a scheduled time between (now - 60s) and now.
        let window_start = now - chrono::Duration::seconds(60);
        let has_match = cron
            .iter_from(window_start, croner::Direction::Forward)
            .take(1)
            .any(|t| t <= now);

        if !has_match {
            continue;
        }

        if info.is_running {
            if info.config.force_restart_on_schedule {
                println!(
                    "rev: scheduled restart for {} (force_restart_on_schedule)",
                    info.name
                );
                crate::logger::write_log(
                    &info.name,
                    "Scheduled restart (force-restart-on-schedule = true)",
                );
                // Stop then restart
                crate::service::stop_service(&info);
                // The restart will happen via the restart policy in handle_exit,
                // or we can restart directly after a brief delay
                if let Some(ref config_path) = info.config_path {
                    let path = std::path::PathBuf::from(config_path);
                    let name = info.name.clone();
                    std::thread::spawn(move || {
                        // Wait for process to actually die
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        crate::init::services::deregister_service(&name);
                        crate::service::start_service_from_path(&path);
                    });
                }
            }
            // If not force_restart, leave it running
        } else {
            // Service not running — start it on schedule
            println!("rev: scheduled start for {}", info.name);
            crate::logger::write_log(&info.name, "Starting on cron schedule");
            if let Some(ref config_path) = info.config_path {
                let path = std::path::PathBuf::from(config_path);
                let name = info.name.clone();
                // Deregister first since it's already registered but not running
                crate::init::services::deregister_service(&name);
                crate::service::start_service_from_path(&path);
            }
        }
    }
}
