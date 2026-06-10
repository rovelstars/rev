//! `rev install` -- validate a .rsc service file and place it where rev looks.
//!
//! System services go to /Construct/Services (the mutable layer over the
//! immutable /Core); per-user services go to the account's vault at
//! /Vault/Services/<uuid>, never a home dotfolder. The file is parsed first so a
//! broken unit is rejected before it is installed.

use crate::parser::{deserialize_service_config, serialize_service_config, ServiceScope};
use std::path::PathBuf;

pub fn run(file: &str, user: bool) {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rev: cannot read {}: {}", file, e);
            std::process::exit(1);
        }
    };
    let mut config = match deserialize_service_config(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rev: {} is not a valid service file: {}", file, e);
            std::process::exit(1);
        }
    };
    if config.name.trim().is_empty() || config.exec_start.trim().is_empty() {
        eprintln!("rev: service needs a name and an exec-start");
        std::process::exit(1);
    }

    let dest_dir = if user {
        // Installed per-user: force user scope (a user install is always a user
        // service; its identity is the installing user regardless of any user=
        // field, which is meaningless here) and place it in the account vault.
        config.scope = ServiceScope::User;
        user_services_dir()
    } else {
        // Installed system-wide in the mutable layer.
        config.scope = ServiceScope::System;
        if cfg!(debug_assertions) {
            PathBuf::from("./Construct/Services")
        } else {
            PathBuf::from("/Construct/Services")
        }
    };

    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        eprintln!(
            "rev: cannot create {} ({}). A per-user vault directory is root-provisioned; \
             a system install needs root.",
            dest_dir.display(),
            e
        );
        std::process::exit(1);
    }

    let dest = dest_dir.join(format!("{}.rsc", config.name));
    let out = serialize_service_config(&config).expect("serialize service config");
    if let Err(e) = std::fs::write(&dest, out) {
        eprintln!("rev: cannot write {}: {}", dest.display(), e);
        std::process::exit(1);
    }

    let scope = if user { "user" } else { "system" };
    println!("rev: installed {} service '{}' to {}", scope, config.name, dest.display());
}

/// The installing user's vault service directory, keyed by their UAC account
/// UUID. Falls back to the uid when UAC cannot resolve the caller (e.g. dev).
fn user_services_dir() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    let key = uac_core::Uac::open()
        .ok()
        .and_then(|u| u.name_by_uid(uid).ok().flatten())
        .and_then(|name| uac_core::Uac::open().ok().and_then(|u| u.get(&name).ok()))
        .map(|acct| acct.uuid)
        .unwrap_or_else(|| uid.to_string());
    crate::bus::lanes::user_service_dir(&key)
}
