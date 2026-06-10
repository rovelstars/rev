//! WireBus service registry.
//!
//! Tracks which services have registered on a bus, their socket paths, the
//! methods they expose, who owns each name, and signal subscriptions.
//!
//! A [`Registry`] is per-bus: the system Highway has one, and every user Lane
//! has its own. Keeping them separate is part of lane isolation -- one user's
//! lane registrations are not visible on another's lane or on the Highway. Reads
//! (Lookup, the hot path) take a shared lock; mutations take it exclusively.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;

use super::protocol::BusEntry;

#[derive(Debug, Clone)]
pub struct Registration {
    pub name: String,
    pub socket_path: PathBuf,
    pub methods: HashMap<String, String>,
    /// The uid that registered this name (0 = a system/root registration). Used
    /// to enforce that only the owner may unregister it or emit signals as it.
    pub owner_uid: u32,
}

/// A signal subscription: (service_name, signal_name).
/// signal_name = "*" means all signals from that service.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct Subscription {
    pub service: String,
    pub signal: String,
}

#[derive(Default)]
struct RegistryState {
    services: HashMap<String, Registration>,
    /// Maps subscriber_name -> set of subscriptions.
    subscriptions: HashMap<String, HashSet<Subscription>>,
}

/// One bus's registry.
#[derive(Default)]
pub struct Registry {
    state: RwLock<RegistryState>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, RegistryState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, RegistryState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a service. `owner_uid` is the registering principal's uid.
    pub fn register(
        &self,
        name: String,
        socket_path: PathBuf,
        methods: HashMap<String, String>,
        owner_uid: u32,
    ) -> Result<(), String> {
        let mut reg = self.write();
        if reg.services.contains_key(&name) {
            return Err(format!("service '{}' is already registered on the bus", name));
        }
        reg.services.insert(
            name.clone(),
            Registration { name, socket_path, methods, owner_uid },
        );
        Ok(())
    }

    /// Unregister a service. Also removes its subscriptions and any subscriptions
    /// others held to its signals.
    pub fn unregister(&self, name: &str) -> Result<(), String> {
        let mut reg = self.write();
        if reg.services.remove(name).is_none() {
            return Err(format!("service '{}' is not registered on the bus", name));
        }
        reg.subscriptions.remove(name);
        for subs in reg.subscriptions.values_mut() {
            subs.retain(|s| s.service != name);
        }
        Ok(())
    }

    /// The uid that owns a registered name, or `None` if the name is not
    /// registered. Used by the authorization layer to gate Unregister and
    /// EmitSignal on ownership.
    pub fn owner_of(&self, name: &str) -> Option<u32> {
        self.read().services.get(name).map(|r| r.owner_uid)
    }

    /// Look up a registered service by name.
    pub fn lookup(&self, name: &str) -> Option<Registration> {
        self.read().services.get(name).cloned()
    }

    /// List all registered services.
    pub fn list(&self) -> Vec<BusEntry> {
        self.read()
            .services
            .values()
            .map(|r| BusEntry {
                name: r.name.clone(),
                socket_path: r.socket_path.clone(),
                methods: r.methods.clone(),
            })
            .collect()
    }

    /// Subscribe a client to signals from a service. Use signal = "*" for all.
    pub fn subscribe(&self, subscriber: &str, service: &str, signal: &str) -> Result<(), String> {
        let mut reg = self.write();
        let subs = reg.subscriptions.entry(subscriber.to_string()).or_default();
        subs.insert(Subscription {
            service: service.to_string(),
            signal: signal.to_string(),
        });
        Ok(())
    }

    /// Unsubscribe a client from a specific signal.
    pub fn unsubscribe(&self, subscriber: &str, service: &str, signal: &str) -> Result<(), String> {
        let mut reg = self.write();
        if let Some(subs) = reg.subscriptions.get_mut(subscriber) {
            let key = Subscription {
                service: service.to_string(),
                signal: signal.to_string(),
            };
            if !subs.remove(&key) {
                return Err(format!("'{}' is not subscribed to {}:{}", subscriber, service, signal));
            }
        } else {
            return Err(format!("'{}' has no subscriptions", subscriber));
        }
        Ok(())
    }

    /// All subscriber names interested in `signal` from `source`.
    pub fn get_signal_subscribers(&self, source: &str, signal: &str) -> Vec<String> {
        let reg = self.read();
        let mut result = Vec::new();
        for (subscriber, subs) in &reg.subscriptions {
            for sub in subs {
                if sub.service == source && (sub.signal == signal || sub.signal == "*") {
                    result.push(subscriber.clone());
                    break;
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_tracked() {
        let r = Registry::new();
        r.register("com.user.app".into(), PathBuf::from("/x"), HashMap::new(), 1000).unwrap();
        assert_eq!(r.owner_of("com.user.app"), Some(1000));
        assert_eq!(r.owner_of("com.other"), None);
        // Duplicate name is refused.
        assert!(r.register("com.user.app".into(), PathBuf::from("/y"), HashMap::new(), 1001).is_err());
        r.unregister("com.user.app").unwrap();
        assert_eq!(r.owner_of("com.user.app"), None);
    }

    #[test]
    fn registries_are_independent() {
        let a = Registry::new();
        let b = Registry::new();
        a.register("svc".into(), PathBuf::from("/a"), HashMap::new(), 0).unwrap();
        // A name on one bus is invisible on another.
        assert!(a.lookup("svc").is_some());
        assert!(b.lookup("svc").is_none());
    }
}
