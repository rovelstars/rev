//! RookGuard + UAC integration for rev's privileged operations.
//!
//! rev (PID 1, root) must never run something on a caller's behalf without
//! proof. The authorization policy -- verify a RookGuard capability token bound
//! to the connecting socket peer, resolve the target through UAC, allowlist the
//! environment -- is shared with rexecd in the `rook-elevate` crate so the two
//! privileged exec paths cannot drift. This module is a thin rev-side adapter:
//! it opens UAC, loads rookd's public key, owns the process's single-use nonce
//! cache, and maps errors to the strings rev's bus replies with.

use rook_core::policy::Purpose;
use rook_elevate::{authorize_for, now, rookd_pubkey, NonceCache};
use std::path::PathBuf;
use std::sync::OnceLock;
use uac_core::Uac;

pub use rook_elevate::{env_allowed, Target};

/// Token nonces rev has already redeemed (single-use within the TTL). Process
/// lifetime; shared with rexecd's behaviour via the same `rook-elevate` logic.
fn nonces() -> &'static NonceCache {
    static N: OnceLock<NonceCache> = OnceLock::new();
    N.get_or_init(NonceCache::new)
}

fn pubkey() -> Result<[u8; 32], String> {
    let dir = std::env::var("ROOK_KEY_DIR").unwrap_or_else(|_| "/Vault/State/RookGuard".into());
    rookd_pubkey(&PathBuf::from(dir)).map_err(|e| format!("{e:#}"))
}

/// Authorize a caller to elevate (an ExecAs request). `peer_uid` comes from the
/// socket (SO_PEERCRED) so it cannot be forged. Returns the authorized caller
/// name. Delegates to the shared `rook-elevate` policy: root by credentials,
/// everyone else by a single-use ElevateRoot token bound to their own uid and
/// UAC admin status.
/// Verify a RookGuard capability token for an arbitrary `purpose`, bound to the
/// connecting peer. Used by the bus authorization choke point when the policy
/// returns `Elevated(purpose)` (ElevateRoot for ExecAs, SystemServiceControl for
/// cross-scope service administration). Returns the authorized caller name.
pub fn verify_for(peer_uid: Option<u32>, auth_token: Option<&str>, purpose: Purpose) -> Result<String, String> {
    let uac = Uac::open().map_err(|e| format!("open UAC: {e}"))?;
    let pubkey = pubkey()?;
    authorize_for(&uac, &pubkey, peer_uid, auth_token, now(), nonces(), purpose)
        .map_err(|e| format!("{e:#}"))
}

/// Resolve a target uid to its name/gid/home/shell through UAC, instead of
/// trusting a caller-supplied gid or the old gid==uid fallback.
pub fn resolve_target(uid: u32) -> Result<Target, String> {
    let uac = Uac::open().map_err(|e| format!("open UAC: {e}"))?;
    rook_elevate::resolve_by_uid(&uac, uid).map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rook_core::policy::{AssuranceTier, Purpose};
    use rook_core::token::{self, Claims, Keypair};
    use uac_core::{CreateOpts, Uac};

    fn token_for(kp: &Keypair, sub: &str, uid: u32, exp_in: i64) -> String {
        let n = now();
        let claims = Claims {
            sub: sub.into(),
            purpose: Purpose::ElevateRoot,
            audience: format!("uid:{uid}"),
            factor: "password".into(),
            tier: AssuranceTier::High,
            strength: 2,
            iat: n,
            exp: n + exp_in,
            nonce: token::nonce(),
        };
        kp.issue(&claims).unwrap()
    }

    #[test]
    fn elevation_authorization() {
        let dir = std::env::temp_dir().join(format!("rev-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let keyd = dir.join("keys");
        std::fs::create_dir_all(&keyd).unwrap();
        // SAFETY: test-local; this is the only rev test touching these vars.
        unsafe {
            std::env::set_var("UAC_ROOT", dir.join("vault"));
            std::env::set_var("SPACE_ROOT", dir.join("space"));
            std::env::set_var("ROOK_KEY_DIR", &keyd);
        }

        let uac = Uac::open().unwrap();
        uac.create("boss", None, CreateOpts { admin: true, uid: Some(4242), no_space: true, ..Default::default() }).unwrap();
        uac.create("peon", None, CreateOpts { admin: false, uid: Some(4343), no_space: true, ..Default::default() }).unwrap();
        let kp = Keypair::generate();
        std::fs::write(keyd.join("rookd.pub"), kp.public_bytes()).unwrap();

        let elevate = |peer, tok: Option<&str>| verify_for(peer, tok, Purpose::ElevateRoot);

        // root peer: trusted by credentials, no token needed.
        assert_eq!(elevate(Some(0), None).unwrap(), "root");
        // no peer identity at all: refused.
        assert!(elevate(None, None).is_err());
        // admin with a valid token bound to its own uid: authorized.
        let t = token_for(&kp, "boss", 4242, 120);
        assert_eq!(elevate(Some(4242), Some(&t)).unwrap(), "boss");
        // the same token a second time: refused (single-use, now enforced on the
        // rev path too, not only rexecd).
        assert!(elevate(Some(4242), Some(&t)).is_err());
        // same admin, no token: refused (fail closed).
        assert!(elevate(Some(4242), None).is_err());
        // token presented by a different uid (audience mismatch): refused.
        let t2 = token_for(&kp, "boss", 4242, 120);
        assert!(elevate(Some(9999), Some(&t2)).is_err());
        // expired token: refused.
        let expired = token_for(&kp, "boss", 4242, -10);
        assert!(elevate(Some(4242), Some(&expired)).is_err());
        // non-admin with a valid token: refused (not permitted to elevate).
        let pt = token_for(&kp, "peon", 4343, 120);
        assert!(elevate(Some(4343), Some(&pt)).is_err());
        // subject mismatch: token sub != the calling uid's account.
        let mismatch = token_for(&kp, "peon", 4242, 120);
        assert!(elevate(Some(4242), Some(&mismatch)).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
