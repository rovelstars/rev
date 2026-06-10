//! Boot start ordering from service dependencies.
//!
//! systemd orders units with `After=`/`Before=` and pulls them in with
//! `Requires=`/`Wants=`. rev maps all of these onto one question at boot: given
//! every service we are about to start, in what order do we start them so each
//! starts after the services it depends on?
//!
//! [`start_order`] answers that with a stable topological sort: with no
//! constraints the input (discovery) order is preserved, unknown dependency
//! names are ignored, and a dependency cycle is broken deterministically (the
//! offending nodes are emitted in input order and reported, never dropped).

use crate::parser::ServiceConfig;
use std::collections::HashMap;

/// Compute the order to start `services` in, returned as indices into the slice.
///
/// `after`, `requires`, and `wants` all mean "start the named service before
/// this one"; `before` is the inverse edge. The second return value lists the
/// names whose ordering had to be forced because they took part in a cycle, so
/// the caller can log it.
pub fn start_order(services: &[(String, ServiceConfig)]) -> (Vec<usize>, Vec<String>) {
    let n = services.len();
    let index: HashMap<&str, usize> = services
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (name.as_str(), i))
        .collect();

    // prereqs[i] = the set of nodes that must start before node i.
    let mut prereqs: Vec<Vec<usize>> = vec![Vec::new(); n];
    let add = |before: usize, after: usize, prereqs: &mut Vec<Vec<usize>>| {
        // `before` must start before `after`: it is a prerequisite of `after`.
        if before != after && !prereqs[after].contains(&before) {
            prereqs[after].push(before);
        }
    };

    for (i, (_, cfg)) in services.iter().enumerate() {
        for dep in cfg.after.iter().chain(&cfg.requires).chain(&cfg.wants) {
            if let Some(&j) = index.get(dep.as_str()) {
                add(j, i, &mut prereqs);
            }
        }
        for dep in &cfg.before {
            if let Some(&j) = index.get(dep.as_str()) {
                // i must start before j.
                add(i, j, &mut prereqs);
            }
        }
    }

    let mut emitted = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut forced = Vec::new();

    while order.len() < n {
        // Stable pick: the first not-yet-emitted node whose prerequisites are
        // all already emitted.
        let ready = (0..n).find(|&i| {
            !emitted[i] && prereqs[i].iter().all(|&p| emitted[p])
        });

        let pick = match ready {
            Some(i) => i,
            None => {
                // Every remaining node is blocked by another remaining node: a
                // cycle. Break it by forcing the first remaining node, recording
                // it so the caller can warn.
                let i = (0..n).find(|&i| !emitted[i]).expect("nodes remain");
                forced.push(services[i].0.clone());
                i
            }
        };
        emitted[pick] = true;
        order.push(pick);
    }

    (order, forced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, after: &[&str], before: &[&str]) -> (String, ServiceConfig) {
        (
            name.to_string(),
            ServiceConfig {
                name: name.to_string(),
                after: after.iter().map(|s| s.to_string()).collect(),
                before: before.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )
    }

    fn names<'a>(services: &'a [(String, ServiceConfig)], order: &[usize]) -> Vec<&'a str> {
        order.iter().map(|&i| services[i].0.as_str()).collect()
    }

    #[test]
    fn no_constraints_preserves_input_order() {
        let s = vec![svc("a", &[], &[]), svc("b", &[], &[]), svc("c", &[], &[])];
        let (order, forced) = start_order(&s);
        assert_eq!(names(&s, &order), ["a", "b", "c"]);
        assert!(forced.is_empty());
    }

    #[test]
    fn after_starts_dependency_first() {
        // c declared first but must start after a and b.
        let s = vec![svc("c", &["a", "b"], &[]), svc("a", &[], &[]), svc("b", &[], &[])];
        let (order, _) = start_order(&s);
        let o = names(&s, &order);
        let pos = |x| o.iter().position(|&y| y == x).unwrap();
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn before_is_the_inverse_edge() {
        // a must start before b.
        let s = vec![svc("b", &[], &[]), svc("a", &[], &["b"])];
        let (order, _) = start_order(&s);
        let o = names(&s, &order);
        assert!(o.iter().position(|&y| y == "a") < o.iter().position(|&y| y == "b"));
    }

    #[test]
    fn unknown_dependencies_are_ignored() {
        let s = vec![svc("a", &["ghost"], &[])];
        let (order, forced) = start_order(&s);
        assert_eq!(names(&s, &order), ["a"]);
        assert!(forced.is_empty());
    }

    #[test]
    fn cycle_is_broken_not_dropped() {
        // a after b, b after a: a cycle. All nodes still come out, and the
        // forced list is non-empty.
        let s = vec![svc("a", &["b"], &[]), svc("b", &["a"], &[])];
        let (order, forced) = start_order(&s);
        assert_eq!(order.len(), 2);
        assert!(!forced.is_empty());
    }
}
