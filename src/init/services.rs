use crate::parser::ServiceInfo;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct ServicesStatus {
    services: HashMap<String, ServiceInfo>,
}

static SERVICES: Lazy<Mutex<ServicesStatus>> = Lazy::new(|| {
    Mutex::new(ServicesStatus {
        services: HashMap::new(),
    })
});

/// Maps PID -> service name for running processes.
static RUNNING_PROCESSES: Lazy<Mutex<HashMap<u32, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Register a service from its config.
pub fn register_service(name: String, info: ServiceInfo) {
    let mut status = SERVICES.lock().expect("services lock poisoned");
    status.services.insert(name, info);
}

pub fn get_service(name: &str) -> Option<ServiceInfo> {
    let status = SERVICES.lock().expect("services lock poisoned");
    status.services.get(name).cloned()
}

/// Look up a service by its running PID.
pub fn get_service_by_pid(pid: u32) -> Option<ServiceInfo> {
    let running = RUNNING_PROCESSES.lock().expect("running_processes lock poisoned");
    let name = running.get(&pid)?;
    let status = SERVICES.lock().expect("services lock poisoned");
    status.services.get(name).cloned()
}

pub fn list_services() -> Vec<(String, ServiceInfo)> {
    let status = SERVICES.lock().expect("services lock poisoned");
    status
        .services
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Update service status when starting or stopping.
/// - `name` + `new_pid` = starting a service
/// - `old_pid` only = a process exited (zombie reaped)
pub fn update_service_pid(name: Option<&str>, new_pid: Option<i32>, old_pid: Option<i32>) {
    let mut status = SERVICES.lock().expect("services lock poisoned");
    let mut running = RUNNING_PROCESSES.lock().expect("running_processes lock poisoned");

    if let Some(service_name) = name {
        if let Some(info) = status.services.get_mut(service_name) {
            if let Some(pid) = new_pid {
                info.is_running = true;
                info.pid = Some(pid as u32);
                info.up_timestamp = Some(chrono::Utc::now());
                running.insert(pid as u32, service_name.to_string());
            } else if let Some(old) = old_pid {
                info.is_running = false;
                info.pid = None;
                info.up_timestamp = None;
                running.remove(&(old as u32));
            }
        }
    }
}

/// Mark a service as exited by its PID. Called from zombie reaper.
pub fn mark_service_exited(pid: u32, exit_code: Option<i32>) {
    let mut running = RUNNING_PROCESSES.lock().expect("running_processes lock poisoned");
    let mut status = SERVICES.lock().expect("services lock poisoned");

    if let Some(service_name) = running.remove(&pid) {
        if let Some(info) = status.services.get_mut(&service_name) {
            info.is_running = false;
            info.pid = None;
            info.last_exit_code = exit_code;
            info.up_timestamp = None;
        }
    }
}

/// Increment the restart count for a service.
pub fn increment_restart_count(name: &str) {
    let mut status = SERVICES.lock().expect("services lock poisoned");
    if let Some(info) = status.services.get_mut(name) {
        info.restart_count += 1;
    }
}

pub fn deregister_service(name: &str) {
    let mut status = SERVICES.lock().expect("services lock poisoned");
    status.services.remove(name);
}
