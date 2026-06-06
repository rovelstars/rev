//! Early pseudo-filesystem mounts. As PID 1 Rev must mount the kernel
//! pseudo-filesystems before anything else needs them (the kernel only
//! auto-mounts devtmpfs on /dev when configured). Done via the mount(2) syscall
//! directly - there is no `mount` binary in a minimal RunixOS image.

use nix::mount::{mount, MsFlags};
use std::path::Path;

struct Pfs {
    source: &'static str,
    target: &'static str,
    fstype: &'static str,
    flags: MsFlags,
    data: Option<&'static str>,
}

fn already_mounted(target: &str) -> bool {
    if let Ok(m) = std::fs::read_to_string("/proc/mounts") {
        return m.lines().any(|l| {
            let mut it = l.split_whitespace();
            it.next();
            it.next() == Some(target)
        });
    }
    false
}

/// Mount /proc, /sys, /dev (+pts, shm), /run and /tmp. Best-effort: a mount that
/// is already present (e.g. devtmpfs auto-mounted by the kernel) is skipped, and
/// individual failures are logged but do not abort boot.
pub fn early_mounts() {
    let nodev_noexec_nosuid =
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV;

    let table = [
        Pfs {
            source: "proc",
            target: "/proc",
            fstype: "proc",
            flags: nodev_noexec_nosuid,
            data: None,
        },
        Pfs {
            source: "sysfs",
            target: "/sys",
            fstype: "sysfs",
            flags: nodev_noexec_nosuid,
            data: None,
        },
        Pfs {
            source: "devtmpfs",
            target: "/dev",
            fstype: "devtmpfs",
            flags: MsFlags::MS_NOSUID,
            data: Some("mode=0755"),
        },
        Pfs {
            source: "tmpfs",
            target: "/run",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=0755"),
        },
        Pfs {
            source: "tmpfs",
            target: "/tmp",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=1777"),
        },
    ];

    for p in &table {
        let _ = std::fs::create_dir_all(p.target);
        if already_mounted(p.target) {
            continue;
        }
        match mount(
            Some(p.source),
            p.target,
            Some(p.fstype),
            p.flags,
            p.data,
        ) {
            Ok(()) => println!("rev: mounted {} on {}", p.fstype, p.target),
            Err(e) => eprintln!("rev: mount {} on {} failed: {}", p.fstype, p.target, e),
        }
    }

    // /dev/pts (needed for pseudo-terminals: sessions, terminal emulators) and
    // /dev/shm (POSIX shared memory). Only after /dev exists.
    if Path::new("/dev").exists() {
        let _ = std::fs::create_dir_all("/dev/pts");
        if !already_mounted("/dev/pts") {
            let _ = mount(
                Some("devpts"),
                "/dev/pts",
                Some("devpts"),
                MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
                Some("mode=0620,ptmxmode=0666"),
            );
        }
        let _ = std::fs::create_dir_all("/dev/shm");
        if !already_mounted("/dev/shm") {
            let _ = mount(
                Some("tmpfs"),
                "/dev/shm",
                Some("tmpfs"),
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
                Some("mode=1777"),
            );
        }
    }
}
