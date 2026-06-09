//! RookGuard + UAC integration for rev's privileged operations.
//!
//! rev (PID 1, root) must never run something on a caller's behalf without
//! proof. This module verifies a RookGuard capability token bound to the
//! connecting socket peer and resolves the target user through UAC -- the same
//! rules rexecd applies, so the two privileged exec paths cannot drift.

use rook_core::policy::Purpose;
use rook_core::token;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uac_core::Uac;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn rookd_pubkey() -> Result<[u8; 32], String> {
    let dir = std::env::var("ROOK_KEY_DIR").unwrap_or_else(|_| "/Vault/State/RookGuard".into());
    let path = PathBuf::from(dir).join("rookd.pub");
    let bytes = std::fs::read(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    bytes.as_slice().try_into().map_err(|_| "rookd.pub is not 32 bytes".to_string())
}

/// Authorize a caller to elevate (an ExecAs request). `peer_uid` comes from the
/// socket (SO_PEERCRED) so it cannot be forged. A root caller (a system
/// component) is trusted by its credentials; anyone else must present a valid
/// ElevateRoot token bound to their own uid, whose subject is themselves, and be
/// a UAC admin. Returns the authorized caller name on success.
pub fn authorize_elevation(peer_uid: Option<u32>, auth_token: Option<&str>) -> Result<String, String> {
    let peer = peer_uid.ok_or_else(|| "no peer identity on the connection".to_string())?;
    if peer == 0 {
        return Ok("root".into());
    }
    let token = auth_token
        .ok_or_else(|| "elevation requires RookGuard authentication".to_string())?;
    let pubkey = rookd_pubkey()?;
    let audience = format!("uid:{peer}");
    let claims = token::verify(&pubkey, token, now(), &audience, Purpose::ElevateRoot)
        .map_err(|e| format!("token verification: {e}"))?;
    let uac = Uac::open().map_err(|e| format!("open UAC: {e}"))?;
    let name = uac
        .name_by_uid(peer)
        .map_err(|e| format!("{e}"))?
        .ok_or_else(|| format!("uid {peer} is not a UAC account"))?;
    if claims.sub != name {
        return Err(format!("token subject {:?} does not match caller {name}", claims.sub));
    }
    if !uac.is_admin(&name).map_err(|e| format!("{e}"))? {
        return Err(format!("{name} is not permitted to elevate"));
    }
    Ok(name)
}

/// The target user's resolved identity for a privileged exec / session spawn.
pub struct Target {
    pub name: String,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

/// Resolve a target uid to its name/gid/home/shell through UAC, instead of
/// trusting a caller-supplied gid or the old gid==uid fallback. root is
/// well-known.
pub fn resolve_target(uid: u32) -> Result<Target, String> {
    if uid == 0 {
        return Ok(Target {
            name: "root".into(),
            gid: 0,
            home: "/Space/root".into(),
            shell: "/Core/Bin/brush".into(),
        });
    }
    let uac = Uac::open().map_err(|e| format!("open UAC: {e}"))?;
    let name = uac
        .name_by_uid(uid)
        .map_err(|e| format!("{e}"))?
        .ok_or_else(|| format!("uid {uid} is not a UAC account"))?;
    let a = uac.get(&name).map_err(|e| format!("{e}"))?;
    Ok(Target {
        name: a.name,
        gid: a.gid,
        home: a.home.unwrap_or_else(|| "/".into()),
        shell: a.shell,
    })
}

/// Whether an environment variable may ride into a privileged child. Allowlist,
/// not a denylist: only a safe display/locale subset passes; loader/shell
/// variables (LD_PRELOAD, LD_LIBRARY_PATH, IFS, BASH_ENV, ...) can never slip
/// through. Mirrors rexecd's env sanitation.
pub fn env_allowed(name: &str) -> bool {
    const ALLOW: &[&str] = &[
        "TERM",
        "COLORTERM",
        "TERMINFO",
        "LANG",
        "LANGUAGE",
        "TZ",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
    ];
    ALLOW.contains(&name) || name.starts_with("LC_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rook_core::policy::AssuranceTier;
    use rook_core::token::{Claims, Keypair};
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

        // root peer: trusted by credentials, no token needed.
        assert_eq!(authorize_elevation(Some(0), None).unwrap(), "root");
        // no peer identity at all: refused.
        assert!(authorize_elevation(None, None).is_err());
        // admin with a valid token bound to its own uid: authorized.
        let t = token_for(&kp, "boss", 4242, 120);
        assert_eq!(authorize_elevation(Some(4242), Some(&t)).unwrap(), "boss");
        // same admin, no token: refused (fail closed).
        assert!(authorize_elevation(Some(4242), None).is_err());
        // token presented by a different uid (audience mismatch): refused.
        assert!(authorize_elevation(Some(9999), Some(&t)).is_err());
        // expired token: refused.
        let expired = token_for(&kp, "boss", 4242, -10);
        assert!(authorize_elevation(Some(4242), Some(&expired)).is_err());
        // non-admin with a valid token: refused (not permitted to elevate).
        let pt = token_for(&kp, "peon", 4343, 120);
        assert!(authorize_elevation(Some(4343), Some(&pt)).is_err());
        // subject mismatch: token sub != the calling uid's account.
        let mismatch = token_for(&kp, "peon", 4242, 120);
        assert!(authorize_elevation(Some(4242), Some(&mismatch)).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
