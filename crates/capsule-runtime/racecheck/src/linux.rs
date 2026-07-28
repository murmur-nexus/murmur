//! The Linux-only half of the probe scaffolding.
//!
//! **Never compiled on this repo's macOS dev machines** — `libseccomp` needs a Linux `cc`/sysroot
//! and cross-compiling is blocked by a transitive `ring` dependency in the workspace. Everything
//! here was written by reading `crates/capsule-runtime/src/sandbox.rs` and the `libseccomp` 0.4.0
//! crate source side by side; it is verified by review, not by a build on the machine that wrote
//! it. Build and run it on a real Linux host.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::RawFd;
use std::os::unix::fs::FileExt;

/// `AF_INET`/`AF_INET6` as Linux's ABI defines them, spelled as literals for the same reason
/// `sandbox.rs` spells `LINUX_AF_*` that way: the numbers must be Linux's regardless of host.
const LINUX_AF_INET: u16 = 2;
const LINUX_AF_INET6: u16 = 10;

/// Mirrors `sandbox.rs::linux_enforce::Decision`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Allow,
    Deny,
}

/// What the supervisor thread did, so a run can distinguish "the race was never even reached"
/// from "the race was reached and lost".
#[derive(Clone, Copy, Debug, Default)]
pub struct SupervisorStats {
    /// Notifications answered with `SECCOMP_USER_NOTIF_FLAG_CONTINUE`.
    pub allowed: u64,
    /// Notifications answered with `EACCES`.
    pub denied: u64,
    /// Notifications dropped because `notify_id_valid` reported the id stale between the
    /// argument read and the response.
    pub stale: u64,
}

/// Builds a notify-only seccomp filter for `syscalls` and hands its notify fd to the parent over
/// `child_sock_fd`. Call from the forked child, before it spawns any thread.
///
/// **Deliberate divergence from `install_seccomp_filter`:** the default action here is `Allow`,
/// not `Errno(EPERM)`. The probe is not a sandbox — it exists to observe one syscall's
/// notify→read→continue round trip, and a default-deny would just make the probe process itself
/// unable to run. Nothing about the default action changes the behaviour under test: the race
/// lives entirely in the `Notify` rules, which are byte-for-byte the same construction
/// (`ScmpFilterContext` + `ScmpAction::Notify` + `load()` + `get_notify_fd()`) the real filter
/// uses.
///
/// The `get_notify_fd()`-only-valid-after-`load()` ordering, and the fact that dropping the
/// `ScmpFilterContext` afterwards does not invalidate the fd, are both as documented in
/// `install_seccomp_filter`.
pub fn install_notify_filter(syscalls: &[&str], child_sock_fd: RawFd) -> io::Result<()> {
    let mut filter = libseccomp::ScmpFilterContext::new(libseccomp::ScmpAction::Allow)
        .map_err(to_io_err)?;

    for name in syscalls {
        let syscall = libseccomp::ScmpSyscall::from_name(name).map_err(to_io_err)?;
        filter
            .add_rule(libseccomp::ScmpAction::Notify, syscall)
            .map_err(to_io_err)?;
    }

    filter.load().map_err(to_io_err)?;
    let notify_fd: RawFd = filter.get_notify_fd().map_err(to_io_err)?;
    send_fd_over_socket(child_sock_fd, notify_fd)
}

/// Mirrors `sandbox.rs::linux_enforce::supervisor_loop` exactly — including the ordering that is
/// the subject of this audit:
///
///   1. receive the notification,
///   2. run `decide`, which reads the pointed-to argument out of `/proc/<pid>/mem`,
///   3. `notify_id_valid` (a *liveness* check — it does not re-validate the bytes just read),
///   4. respond, with `new_continue` for an allow.
///
/// Step 4's `new_continue` is what hands the kernel the job of re-dereferencing the argument
/// pointer after the supervisor has already decided. `libseccomp` 0.4.0's `ScmpNotifResp::
/// new_continue` unconditionally ORs `ScmpNotifRespFlags::CONTINUE` into the response flags
/// (`notify.rs:351-358`), so passing `empty()` here still yields
/// `SECCOMP_USER_NOTIF_FLAG_CONTINUE`.
///
/// Returns once the notify fd errors/EOFs, which happens when every process holding the filter
/// has exited.
pub fn supervise<F>(notify_fd: RawFd, mut decide: F) -> SupervisorStats
where
    F: FnMut(&libseccomp::ScmpNotifReq) -> Decision,
{
    let mut stats = SupervisorStats::default();
    loop {
        let req = match libseccomp::ScmpNotifReq::receive(notify_fd) {
            Ok(req) => req,
            Err(_) => break,
        };

        let decision = decide(&req);

        if libseccomp::notify_id_valid(notify_fd, req.id).is_err() {
            stats.stale += 1;
            continue;
        }

        let resp = match decision {
            Decision::Allow => {
                stats.allowed += 1;
                libseccomp::ScmpNotifResp::new_continue(
                    req.id,
                    libseccomp::ScmpNotifRespFlags::empty(),
                )
            }
            Decision::Deny => {
                stats.denied += 1;
                libseccomp::ScmpNotifResp::new_error(
                    req.id,
                    -libc::EACCES,
                    libseccomp::ScmpNotifRespFlags::empty(),
                )
            }
        };
        let _ = resp.respond(notify_fd);
    }
    stats
}

/// Mirrors `sandbox.rs::linux_enforce::read_cstr_from_child`, including the 4096-then-256 retry.
///
/// This is the read the `seccomp_unotify(2)` man page warns about: `/proc/<pid>/mem` and
/// `process_vm_readv(2)` are the two spellings of the same primitive, and both give the supervisor
/// a snapshot the kernel is under no obligation to still honour after `CONTINUE`.
pub fn read_cstr_from_child(pid: u32, addr: u64) -> io::Result<String> {
    let file = std::fs::File::open(format!("/proc/{pid}/mem"))?;

    for len in [4096usize, 256] {
        let mut buf = vec![0u8; len];
        match file.read_at(&mut buf, addr) {
            Ok(n) if n > 0 => {
                let nul_pos = buf[..n].iter().position(|&b| b == 0).unwrap_or(n);
                return Ok(String::from_utf8_lossy(&buf[..nul_pos]).into_owned());
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }

    Err(io::Error::other(format!(
        "failed to read pathname from /proc/{pid}/mem at {addr:#x}"
    )))
}

/// Mirrors `sandbox.rs::linux_enforce::read_sockaddr_ip_from_child`.
pub fn read_sockaddr_ip_from_child(pid: u32, addr: u64) -> io::Result<Option<IpAddr>> {
    let file = std::fs::File::open(format!("/proc/{pid}/mem"))?;
    // sizeof(sockaddr_in6) is the largest layout parsed.
    let mut buf = [0u8; 28];
    let n = file.read_at(&mut buf, addr)?;
    Ok(parse_sockaddr_ip(&buf[..n]))
}

/// Mirrors `sandbox.rs::parse_sockaddr_ip`: destination IP out of a raw `sockaddr`, or `None` for
/// an address family this layer does not govern.
pub fn parse_sockaddr_ip(buf: &[u8]) -> Option<IpAddr> {
    if buf.len() < 8 {
        return None;
    }
    let family = u16::from_ne_bytes([buf[0], buf[1]]);
    match family {
        LINUX_AF_INET => {
            let octets: [u8; 4] = buf[4..8].try_into().ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(octets)))
        }
        LINUX_AF_INET6 => {
            if buf.len() < 24 {
                return None;
            }
            let octets: [u8; 16] = buf[8..24].try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

/// A `SOCK_STREAM` `AF_UNIX` socketpair, for handing the notify fd from child to parent — the
/// same channel `sandbox.rs` uses between its `pre_exec` closure and `start_supervisor`.
pub fn socketpair() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [-1 as RawFd; 2];
    // SAFETY: `fds` is a live, correctly sized stack array; `socketpair` writes exactly two fds
    // into it and touches nothing else.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// Mirrors `sandbox.rs::linux_enforce::send_fd_over_socket`.
pub fn send_fd_over_socket(sock_fd: RawFd, fd_to_send: RawFd) -> io::Result<()> {
    // SAFETY: `sock_fd` and `fd_to_send` are both valid, open fds owned by this process. All
    // buffers are stack-allocated locals that outlive the single `sendmsg` call using them.
    unsafe {
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };

        let mut cmsg_buf = [0u8; 128];
        let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;

        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space as _;

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd_to_send);

        let ret = libc::sendmsg(sock_fd, &msg, 0);
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Mirrors `sandbox.rs::linux_enforce::receive_fd_over_socket`.
pub fn receive_fd_over_socket(sock_fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: `sock_fd` is a valid, open fd; buffers are stack-allocated and sized for exactly
    // one fd's worth of ancillary data.
    unsafe {
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };

        let mut cmsg_buf = [0u8; 128];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len() as _;

        let ret = libc::recvmsg(sock_fd, &mut msg, libc::MSG_CMSG_CLOEXEC);
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other(
                "recvmsg did not return an SCM_RIGHTS control message",
            ));
        }

        let fd_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
        Ok(std::ptr::read_unaligned(fd_ptr))
    }
}

/// `close(2)` on a raw fd, ignoring failure — every call site here is dropping an fd it is done
/// with, and there is nothing useful to do about `EBADF` in a probe.
pub fn close(fd: RawFd) {
    // SAFETY: callers pass an fd this process owns and will not use again.
    unsafe {
        libc::close(fd);
    }
}

/// `fork(2)`, as a checked wrapper. Returns `Ok(0)` in the child.
///
/// # Safety
///
/// The caller must be single-threaded at the point of the call, or must guarantee the child does
/// nothing but async-signal-safe work before `execve`/`_exit`. Both probes satisfy one or the
/// other, and each `fork` call site says which.
pub unsafe fn fork() -> io::Result<libc::pid_t> {
    // SAFETY: forwarded to the caller by this function's own `# Safety` contract.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Blocking `waitpid(2)`, returning the raw status word.
pub fn waitpid(pid: libc::pid_t) -> io::Result<libc::c_int> {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a live stack local; `waitpid` writes only through that pointer.
    let ret = unsafe { libc::waitpid(pid, &mut status, 0) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status)
}

/// The exit code from a `waitpid` status word, or `None` if the process was signalled.
pub fn exit_code(status: libc::c_int) -> Option<libc::c_int> {
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    }
}

fn to_io_err<E: std::fmt::Display>(error: E) -> io::Error {
    io::Error::other(error.to_string())
}
