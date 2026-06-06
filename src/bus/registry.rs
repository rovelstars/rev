//! WireBus service registry.
//!
//! Tracks which services have registered on the bus, their socket paths,
//! methods they expose, and signal subscriptions.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::protocol::BusEntry;

#[derive(Debug, Clone)]
pub struct Registration {
    pub name: String,
    pub socket_path: PathBuf,
    pub methods: HashMap<String, String>,
}

/// A signal subscription: (service_name, signal_name).
/// signal_name = "*" means all signals from that service.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct Subscription {
    pub service: String,
    pub signal: String,
}

struct RegistryState {
    services: HashMap<String, Registration>,
    /// Maps subscriber_name -> set of subscriptions
    subscriptions: HashMap<String, HashSet<Subscription>>,
}

static REGISTRY: Lazy<Mutex<RegistryState>> = Lazy::new(|| {
    Mutex::new(RegistryState {
        services: HashMap::new(),
        subscriptions: HashMap::new(),
    })
});

/// Register a service on the bus.
pub fn register(
    name: String,
    socket_path: PathBuf,
    methods: HashMap<String, String>,
) -> Result<(), String> {
    let mut reg = REGISTRY.lock().expect("bus registry lock poisoned");
    if reg.services.contains_key(&name) {
        return Err(format!("service '{}' is already registered on the bus", name));
    }
    reg.services.insert(
        name.clone(),
        Registration {
            name,
            socket_path,
            methods,
        },
    );
    Ok(())
}

/// Unregister a service from the bus. Also removes all its subscriptions
/// and any subscriptions others have to its signals.
pub fn unregister(name: &str) -> Result<(), String> {
    let mut reg = REGISTRY.lock().expect("bus registry lock poisoned");
    if reg.services.remove(name).is_none() {
        return Err(format!("service '{}' is not registered on the bus", name));
    }
    // Remove this service's own subscriptions
    reg.subscriptions.remove(name);
    // Remove subscriptions others have to this service's signals
    for subs in reg.subscriptions.values_mut() {
        subs.retain(|s| s.service != name);
    }
    Ok(())
}

/// Look up a registered service by name.
pub fn lookup(name: &str) -> Option<Registration> {
    let reg = REGISTRY.lock().expect("bus registry lock poisoned");
    reg.services.get(name).cloned()
}

/// List all registered services.
pub fn list() -> Vec<BusEntry> {
    let reg = REGISTRY.lock().expect("bus registry lock poisoned");
    reg.services
        .values()
        .map(|r| BusEntry {
            name: r.name.clone(),
            socket_path: r.socket_path.clone(),
            methods: r.methods.clone(),
        })
        .collect()
}

/// Subscribe a client to signals from a service.
/// Use signal = "*" to subscribe to all signals.
pub fn subscribe(subscriber: &str, service: &str, signal: &str) -> Result<(), String> {
    let mut reg = REGISTRY.lock().expect("bus registry lock poisoned");
    // The target service must exist (unless subscribing to a service that hasn't started yet)
    let subs = reg
        .subscriptions
        .entry(subscriber.to_string())
        .or_default();
    subs.insert(Subscription {
        service: service.to_string(),
        signal: signal.to_string(),
    });
    Ok(())
}

/// Unsubscribe a client from a specific signal.
pub fn unsubscribe(subscriber: &str, service: &str, signal: &str) -> Result<(), String> {
    let mut reg = REGISTRY.lock().expect("bus registry lock poisoned");
    if let Some(subs) = reg.subscriptions.get_mut(subscriber) {
        let key = Subscription {
            service: service.to_string(),
            signal: signal.to_string(),
        };
        if !subs.remove(&key) {
            return Err(format!(
                "'{}' is not subscribed to {}:{}",
                subscriber, service, signal
            ));
        }
    } else {
        return Err(format!("'{}' has no subscriptions", subscriber));
    }
    Ok(())
}

/// Find all subscribers for a given signal from a given source service.
/// Returns a list of subscriber names.
pub fn get_signal_subscribers(source: &str, signal: &str) -> Vec<String> {
    let reg = REGISTRY.lock().expect("bus registry lock poisoned");
    let mut result = Vec::new();
    for (subscriber, subs) in &reg.subscriptions {
        for sub in subs {
            if sub.service == source && (sub.signal == signal || sub.signal == "*") {
                result.push(subscriber.clone());
                break; // don't add same subscriber twice
            }
        }
    }
    result
}
