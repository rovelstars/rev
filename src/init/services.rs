use crate::parser::ServiceInfo;
use once_cell::sync::Lazy;
use std::sync::Mutex;

// SERVICES = hashmap of service name to ServiceInfo
#[derive(Default)]
struct ServicesStatus {
    services: std::collections::HashMap<String, ServiceInfo>,
}

static SERVICES: Lazy<Mutex<ServicesStatus>> = Lazy::new(|| {
    let status = ServicesStatus {
        services: std::collections::HashMap::new(),
    };
    Mutex::new(status)
});

// hashmap of pid to service name for running processes
static RUNNING_PROCESSES: Lazy<Mutex<std::collections::HashMap<u32, String>>> = Lazy::new(|| {
    let map = std::collections::HashMap::new();
    Mutex::new(map)
});

// NOTE: Registration happens when a service is read from disk, not when it is started.
// Therefore, its IsRunning and Pid fields are not set at registration time.
// As long as the config file is present, the service is considered registered.

// register a new service from its config
pub fn register_service(name: String, info: ServiceInfo) {
    let mut status = SERVICES.lock().unwrap();
    status.services.insert(name, info);
}

// get service info by name
pub fn get_service(name: &str) -> Option<ServiceInfo> {
    let status = SERVICES.lock().unwrap();
    status.services.get(name).cloned()
}

// list all services that are registered
pub fn list_services() -> Vec<(String, ServiceInfo)> {
    let status = SERVICES.lock().unwrap();
    status
        .services
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Update service status, used when starting/stopping a service.
/// If `new_pid` is None, the service is being stopped.
pub fn update_service_pid(name: Option<&str>, new_pid: Option<i32>, old_pid: Option<i32>) {
  //if name & pid provided = starting service
  // if only old_pid provided = stopping service
    let mut status = SERVICES.lock().unwrap();
    let mut running_processes = RUNNING_PROCESSES.lock().unwrap();

    if let Some(service_name) = name {
        if let Some(service_info) = status.services.get_mut(service_name) {
            if let Some(pid) = new_pid {
                // Starting service
                service_info.IsRunning = true;
                service_info.Pid = Some(pid as u32);
                service_info.UpTimestamp = Some(chrono::Utc::now());
                running_processes.insert(pid as u32, service_name.to_string());
            } else if let Some(old_pid) = old_pid {
                // Stopping service
                service_info.IsRunning = false;
                service_info.Pid = None;
                service_info.UpTimestamp = None;
                running_processes.remove(&(old_pid as u32));
            }
        }
    }
}

pub fn deregister_service(name: &str) {
    let mut status = SERVICES.lock().unwrap();
    status.services.remove(name);
}
