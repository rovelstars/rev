//! Console session. On boot Rev runs out-of-box setup (OOBE) if no human
//! account exists yet, then brings up an interactive login session on the system
//! console (/dev/console) as that account - dropping from root to the user's
//! uid/gid. It respawns the session if the shell exits and never blocks Rev's
//! main loop.
//!
//! This is still a dev console (auto-login as the single admin, no password
//! prompt yet); the full greeter + Rook Guard authentication replaces the
//! auto-login later. See session::start_session.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

const SHELL: &str = "/Core/Bin/brush";
const OOBE: &str = "/Core/Bin/oobe";
const PASSWD: &str = "/Vault/Accounts/passwd";

/// A login target resolved from the UAC passwd projection.
struct LoginUser {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
}

/// True if no human (uid >= 1000) account exists yet.
fn needs_oobe() -> bool {
    match std::fs::read_to_string(PASSWD) {
        Ok(c) => !c.lines().any(|l| {
            l.split(':').nth(2).and_then(|u| u.parse::<u32>().ok()).is_some_and(|u| u >= 1000)
        }),
        Err(_) => true,
    }
}

/// First human account (uid >= 1000) from the passwd projection.
fn first_user() -> Option<LoginUser> {
    let c = std::fs::read_to_string(PASSWD).ok()?;
    for line in c.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 7 {
            if let Ok(uid) = f[2].parse::<u32>() {
                if uid >= 1000 {
                    return Some(LoginUser {
                        name: f[0].to_string(),
                        uid,
                        gid: f[3].parse().unwrap_or(uid),
                        home: f[5].to_string(),
                        shell: f[6].to_string(),
                    });
                }
            }
        }
    }
    None
}

/// Make /dev/console our controlling terminal and wire it to stdio. Must run as
/// root (the console is root-owned) before any privilege drop.
fn take_console() {
    let _ = nix::unistd::setsid();
    if let Ok(fd) = nix::fcntl::open(
        "/dev/console",
        nix::fcntl::OFlag::O_RDWR,
        nix::sys::stat::Mode::empty(),
    ) {
        let raw = fd.as_raw_fd();
        unsafe {
            libc::ioctl(raw, libc::TIOCSCTTY, 0);
            libc::dup2(raw, libc::STDIN_FILENO);
            libc::dup2(raw, libc::STDOUT_FILENO);
            libc::dup2(raw, libc::STDERR_FILENO);
            if raw > 2 {
                libc::close(raw);
            }
        }
    }
}

/// Run a forked child that owns the console and execs `prog`, waiting (via
/// kill-poll so it does not fight the zombie reaper) until it exits.
fn run_on_console_blocking(prog: &str) {
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => loop {
            thread::sleep(Duration::from_millis(500));
            if nix::sys::signal::kill(child, None).is_err() {
                break;
            }
        },
        Ok(nix::unistd::ForkResult::Child) => {
            take_console();
            let p = CString::new(prog).unwrap();
            let _ = nix::unistd::execv(&p, &[p.clone()]);
            std::process::exit(127);
        }
        Err(_) => {}
    }
}

/// Spawn the console supervisor: run OOBE once if needed, then keep a login
/// session alive on the console.
pub fn spawn_console() {
    thread::spawn(|| {
        // Out-of-box setup if there is no human account yet.
        if needs_oobe() && std::path::Path::new(OOBE).exists() {
            run_on_console_blocking(OOBE);
        }
        loop {
            match unsafe { nix::unistd::fork() } {
                Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
                    loop {
                        thread::sleep(Duration::from_millis(800));
                        if nix::sys::signal::kill(child, None).is_err() {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(300));
                }
                Ok(nix::unistd::ForkResult::Child) => {
                    login_child();
                    std::process::exit(127);
                }
                Err(e) => {
                    eprintln!("rev: console fork failed: {}", e);
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}

/// Child: own the console, then log in as the first human account (dropping to
/// its uid/gid) and exec its shell. Falls back to a root shell if no account is
/// present (e.g. OOBE was skipped).
fn login_child() {
    take_console();

    let user = first_user();
    let (name, uid, gid, home, shell) = match &user {
        Some(u) => (u.name.as_str(), u.uid, u.gid, u.home.as_str(), u.shell.as_str()),
        None => ("root", 0, 0, "/", SHELL),
    };

    // Drop privileges: supplementary groups, gid, then uid (uid last). Done in
    // the forked, single-threaded child before exec.
    if uid != 0 {
        let g = nix::unistd::Gid::from_raw(gid);
        let _ = nix::unistd::setgroups(&[g]);
        let _ = nix::unistd::setgid(g);
        let _ = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid));
    }

    let _ = nix::unistd::chdir(home).or_else(|_| nix::unistd::chdir("/"));

    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("PATH", "/Core/Bin");
        std::env::set_var("USER", name);
        std::env::set_var("LOGNAME", name);
        std::env::set_var("SHELL", shell);
        std::env::set_var("TERM", "linux");
        std::env::set_var("TMPDIR", "/Transit/Ephemeral");
    }

    let prog = CString::new(shell).unwrap();
    let arg0 = CString::new(shell).unwrap();
    let argi = CString::new("-i").unwrap();
    let _ = nix::unistd::execv(&prog, &[arg0, argi]);
}
