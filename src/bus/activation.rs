//! Bus-activation: start a service on demand when a client looks up a name it
//! provides.
//!
//! A service declares the WireBus names it offers with `provides = [...]` in its
//! .rsc file. When a Highway `Lookup` for one of those names misses (the service
//! is loaded but idle), rev starts the service and waits for it to register the
//! name before answering, so the client sees the service as if it had been
//! running all along. This is the rev equivalent of D-Bus service activation.
//!
//! Activation is only attempted for names some loaded service actually declares,
//! so a client cannot make rev start anything that did not opt in. Concurrent
//! lookups for the same name start the service only once (`IN_FLIGHT`).

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;

use super::registry::Registry;

/// Names currently being activated, so concurrent lookups for the same name
/// trigger a single service start rather than one per caller.
static IN_FLIGHT: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// How long to wait for an activated service to register its name.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How often to re-check the registry while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The name of a loaded service that declares it provides `bus_name`, if any.
fn provider_of(bus_name: &str) -> Option<String> {
    crate::init::services::list_services()
        .into_iter()
        .find(|(_, info)| info.config.provides.iter().any(|p| p == bus_name))
        .map(|(name, _)| name)
}

/// Ensure the service providing `bus_name` is started and has registered the
/// name on the bus. Returns whether `bus_name` is registered by the time this
/// returns: `true` if it was already registered or activation succeeded,
/// `false` if no service provides it or it did not register before the timeout.
pub async fn activate(bus_name: &str, registry: &Registry) -> bool {
    if registry.lookup(bus_name).is_some() {
        return true;
    }
    let service = match provider_of(bus_name) {
        Some(s) => s,
        None => return false, // nothing opted in to provide this name
    };

    // Exactly one caller starts the service; the rest just wait for it.
    let we_start = IN_FLIGHT.lock().unwrap().insert(bus_name.to_string());
    if we_start {
        crate::logger::write_log(
            "rev",
            &format!("bus-activation: starting '{service}' to provide '{bus_name}'"),
        );
        crate::service::start_known_service(&service);
    }

    let mut waited = Duration::ZERO;
    let mut found = registry.lookup(bus_name).is_some();
    while !found && waited < ACTIVATION_TIMEOUT {
        tokio::time::sleep(POLL_INTERVAL).await;
        waited += POLL_INTERVAL;
        found = registry.lookup(bus_name).is_some();
    }

    if we_start {
        IN_FLIGHT.lock().unwrap().remove(bus_name);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ServiceConfig, ServiceInfo};

    #[test]
    fn no_provider_means_no_activation() {
        // A name nothing declares: activate is a no-op that reports failure.
        let registry = Registry::new();
        let got = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(activate("nobody.provides.this", &registry));
        assert!(!got);
    }

    #[test]
    fn already_registered_short_circuits() {
        // If the name is already on the bus, activate returns true without
        // touching services at all.
        let registry = Registry::new();
        registry
            .register(
                "already.up".to_string(),
                "/tmp/already.sock".into(),
                Default::default(),
                0,
            )
            .unwrap();
        let got = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(activate("already.up", &registry));
        assert!(got);
    }

    #[test]
    fn provider_lookup_matches_provides() {
        // A loaded service that declares the name is found as its provider.
        let mut config = ServiceConfig {
            name: "act-test-svc".to_string(),
            ..Default::default()
        };
        config.provides = vec!["act.test.name".to_string()];
        crate::init::services::register_service(
            "act-test-svc".to_string(),
            ServiceInfo {
                name: "act-test-svc".to_string(),
                config,
                ..Default::default()
            },
        );
        assert_eq!(provider_of("act.test.name").as_deref(), Some("act-test-svc"));
        assert_eq!(provider_of("act.test.absent"), None);
    }
}
