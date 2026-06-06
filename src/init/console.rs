//! Console session. Until the full greeter + Rook Guard login flow is wired up,
//! Rev brings up an interactive root shell on the system console (/dev/console)
//! so the booted system is usable. It respawns the shell if it exits, and never
//! blocks Rev's main loop.
//!
//! This is a development console, not the final login path: real per-user login
//! (authenticate via UAC/Rook Guard, then start_session with dropped privileges)
//! replaces the auto-root shell. See session::start_session.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

const SHELL: &str = "/Core/Bin/brush";

/// Spawn a background supervisor that keeps an interactive shell alive on the
/// console. Uses kill(pid, 0) polling to detect exit so it does not fight the
/// global SIGCHLD zombie reaper over waitpid.
pub fn spawn_console() {
    thread::spawn(|| loop {
        match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
                // Poll until the shell exits; the reaper does the actual reaping.
                loop {
                    thread::sleep(Duration::from_millis(800));
                    // ESRCH => process gone.
                    if nix::sys::signal::kill(child, None).is_err() {
                        break;
                    }
                }
                // Brief pause before respawn to avoid a tight loop on instant exit.
                thread::sleep(Duration::from_millis(300));
            }
            Ok(nix::unistd::ForkResult::Child) => {
                console_child();
                // console_child execs; only reached on failure.
                std::process::exit(127);
            }
            Err(e) => {
                eprintln!("rev: console fork failed: {}", e);
                thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

/// Child side: become a session leader owning /dev/console, wire up stdio, set a
/// minimal root environment, and exec the shell.
fn console_child() {
    // New session so the console becomes our controlling terminal.
    let _ = nix::unistd::setsid();

    // Open the console read/write and make it our controlling tty.
    if let Ok(fd) = nix::fcntl::open(
        "/dev/console",
        nix::fcntl::OFlag::O_RDWR,
        nix::sys::stat::Mode::empty(),
    ) {
        let raw = fd.as_raw_fd();
        // TIOCSCTTY = 0x540E: acquire the controlling terminal.
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

    // Minimal root environment. Safe here: this is the forked child, single
    // threaded, before exec.
    unsafe {
        std::env::set_var("HOME", "/Space");
        std::env::set_var("PATH", "/Core/Bin");
        std::env::set_var("USER", "root");
        std::env::set_var("LOGNAME", "root");
        std::env::set_var("SHELL", SHELL);
        std::env::set_var("TERM", "linux");
    }

    let prog = CString::new(SHELL).unwrap();
    let arg0 = CString::new(SHELL).unwrap();
    let argi = CString::new("-i").unwrap();
    let _ = nix::unistd::execv(&prog, &[arg0, argi]);
}
