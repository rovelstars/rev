//! SCM_RIGHTS file descriptor passing over Unix sockets.
//!
//! This is the mechanism that allows Rev (PID 1, running as root) to open
//! restricted device nodes and pass the file descriptors to unprivileged
//! compositor processes without those processes needing root access.
//!
//! Uses `nix::sys::socket::{sendmsg, recvmsg}` for safe ancillary data
//! handling instead of raw libc CMSG macros.
//!
//! Language-agnostic: any language with Unix socket + cmsg support can
//! receive these FDs (C, Rust, Go, Python, etc).

#![allow(dead_code)]

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use nix::sys::socket::{
    self, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr,
};

/// Send one or more file descriptors over a Unix socket using SCM_RIGHTS.
///
/// `socket_fd` must be a connected Unix domain socket (SOCK_STREAM).
/// `fds` is a slice of file descriptors to pass to the other end.
///
/// A single byte of regular data (0x01) is sent alongside — the kernel
/// requires at least one byte of normal data to carry ancillary data.
/// The receiver should read and discard this byte.
pub fn send_fds(socket_fd: RawFd, fds: &[RawFd]) -> io::Result<()> {
    // At least one byte of regular data is required to carry ancillary data
    let data_byte: [u8; 1] = [0x01];
    let iov = [io::IoSlice::new(&data_byte)];

    // Build the SCM_RIGHTS control message
    let cmsg = [ControlMessage::ScmRights(fds)];

    socket::sendmsg::<UnixAddr>(socket_fd, &iov, &cmsg, MsgFlags::empty(), None)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

    Ok(())
}

/// Convenience wrapper: send a single file descriptor.
pub fn send_fd(socket_fd: RawFd, fd: RawFd) -> io::Result<()> {
    send_fds(socket_fd, &[fd])
}

/// Receive file descriptors from a Unix socket using SCM_RIGHTS.
///
/// Returns a tuple of (bytes_read, received_fds). The caller takes
/// ownership of the FDs and is responsible for closing them.
///
/// The ancillary buffer is sized for up to 8 file descriptors per message.
pub fn recv_fds(socket_fd: RawFd) -> io::Result<Vec<RawFd>> {
    let mut data_buf = [0u8; 1];
    let mut iov = [io::IoSliceMut::new(&mut data_buf)];

    // Allocate space for ancillary data (up to 8 FDs)
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 8]);

    let msg = socket::recvmsg::<UnixAddr>(
        socket_fd,
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )
    .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

    let mut received_fds = Vec::new();

    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            received_fds.extend_from_slice(&fds);
        }
    }

    if received_fds.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "no file descriptors received in SCM_RIGHTS message",
        ));
    }

    Ok(received_fds)
}

/// Convenience wrapper: receive a single file descriptor.
pub fn recv_fd(socket_fd: RawFd) -> io::Result<RawFd> {
    let fds = recv_fds(socket_fd)?;
    fds.into_iter().next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "no file descriptor received")
    })
}

/// Send a file descriptor using a tokio UnixStream.
///
/// This temporarily accesses the raw fd from the tokio socket.
/// Must be called from within an async context, but the actual send
/// is synchronous (FD passing must happen at the kernel level via sendmsg).
///
/// IMPORTANT: The caller should ensure the socket is writable before calling.
/// In practice, call this right after writing the WireBus Ok response frame.
pub fn send_fd_over_stream<S: AsRawFd>(stream: &S, fd: RawFd) -> io::Result<()> {
    send_fd(stream.as_raw_fd(), fd)
}

/// Receive a file descriptor using a tokio UnixStream.
///
/// Synchronous recvmsg under the hood — call after reading the WireBus
/// Ok response to OpenDevice.
pub fn recv_fd_from_stream<S: AsRawFd>(stream: &S) -> io::Result<RawFd> {
    recv_fd(stream.as_raw_fd())
}
