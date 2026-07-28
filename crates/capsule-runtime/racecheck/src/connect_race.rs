//! Race probe: `connect(2)`'s `sockaddr` buffer, against the seccomp-notify supervisor's
//! read-then-`notify_id_valid`-then-`CONTINUE` pattern.
//!
//! Reproduces the exact shape of `sandbox.rs`'s `connect` mediation and asks one question: can a
//! second thread in the notified process swap the destination address between the supervisor's
//! `/proc/<pid>/mem` read and the kernel's post-`CONTINUE` re-use of that same buffer?
//!
//! Layout of a run:
//!
//! - The parent binds two `TcpListener`s on the **same port**, one on `127.0.0.1` (the "allowed"
//!   destination, standing in for a capsule's single `network.allow` entry) and one on
//!   `127.0.0.2` (the "disallowed" destination). Linux routes all of `127.0.0.0/8` to `lo`, which
//!   is what makes two distinct loopback IPs available without any host configuration — and using
//!   two *IPs* rather than two ports means the probe's allow/deny decision is keyed on exactly the
//!   field `read_sockaddr_ip_from_child` + `network_ip_allowed` key on in production.
//! - The child installs a notify filter on `connect`, hands the notify fd back, then runs two
//!   threads. Thread A connects in a loop through a shared 16-byte `sockaddr_in`. Thread B spins,
//!   flipping that buffer's **last address octet** between `1` and `2`. One byte is all it takes,
//!   and a single-byte flip means there is no torn intermediate state to explain away: the buffer
//!   is only ever a valid `127.0.0.1` or a valid `127.0.0.2`, both with a live listener behind
//!   them.
//! - The parent supervises: read the `sockaddr` out of the child, allow iff the IP is `127.0.0.1`,
//!   answer allows with `new_continue`.
//!
//! **What counts as a win.** After each successful `connect`, thread A calls `getpeername(2)` —
//! the kernel's own record of where the connection actually went. A peer of `127.0.0.2` proves the
//! kernel acted on the disallowed address. It cannot be a false positive: had the supervisor read
//! `127.0.0.2`, it would have answered `EACCES` and the `connect` would have failed outright, so
//! every `127.0.0.2` peer is a connection the supervisor *approved* while looking at `127.0.0.1`.
//!
//! **What counts as a loss.** `RACE_WON: 0/N` is a perfectly valid outcome and is not a bug in the
//! probe — it means the window did not open on this kernel, this hardware, in N attempts. Report
//! it as such.

#[cfg(not(target_os = "linux"))]
fn main() {
    racecheck::non_linux_stub("connect_race");
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux::run() {
        eprintln!("connect_race: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use racecheck::linux::{
        close, exit_code, fork, install_notify_filter, parse_sockaddr_ip,
        read_sockaddr_ip_from_child, receive_fd_over_socket, socketpair, supervise, waitpid,
        Decision,
    };
    use racecheck::{iterations_from_args, progress_every};
    use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
    use std::sync::Arc;

    const ALLOWED_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
    const DISALLOWED_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

    /// `sockaddr_in` is 16 bytes on Linux: `sin_family` (2), `sin_port` (2), `sin_addr` (4),
    /// `sin_zero` (8).
    const SOCKADDR_IN_LEN: usize = 16;
    /// Index of the last octet of `sin_addr` — the single byte thread B flips.
    const LAST_OCTET: usize = 7;

    const DEFAULT_ITERATIONS: u64 = 100_000;

    pub fn run() -> Result<(), String> {
        let iterations = iterations_from_args(DEFAULT_ITERATIONS)?;
        let (allowed_listener, disallowed_listener, port) = bind_listener_pair()?;

        println!(
            "connect_race: allowed={ALLOWED_IP}:{port} disallowed={DISALLOWED_IP}:{port} \
             iterations={iterations}"
        );

        let (parent_sock, child_sock) = socketpair().map_err(|e| format!("socketpair: {e}"))?;

        // SAFETY: this process is still single-threaded here — nothing above spawns a thread — so
        // the child inherits no held locks and may allocate and spawn threads of its own freely.
        let pid = unsafe { fork() }.map_err(|e| format!("fork: {e}"))?;
        if pid == 0 {
            close(parent_sock);
            drop(allowed_listener);
            drop(disallowed_listener);
            let code = match child(child_sock, port, iterations) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("connect_race[child]: {error}");
                    1
                }
            };
            std::process::exit(code);
        }
        close(child_sock);
        parent(parent_sock, pid, port, allowed_listener, disallowed_listener)
    }

    /// Binds the allowed/disallowed listener pair on one shared port, retrying until an ephemeral
    /// port happens to be free on both addresses.
    fn bind_listener_pair() -> Result<(TcpListener, TcpListener, u16), String> {
        for _ in 0..64 {
            let allowed = TcpListener::bind((ALLOWED_IP, 0))
                .map_err(|e| format!("bind {ALLOWED_IP}: {e}"))?;
            let port = allowed
                .local_addr()
                .map_err(|e| format!("local_addr: {e}"))?
                .port();
            if let Ok(disallowed) = TcpListener::bind((DISALLOWED_IP, port)) {
                // Rust's `TcpListener::bind` listens with a backlog of 128, which a tight connect
                // loop can outrun. Calling `listen(2)` again on an already-listening socket is how
                // Linux lets a backlog be raised in place.
                for fd in [allowed.as_raw_fd(), disallowed.as_raw_fd()] {
                    // SAFETY: both fds are valid listening sockets owned by this process.
                    unsafe { libc::listen(fd, 4096) };
                }
                return Ok((allowed, disallowed, port));
            }
        }
        Err(format!(
            "could not find a port free on both {ALLOWED_IP} and {DISALLOWED_IP}. \
             {DISALLOWED_IP} must be locally bindable — on Linux the whole 127.0.0.0/8 is \
             normally routed to `lo`; a host that has changed that cannot run this probe"
        ))
    }

    // ---- child: the racing process ------------------------------------------------------------

    fn child(child_sock: RawFd, port: u16, iterations: u64) -> Result<(), String> {
        install_notify_filter(&["connect"], child_sock)
            .map_err(|e| format!("install notify filter: {e}"))?;
        close(child_sock);

        let addr: Arc<[AtomicU8; SOCKADDR_IN_LEN]> = Arc::new(sockaddr_in(ALLOWED_IP, port));
        let stop = Arc::new(AtomicBool::new(false));

        // Thread B — the hostile sibling. It only ever writes one byte, so the buffer thread A
        // hands the kernel is always a well-formed address with a listener behind it.
        //
        // `AtomicU8` rather than a raw `*mut u8` with volatile writes: an `AtomicU8` has the same
        // size and layout as a `u8`, so the array is still a valid `sockaddr_in` to pass to
        // `connect(2)`, but the concurrent write is defined behaviour under Rust's memory model
        // instead of a data race the compiler is entitled to miscompile. The kernel sees exactly
        // the same bytes either way; what changes is that the probe cannot be dismissed as UB.
        let flipper = {
            let addr = Arc::clone(&addr);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    addr[LAST_OCTET].store(DISALLOWED_IP.octets()[3], Ordering::Relaxed);
                    addr[LAST_OCTET].store(ALLOWED_IP.octets()[3], Ordering::Relaxed);
                }
            })
        };

        let mut wins = 0u64;
        let mut reached_allowed = 0u64;
        let mut denied = 0u64;
        let mut other_errors = 0u64;
        let progress = progress_every(iterations);

        for i in 0..iterations {
            // SAFETY: a plain `socket(2)` with constant arguments; the returned fd is checked
            // before use and closed on every path below.
            let fd = unsafe {
                libc::socket(
                    libc::AF_INET,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    libc::IPPROTO_TCP,
                )
            };
            if fd < 0 {
                other_errors += 1;
                continue;
            }

            // SAFETY: `fd` is a fresh AF_INET stream socket; `addr` is a live 16-byte allocation
            // (kept alive by the `Arc` for the whole loop) laid out as a `sockaddr_in`, and 16 is
            // exactly its length. Thread B's concurrent writes into it are atomic.
            let ret = unsafe {
                libc::connect(
                    fd,
                    addr.as_ptr() as *const libc::sockaddr,
                    SOCKADDR_IN_LEN as libc::socklen_t,
                )
            };

            if ret == 0 {
                match peer_ip(fd) {
                    Some(IpAddr::V4(ip)) if ip == DISALLOWED_IP => wins += 1,
                    Some(IpAddr::V4(ip)) if ip == ALLOWED_IP => reached_allowed += 1,
                    _ => other_errors += 1,
                }
            } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EACCES) {
                denied += 1;
            } else {
                other_errors += 1;
            }
            close(fd);

            if (i + 1) % progress == 0 {
                println!(
                    "connect_race: {}/{iterations} attempts, {wins} win(s) so far",
                    i + 1
                );
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = flipper.join();

        println!("RACE_WON: {wins}/{iterations}");
        println!(
            "connect_race: detail: reached-disallowed={wins} reached-allowed={reached_allowed} \
             denied-EACCES={denied} other-errors={other_errors}"
        );
        if other_errors > 0 {
            println!(
                "connect_race: note: `other-errors` is usually ephemeral-port or backlog \
                 pressure. If it dominates, lower --iterations or set \
                 `sysctl -w net.ipv4.tcp_tw_reuse=1`."
            );
        }
        Ok(())
    }

    /// A `sockaddr_in` for `ip:port`, as a byte array of `AtomicU8`.
    fn sockaddr_in(ip: Ipv4Addr, port: u16) -> [AtomicU8; SOCKADDR_IN_LEN] {
        let mut bytes = [0u8; SOCKADDR_IN_LEN];
        // `sin_family` is host byte order; `sin_port` is network byte order.
        bytes[0..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
        bytes[2..4].copy_from_slice(&port.to_be_bytes());
        bytes[4..8].copy_from_slice(&ip.octets());
        std::array::from_fn(|i| AtomicU8::new(bytes[i]))
    }

    /// The kernel's own record of where a connected socket actually went.
    fn peer_ip(fd: RawFd) -> Option<IpAddr> {
        let mut buf = [0u8; 28];
        let mut len = buf.len() as libc::socklen_t;
        // SAFETY: `buf`/`len` are live stack locals; `getpeername` writes at most `len` bytes into
        // `buf` and updates `len` in place.
        let ret = unsafe { libc::getpeername(fd, buf.as_mut_ptr() as *mut libc::sockaddr, &mut len) };
        if ret < 0 {
            return None;
        }
        parse_sockaddr_ip(&buf[..(len as usize).min(buf.len())])
    }

    // ---- parent: the supervisor ---------------------------------------------------------------

    fn parent(
        parent_sock: RawFd,
        pid: libc::pid_t,
        port: u16,
        allowed_listener: TcpListener,
        disallowed_listener: TcpListener,
    ) -> Result<(), String> {
        let notify_fd =
            receive_fd_over_socket(parent_sock).map_err(|e| format!("receive notify fd: {e}"))?;
        close(parent_sock);

        // Drain threads exist to keep the accept queues from filling under a tight connect loop.
        // Their counts corroborate the child's `getpeername` verdict but are only a lower bound:
        // a connection reset before `accept(2)` reaches it is dropped from the queue and never
        // counted. The child's count is the authoritative one.
        let stop = Arc::new(AtomicBool::new(false));
        let allowed_accepts = Arc::new(AtomicU64::new(0));
        let disallowed_accepts = Arc::new(AtomicU64::new(0));
        let drains = [
            drain(allowed_listener, Arc::clone(&allowed_accepts), Arc::clone(&stop)),
            drain(
                disallowed_listener,
                Arc::clone(&disallowed_accepts),
                Arc::clone(&stop),
            ),
        ];

        // The audited pattern, verbatim: read the pointed-to `sockaddr` out of the notifying task,
        // decide from what was read, then `CONTINUE`. Mirrors `classify_and_decide`'s `connect`
        // arm plus `network_ip_allowed` against a one-entry allowlist.
        let stats = supervise(notify_fd, |req| {
            match read_sockaddr_ip_from_child(req.pid, req.data.args[1]) {
                Ok(Some(ip)) => {
                    if ip == IpAddr::V4(ALLOWED_IP) {
                        Decision::Allow
                    } else {
                        Decision::Deny
                    }
                }
                // Non-IP family — outside this layer's scope, exactly as in production.
                Ok(None) => Decision::Allow,
                Err(_) => Decision::Deny,
            }
        });

        let status = waitpid(pid).map_err(|e| format!("waitpid: {e}"))?;

        // Unblock both `accept(2)` calls so the drain threads can observe `stop` and exit.
        stop.store(true, Ordering::Release);
        let _ = TcpStream::connect((ALLOWED_IP, port));
        let _ = TcpStream::connect((DISALLOWED_IP, port));
        for handle in drains {
            let _ = handle.join();
        }

        println!(
            "connect_race: supervisor: allowed={} denied={} stale-id={}",
            stats.allowed, stats.denied, stats.stale
        );
        println!(
            "connect_race: parent accepts (lower bound): allowed={} disallowed={}",
            allowed_accepts.load(Ordering::Relaxed),
            disallowed_accepts.load(Ordering::Relaxed)
        );
        match exit_code(status) {
            Some(0) => Ok(()),
            Some(code) => Err(format!("child exited with status {code}")),
            None => Err("child was killed by a signal".to_string()),
        }
    }

    fn drain(
        listener: TcpListener,
        count: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            // `accept` returning `Err` ends the thread: the listener is gone and there is nothing
            // left to drain. The `stop` check comes first so the wake-up connection the parent
            // makes to unblock this `accept` is not counted as a real hit.
            while listener.accept().is_ok() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                count.fetch_add(1, Ordering::Relaxed);
            }
        })
    }
}
