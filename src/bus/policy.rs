//! WireBus authorization policy: the single place that decides who may do what.
//!
//! Every request on the bus passes through [`authorize`] before it is
//! dispatched. The decision is a pure function of three things:
//!
//!   * the [`Principal`] -- who is asking, resolved once per connection from
//!     unforgeable inputs (the socket peer credential, which listener accepted
//!     the connection, and an optional verified RookGuard token);
//!   * the [`Tier`] the request arrived on (the system Highway, or a user's
//!     private Lane);
//!   * the [`Operation`] -- a rev-internal description of the request, which the
//!     handler derives from the wire message and the resources it names.
//!
//! Keeping the wire types out of this module makes the policy a small, total,
//! unit-testable function. The governing rule is *scope*: acting within your own
//! scope (your session, your own services) needs only your ambient identity;
//! acting on another user or on the system needs a freshly proven RookGuard
//! capability. [`authorize`] never performs I/O and never verifies a token
//! itself -- when an operation requires elevation it returns
//! [`Access::Elevated`] naming the purpose, and the caller verifies a token for
//! that purpose against the principal.

use rook_core::policy::Purpose;

/// Who is making a request, resolved once when the connection is accepted. Every
/// field comes from a source the client cannot forge: the kernel socket
/// credential (`SO_PEERCRED`) for the uid, rev's own session table for
/// ownership, and UAC for the admin bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    /// The peer is uid 0: rev itself, the login greeter, or another root system
    /// component. Trusted by its credentials.
    System,
    /// The peer owns the currently active seat session. Distinguished from a
    /// plain user because only the foreground session may touch seat devices.
    SessionOwner { uid: u32, session_id: u64 },
    /// An authenticated local user. `admin` is the UAC administrator bit; it does
    /// not by itself grant cross-scope actions (those still need a token), but it
    /// gates whether a user is *allowed* to elevate at all.
    User { uid: u32, admin: bool },
    /// No peer credential could be read. Denied everything; fail closed.
    Anonymous,
}

impl Principal {
    /// The acting uid, if the principal has one.
    pub fn uid(&self) -> Option<u32> {
        match self {
            Principal::System => Some(0),
            Principal::SessionOwner { uid, .. } | Principal::User { uid, .. } => Some(*uid),
            Principal::Anonymous => None,
        }
    }
}

/// Which bus a request arrived on. A Lane is one user's private bus, so its
/// requests are self-scoped by construction; the Highway is the shared system
/// bus where cross-scope and privileged operations live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Highway,
    Lane { uid: u32 },
}

/// The scope a service-management request acts on: the caller's own services, or
/// the system / another user's. The handler resolves this from the named service
/// before asking the policy, because whether a name is "yours" is a fact about
/// the registry, not about the wire message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A service owned by this uid (a user-lane service).
    OwnUser(u32),
    /// A system (no-user) service, or a service owned by a different user.
    SystemOrOtherUser,
}

/// A rev-internal description of a request, derived by the handler from the wire
/// message plus the resources it names. The policy matches on this, never on the
/// wire `MessageBody`, so the two can evolve independently and the policy stays
/// trivially testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Read-only introspection: Lookup, ListBus, ListServices, ListSessions.
    /// On a Lane these are naturally scoped to that user's bus.
    Read,
    /// Subscribe / Unsubscribe to a signal. Read-like.
    SignalSubscribe,
    /// Emit a signal for a service name. `owns_name` is whether the principal
    /// owns that registered name.
    SignalEmit { owns_name: bool },
    /// Register a bus name. `owns_namespace` is whether the name falls in a
    /// namespace this principal is allowed to claim.
    Register { owns_namespace: bool },
    /// Unregister a bus name. `owns_name` is whether the principal owns it.
    Unregister { owns_name: bool },
    /// Start / stop / reload / rescan a service in the given scope.
    ServiceControl { scope: Scope },
    /// Open or close a seat device (DRM/input fd), or restore the VT. Allowed
    /// only to the active session owner or the system. Highway-only.
    Seat,
    /// Run a command as another user (the sudo path). Always needs an
    /// ElevateRoot token unless the caller is already System. Highway-only.
    ExecAs,
    /// Start a new user session (login). Greeter-only. Highway-only.
    StartSession,
    /// End a session. The owner may end their own; ending another's is
    /// cross-scope. `owner_uid` is the session's owner, if known.
    EndSession { owner_uid: Option<u32> },
}

/// The outcome of an authorization decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Proceed; the principal's ambient identity is sufficient.
    Allow,
    /// Proceed only if the caller presents a valid RookGuard token for this
    /// purpose, bound to the principal. The caller verifies it; the policy only
    /// states the requirement.
    Elevated(Purpose),
    /// Refuse, with a reason for the audit log and the error reply.
    Deny(String),
}

fn deny(reason: impl Into<String>) -> Access {
    Access::Deny(reason.into())
}

/// Operations that only make sense on the system Highway. Requesting them on a
/// user Lane is always refused: a Lane is a single user's private bus and must
/// never be a path to privileged or cross-user action.
fn highway_only(op: &Operation) -> bool {
    matches!(
        op,
        Operation::Seat | Operation::ExecAs | Operation::StartSession
    ) || matches!(
        op,
        Operation::ServiceControl { scope: Scope::SystemOrOtherUser }
    )
}

/// Decide whether `principal` may perform `op`, arriving on `tier`. Pure and
/// total: no I/O, no token verification, no panics.
pub fn authorize(principal: &Principal, tier: Tier, op: &Operation) -> Access {
    // No identity, no access. This must come first so nothing below can leak a
    // capability to an unauthenticated peer.
    if *principal == Principal::Anonymous {
        return deny("no peer identity on the connection");
    }

    // A Lane carries only self-scoped traffic. Privileged or cross-scope
    // operations belong on the Highway, where they are individually policed.
    if let Tier::Lane { .. } = tier {
        if highway_only(op) {
            return deny("operation is not available on a user lane; use the system bus");
        }
    }

    match op {
        Operation::Read | Operation::SignalSubscribe => Access::Allow,

        Operation::SignalEmit { owns_name } => {
            if *owns_name {
                Access::Allow
            } else {
                deny("cannot emit a signal for a name you do not own")
            }
        }

        Operation::Register { owns_namespace } => {
            if *owns_namespace {
                Access::Allow
            } else {
                deny("name is outside the namespace you may claim")
            }
        }

        Operation::Unregister { owns_name } => {
            if *owns_name {
                Access::Allow
            } else {
                deny("not the owner of this name")
            }
        }

        // Seat devices: only the foreground session's owner, or the system. The
        // SessionOwner principal already encodes active-session ownership, so its
        // presence here is the proof; a plain User is not the active compositor.
        Operation::Seat => match principal {
            Principal::System | Principal::SessionOwner { .. } => Access::Allow,
            _ => deny("only the active session owner may access seat devices"),
        },

        // Service control. Own-scope is free; system / other-user is cross-scope
        // and demands a freshly proven SystemServiceControl capability.
        Operation::ServiceControl { scope } => match scope {
            Scope::OwnUser(owner) => match principal.uid() {
                Some(uid) if uid == *owner || uid == 0 => Access::Allow,
                _ => deny("not your service"),
            },
            Scope::SystemOrOtherUser => match principal {
                Principal::System => Access::Allow,
                // Only a user permitted to elevate may even be asked for a token.
                Principal::User { admin: true, .. } | Principal::SessionOwner { .. } => {
                    Access::Elevated(Purpose::SystemServiceControl)
                }
                Principal::User { admin: false, .. } => {
                    deny("administering system or another user's services requires an administrator")
                }
                Principal::Anonymous => deny("no peer identity"),
            },
        },

        // Running as another user is always a token-gated action unless the
        // caller is already root. The handler binds the token to the target uid.
        Operation::ExecAs => match principal {
            Principal::System => Access::Allow,
            Principal::User { admin: true, .. } | Principal::SessionOwner { .. } => {
                Access::Elevated(Purpose::ElevateRoot)
            }
            Principal::User { admin: false, .. } => {
                deny("elevation requires an administrator account")
            }
            Principal::Anonymous => deny("no peer identity"),
        },

        // Spawning a login session is the greeter's job, and the greeter is root.
        Operation::StartSession => match principal {
            Principal::System => Access::Allow,
            _ => deny("only the system greeter may start sessions"),
        },

        // Ending your own session is self-scope; ending another's is cross-scope.
        Operation::EndSession { owner_uid } => match principal {
            Principal::System => Access::Allow,
            _ => match (principal.uid(), owner_uid) {
                (Some(uid), Some(owner)) if uid == *owner => Access::Allow,
                _ => Access::Elevated(Purpose::SystemServiceControl),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIGHWAY: Tier = Tier::Highway;

    fn user(uid: u32) -> Principal {
        Principal::User { uid, admin: false }
    }
    fn admin(uid: u32) -> Principal {
        Principal::User { uid, admin: true }
    }

    #[test]
    fn anonymous_is_denied_everything() {
        assert!(matches!(
            authorize(&Principal::Anonymous, HIGHWAY, &Operation::Read),
            Access::Deny(_)
        ));
        assert!(matches!(
            authorize(&Principal::Anonymous, HIGHWAY, &Operation::Seat),
            Access::Deny(_)
        ));
    }

    #[test]
    fn reads_are_open_to_any_authenticated_peer() {
        assert_eq!(authorize(&user(1000), HIGHWAY, &Operation::Read), Access::Allow);
        assert_eq!(
            authorize(&user(1000), Tier::Lane { uid: 1000 }, &Operation::Read),
            Access::Allow
        );
    }

    #[test]
    fn own_service_is_free_cross_scope_needs_token() {
        // Your own user service: no token.
        assert_eq!(
            authorize(&user(1000), HIGHWAY, &Operation::ServiceControl { scope: Scope::OwnUser(1000) }),
            Access::Allow
        );
        // Someone else's / a system service: a non-admin cannot even try.
        assert!(matches!(
            authorize(&user(1000), HIGHWAY, &Operation::ServiceControl { scope: Scope::SystemOrOtherUser }),
            Access::Deny(_)
        ));
        // An admin is asked for a SystemServiceControl token.
        assert_eq!(
            authorize(&admin(1000), HIGHWAY, &Operation::ServiceControl { scope: Scope::SystemOrOtherUser }),
            Access::Elevated(Purpose::SystemServiceControl)
        );
        // Root just does it.
        assert_eq!(
            authorize(&Principal::System, HIGHWAY, &Operation::ServiceControl { scope: Scope::SystemOrOtherUser }),
            Access::Allow
        );
    }

    #[test]
    fn another_users_service_is_not_own_scope() {
        // uid 1000 naming uid 1001's service resolves to SystemOrOtherUser, so it
        // is cross-scope even for an admin (token required), and denied for a
        // non-admin.
        assert!(matches!(
            authorize(&user(1000), HIGHWAY, &Operation::ServiceControl { scope: Scope::SystemOrOtherUser }),
            Access::Deny(_)
        ));
    }

    #[test]
    fn privileged_ops_are_highway_only() {
        let lane = Tier::Lane { uid: 1000 };
        for op in [
            Operation::Seat,
            Operation::ExecAs,
            Operation::StartSession,
            Operation::ServiceControl { scope: Scope::SystemOrOtherUser },
        ] {
            assert!(
                matches!(authorize(&admin(1000), lane, &op), Access::Deny(_)),
                "{op:?} should be denied on a lane"
            );
        }
    }

    #[test]
    fn seat_needs_active_session_owner() {
        assert_eq!(
            authorize(&Principal::SessionOwner { uid: 1000, session_id: 7 }, HIGHWAY, &Operation::Seat),
            Access::Allow
        );
        assert_eq!(authorize(&Principal::System, HIGHWAY, &Operation::Seat), Access::Allow);
        // A logged-in user who is not the active compositor: denied.
        assert!(matches!(
            authorize(&user(1000), HIGHWAY, &Operation::Seat),
            Access::Deny(_)
        ));
    }

    #[test]
    fn exec_as_requires_admin_token_or_root() {
        assert_eq!(authorize(&Principal::System, HIGHWAY, &Operation::ExecAs), Access::Allow);
        assert_eq!(
            authorize(&admin(1000), HIGHWAY, &Operation::ExecAs),
            Access::Elevated(Purpose::ElevateRoot)
        );
        assert!(matches!(
            authorize(&user(1000), HIGHWAY, &Operation::ExecAs),
            Access::Deny(_)
        ));
    }

    #[test]
    fn start_session_is_greeter_only() {
        assert_eq!(authorize(&Principal::System, HIGHWAY, &Operation::StartSession), Access::Allow);
        assert!(matches!(
            authorize(&admin(1000), HIGHWAY, &Operation::StartSession),
            Access::Deny(_)
        ));
    }

    #[test]
    fn end_session_own_vs_others() {
        // Own session: free.
        assert_eq!(
            authorize(&user(1000), HIGHWAY, &Operation::EndSession { owner_uid: Some(1000) }),
            Access::Allow
        );
        // Another's session: cross-scope, token required.
        assert_eq!(
            authorize(&user(1000), HIGHWAY, &Operation::EndSession { owner_uid: Some(1001) }),
            Access::Elevated(Purpose::SystemServiceControl)
        );
    }

    #[test]
    fn name_ownership_gates_registry_and_signals() {
        assert_eq!(
            authorize(&user(1000), HIGHWAY, &Operation::Register { owns_namespace: true }),
            Access::Allow
        );
        assert!(matches!(
            authorize(&user(1000), HIGHWAY, &Operation::Register { owns_namespace: false }),
            Access::Deny(_)
        ));
        assert_eq!(
            authorize(&user(1000), HIGHWAY, &Operation::SignalEmit { owns_name: true }),
            Access::Allow
        );
        assert!(matches!(
            authorize(&user(1000), HIGHWAY, &Operation::SignalEmit { owns_name: false }),
            Access::Deny(_)
        ));
    }
}
