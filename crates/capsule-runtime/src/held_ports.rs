//! The ports a session's egress proxy holds, and the line a shell result carries when a process in
//! the session tried to bind one.
//!
//! The session's network namespace binds one TCP listener per port its allowlist implies
//! ([`crate::egress_proxy::egress_listen_ports`]) on `INADDR_ANY`, for the whole session, so a
//! server a command starts on one of those ports gets `EADDRINUSE`. That rule stays. This module
//! makes it legible: on the Landlock tiers the per-command seccomp filter routes `bind(2)` to a
//! user notification, and the session's [`BindObserver`] reads the `sockaddr` the process passed,
//! records the port when it is one the proxy holds, and always answers "continue". The kernel
//! performs every bind exactly as it would without the observer; the observer decides nothing.
//!
//! A record is only a port. The next finished shell result of the session drains the pending
//! records into one [`held_port_line`] per port, after the fence, as a runtime note.
//!
//! [`HeldPorts`], [`held_port_line`] and [`HeldPortRecords`] compile everywhere; the observer's
//! thread exists on Linux only.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use crate::network_policy::NetworkAllowRule;

/// Every port the session's egress proxy holds, each mapped to the `capabilities.network.allow`
/// entries that imply it, in manifest order and quoted as the operator wrote them (trimmed).
///
/// A bare host implies 80 and 443, an `http(s)://` entry its explicit port or its scheme's
/// default, and `host:port` its port — the mapping [`crate::egress_proxy::egress_listen_ports`]
/// uses, whose result (truncation included) is the held set this is built over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HeldPorts {
    by_port: BTreeMap<u16, Vec<String>>,
}

impl HeldPorts {
    /// The map for `network_allow` (the raw manifest entries) over `held`, the ports the proxy
    /// actually binds. An entry that does not parse implies nothing; staging refused it already.
    pub(crate) fn from_allowlist(network_allow: &[String], held: &[u16]) -> Self {
        let mut by_port: BTreeMap<u16, Vec<String>> =
            held.iter().map(|&port| (port, Vec::new())).collect();
        for entry in network_allow {
            let Ok(rule) = NetworkAllowRule::parse(entry) else {
                continue;
            };
            let implied: &[u16] = match rule.port {
                Some(ref port) => std::slice::from_ref(port),
                None => &[80, 443],
            };
            let quoted = entry.trim();
            for port in implied {
                if let Some(entries) = by_port.get_mut(port) {
                    if !entries.iter().any(|known| known == quoted) {
                        entries.push(quoted.to_string());
                    }
                }
            }
        }
        by_port.retain(|_, entries| !entries.is_empty());
        Self { by_port }
    }

    /// Whether the proxy holds no port at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_port.is_empty()
    }

    /// Whether the proxy holds `port`.
    pub(crate) fn contains(&self, port: u16) -> bool {
        self.by_port.contains_key(&port)
    }

    /// The held ports, ascending.
    #[cfg(test)]
    pub(crate) fn ports(&self) -> Vec<u16> {
        self.by_port.keys().copied().collect()
    }

    /// The allow entries holding `port`, in manifest order; `None` for a port the proxy does not
    /// hold.
    pub(crate) fn entries(&self, port: u16) -> Option<&[String]> {
        self.by_port.get(&port).map(Vec::as_slice)
    }

    /// [`held_port_line`] for `port`; `None` for a port the proxy does not hold.
    pub(crate) fn line(&self, port: u16) -> Option<String> {
        self.entries(port)
            .map(|entries| held_port_line(port, entries))
    }
}

/// The runtime line naming a held port and the allow entries holding it:
///
/// ```text
/// held_port: 8091 — a process in this session tried to bind port 8091, which this session's egress proxy holds for the capabilities.network.allow entry "127.0.0.1:8091", so no server inside the capsule can listen on it. Serve on a port no allow entry names; the allow entry is for connecting to a server outside the capsule on that port.
/// ```
///
/// With more than one entry the clause reads `entries "a", "b"` and the last sentence speaks of
/// "the allow entries". One line, no trailing newline.
pub(crate) fn held_port_line<S: AsRef<str>>(port: u16, entries: &[S]) -> String {
    let quoted = entries
        .iter()
        .map(|entry| format!("\"{}\"", entry.as_ref()))
        .collect::<Vec<_>>()
        .join(", ");
    let (clause, purpose) = if entries.len() == 1 {
        ("entry", "the allow entry is")
    } else {
        ("entries", "the allow entries are")
    };
    format!(
        "held_port: {port} \u{2014} a process in this session tried to bind port {port}, which \
         this session's egress proxy holds for the capabilities.network.allow {clause} {quoted}, \
         so no server inside the capsule can listen on it. Serve on a port no allow entry names; \
         {purpose} for connecting to a server outside the capsule on that port."
    )
}

/// One session's held-port map and the ports a bind was seen on since the last drain.
///
/// The pending set only ever holds held ports, so it is bounded by the held set (at most
/// [`crate::egress_proxy::MAX_EGRESS_TCP_PORTS`]).
#[derive(Debug)]
pub(crate) struct HeldPortRecords {
    held: HeldPorts,
    pending: Mutex<BTreeSet<u16>>,
}

impl HeldPortRecords {
    pub(crate) fn new(held: HeldPorts) -> Self {
        Self {
            held,
            pending: Mutex::new(BTreeSet::new()),
        }
    }

    /// Records a bind on `port`. A port the proxy does not hold is not recorded; returns whether
    /// it was.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn record(&self, port: u16) -> bool {
        if !self.held.contains(port) {
            return false;
        }
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(port);
        true
    }

    /// Drains every pending record into one [`held_port_line`] per port, ascending.
    pub(crate) fn take_lines(&self) -> Vec<String> {
        let drained =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        drained
            .into_iter()
            .filter_map(|port| self.held.line(port))
            .collect()
    }
}

/// A session's bind observer: its [`HeldPortRecords`] and, on Linux, the one thread that answers
/// every seccomp user notification the session's commands raise for `bind(2)`.
///
/// Owned by [`crate::network_namespace::SessionNetns`], so every enforcement of the session — each
/// tool call's clone, a plan's, a `murmur:process` program's — hands its listeners to the same
/// thread and records into the same pending set. The thread starts with the first listener
/// ([`Self::add_listener`]) and runs until [`Self::stop`].
#[derive(Debug)]
pub(crate) struct BindObserver {
    records: Arc<HeldPortRecords>,
    #[cfg(target_os = "linux")]
    thread: Mutex<linux::ObserverThread>,
}

impl BindObserver {
    /// An observer for `held` with no thread yet.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn new(held: HeldPorts) -> Self {
        Self {
            records: Arc::new(HeldPortRecords::new(held)),
            #[cfg(target_os = "linux")]
            thread: Mutex::new(linux::ObserverThread::NotStarted),
        }
    }

    /// Drains the pending records into lines; see [`HeldPortRecords::take_lines`].
    pub(crate) fn take_lines(&self) -> Vec<String> {
        self.records.take_lines()
    }

    /// Records a bind on `port` without a notification: the seam a test uses to exercise what
    /// reads the records.
    #[cfg(test)]
    pub(crate) fn record_for_test(&self, port: u16) -> bool {
        self.records.record(port)
    }

    /// Hands one command's notification listener to the observer thread, starting it on the
    /// first. The listener is polled until every task under its filter has exited (`POLLHUP`)
    /// or the observer stops, and closed then. After [`Self::stop`] a listener is closed at once.
    #[cfg(target_os = "linux")]
    pub(crate) fn add_listener(&self, listener: std::os::fd::OwnedFd) {
        self.thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add_listener(listener, &self.records);
    }

    /// Stops the observer thread, if it started, and waits for it; its listeners close with it.
    /// Pending records are dropped. Idempotent and final.
    pub(crate) fn stop(&self) {
        #[cfg(target_os = "linux")]
        self.thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stop();
        self.records
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Whether the observer thread is running.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn thread_running(&self) -> bool {
        matches!(
            *self.thread.lock().unwrap_or_else(PoisonError::into_inner),
            linux::ObserverThread::Running(_)
        )
    }
}

/// The observer thread and the read of a notifying process's `sockaddr`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use super::HeldPortRecords;

    /// The most `sockaddr` bytes the observer reads: `sizeof(struct sockaddr_in6)`.
    const SOCKADDR_READ_MAX: usize = 28;
    /// `sizeof(struct sockaddr_in)`, below which the kernel refuses an `AF_INET` bind.
    const SOCKADDR_IN_LEN: usize = 16;
    /// `SIN6_LEN_RFC2133`, below which the kernel refuses an `AF_INET6` bind.
    const SOCKADDR_IN6_MIN_LEN: usize = 24;
    /// How long the thread waits before polling again after `poll` failed with something other
    /// than `EINTR`, so a transient `ENOMEM` neither spins nor drops the listeners.
    const POLL_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

    #[derive(Debug)]
    pub(super) enum ObserverThread {
        NotStarted,
        Running(Running),
        Stopped,
    }

    #[derive(Debug)]
    pub(super) struct Running {
        shared: Arc<Shared>,
        handle: std::thread::JoinHandle<()>,
    }

    /// What the session side and the thread share: listeners waiting to be polled, the stop
    /// flag, and the eventfd that wakes the thread for either.
    #[derive(Debug)]
    struct Shared {
        inbox: Mutex<Vec<OwnedFd>>,
        stop: AtomicBool,
        wake: OwnedFd,
    }

    impl Shared {
        fn wake(&self) {
            let one: u64 = 1;
            // SAFETY: `wake` is the eventfd this struct owns; the buffer is an 8-byte local, the
            // size an eventfd write takes. A failed write (the counter saturated) still leaves the
            // eventfd readable, which is all the thread needs.
            unsafe {
                libc::write(
                    self.wake.as_raw_fd(),
                    std::ptr::addr_of!(one).cast(),
                    std::mem::size_of::<u64>(),
                );
            }
        }
    }

    impl ObserverThread {
        pub(super) fn add_listener(&mut self, listener: OwnedFd, records: &Arc<HeldPortRecords>) {
            match self {
                Self::Running(running) => {
                    running
                        .shared
                        .inbox
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(listener);
                    running.shared.wake();
                }
                Self::NotStarted => match start(listener, Arc::clone(records)) {
                    Ok(running) => *self = Self::Running(running),
                    Err(error) => crate::runtime_err!(
                        "[capsule-runtime] warning: could not start this session's bind \
                         observer: {error}; a bind in that command fails with ENOSYS"
                    ),
                },
                // The session has ended and its processes are being killed; the listener closes
                // here.
                Self::Stopped => drop(listener),
            }
        }

        pub(super) fn stop(&mut self) {
            if let Self::Running(running) = std::mem::replace(self, Self::Stopped) {
                running.shared.stop.store(true, Ordering::SeqCst);
                running.shared.wake();
                let _ = running.handle.join();
            }
        }
    }

    fn start(first: OwnedFd, records: Arc<HeldPortRecords>) -> std::io::Result<Running> {
        // SAFETY: `eventfd` takes two integers and returns a fresh descriptor or -1.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` was just returned by a successful `eventfd` and nothing else owns it.
        let wake = unsafe { OwnedFd::from_raw_fd(raw) };
        let shared = Arc::new(Shared {
            inbox: Mutex::new(vec![first]),
            stop: AtomicBool::new(false),
            wake,
        });
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("mur-bind-observer".to_string())
            .spawn(move || run(&thread_shared, &records))?;
        Ok(Running { shared, handle })
    }

    /// The thread: polls the wake eventfd and every listener, answers each notification, and
    /// drops a listener once every task under its filter has exited.
    fn run(shared: &Shared, records: &HeldPortRecords) {
        let mut listeners: Vec<OwnedFd> = Vec::new();
        loop {
            listeners.append(&mut shared.inbox.lock().unwrap_or_else(PoisonError::into_inner));
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            let mut polled: Vec<libc::pollfd> = std::iter::once(shared.wake.as_raw_fd())
                .chain(listeners.iter().map(AsRawFd::as_raw_fd))
                .map(|fd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // SAFETY: `polled` is a live, correctly sized array of `pollfd`; every descriptor in
            // it is owned by `shared` or `listeners` for the length of the call.
            let ready =
                unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, -1) };
            if ready < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    std::thread::sleep(POLL_RETRY);
                }
                continue;
            }
            if polled[0].revents & libc::POLLIN != 0 {
                let mut counter: u64 = 0;
                // SAFETY: an 8-byte read of the eventfd `shared` owns into a local `u64`.
                unsafe {
                    libc::read(
                        shared.wake.as_raw_fd(),
                        std::ptr::addr_of_mut!(counter).cast(),
                        std::mem::size_of::<u64>(),
                    );
                }
            }
            let mut finished = Vec::new();
            for (index, entry) in polled[1..].iter().enumerate() {
                if entry.revents & libc::POLLIN != 0 {
                    answer_one(listeners[index].as_raw_fd(), records);
                } else if entry.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    finished.push(index);
                }
            }
            for index in finished.into_iter().rev() {
                listeners.swap_remove(index);
            }
        }
    }

    /// Receives one notification from `listener`, records its port when the `sockaddr` names a
    /// held one, and answers continue. Every notification received is answered with continue,
    /// whatever the read found.
    fn answer_one(listener: RawFd, records: &HeldPortRecords) {
        // A notification can vanish between `poll` and the receive (its task was killed); the
        // receive then fails and there is nothing to answer.
        let Ok(request) = libseccomp::ScmpNotifReq::receive(listener) else {
            return;
        };
        let args = request.data.args;
        if let Some(port) = read_bind_port(request.pid, args[1], args[2]) {
            // The pid is only known to name the notifying task while the notification is still
            // pending; a record read from a pid that was reused is discarded here.
            if libseccomp::notify_id_valid(listener, request.id).is_ok() {
                records.record(port);
            }
        }
        let _ = libseccomp::ScmpNotifResp::new_continue(
            request.id,
            libseccomp::ScmpNotifRespFlags::empty(),
        )
        .respond(listener);
    }

    /// The port of the `AF_INET`/`AF_INET6` `sockaddr` of `addrlen` bytes at `address` in `pid`'s
    /// memory, read with `process_vm_readv`. `None` for any other family, a length the kernel
    /// would refuse for the family, or a read that fails.
    ///
    /// The read needs no dumpable target: the runtime's euid owns the session's user namespace,
    /// so it holds `CAP_SYS_PTRACE` in it and in the composed root's namespace nested below it.
    fn read_bind_port(pid: u32, address: u64, addrlen: u64) -> Option<u16> {
        let length = usize::try_from(addrlen).ok()?.min(SOCKADDR_READ_MAX);
        let mut buffer = [0u8; SOCKADDR_READ_MAX];
        let local = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: length,
        };
        let remote = libc::iovec {
            iov_base: address as *mut libc::c_void,
            iov_len: length,
        };
        // SAFETY: `local` points at `buffer`, a live local of at least `length` bytes; `remote`
        // names memory in another process, which the kernel validates and never dereferences in
        // this one.
        let read = unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) };
        if usize::try_from(read).ok()? != length {
            return None;
        }
        port_of_sockaddr(&buffer[..length])
    }

    /// The port of an `AF_INET` or `AF_INET6` `sockaddr` the kernel would accept for a bind.
    pub(super) fn port_of_sockaddr(sockaddr: &[u8]) -> Option<u16> {
        let family = i32::from(u16::from_ne_bytes([*sockaddr.first()?, *sockaddr.get(1)?]));
        let minimum = match family {
            libc::AF_INET => SOCKADDR_IN_LEN,
            libc::AF_INET6 => SOCKADDR_IN6_MIN_LEN,
            _ => return None,
        };
        if sockaddr.len() < minimum {
            return None;
        }
        Some(u16::from_be_bytes([sockaddr[2], sockaddr[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    /// The held set exactly as the session computes it.
    fn held_ports(entries: &[&str]) -> HeldPorts {
        let entries = allow(entries);
        let rules = crate::network_policy::parse_network_allow_rules(&entries).unwrap();
        HeldPorts::from_allowlist(&entries, &crate::egress_proxy::egress_listen_ports(&rules))
    }

    #[test]
    fn held_ports_map_a_host_port_entry_to_its_port() {
        let held = held_ports(&["127.0.0.1:8091"]);
        assert_eq!(held.ports(), vec![8091]);
        assert_eq!(
            held.entries(8091),
            Some(&["127.0.0.1:8091".to_string()][..])
        );
        assert_eq!(held.entries(80), None);
    }

    #[test]
    fn held_ports_map_a_url_entry_to_its_scheme_port_or_its_explicit_port() {
        let held = held_ports(&["https://api.example.com"]);
        assert_eq!(held.ports(), vec![443]);
        assert_eq!(
            held.entries(443),
            Some(&["https://api.example.com".to_string()][..])
        );

        let held = held_ports(&["http://api.example.com:8080"]);
        assert_eq!(held.ports(), vec![8080]);
    }

    #[test]
    fn held_ports_map_a_bare_host_to_80_and_443() {
        let held = held_ports(&["api.example.com"]);
        assert_eq!(held.ports(), vec![80, 443]);
        for port in [80, 443] {
            assert_eq!(
                held.entries(port),
                Some(&["api.example.com".to_string()][..])
            );
        }
    }

    #[test]
    fn held_ports_name_every_entry_sharing_a_port_in_manifest_order() {
        let held = held_ports(&["https://b.example.com", "a.example.com"]);
        assert_eq!(held.ports(), vec![80, 443]);
        assert_eq!(
            held.entries(443),
            Some(
                &[
                    "https://b.example.com".to_string(),
                    "a.example.com".to_string()
                ][..]
            )
        );
        assert_eq!(held.entries(80), Some(&["a.example.com".to_string()][..]));
    }

    #[test]
    fn held_ports_quote_a_padded_entry_trimmed() {
        let held = held_ports(&["  127.0.0.1:8091 \t"]);
        assert_eq!(
            held.entries(8091),
            Some(&["127.0.0.1:8091".to_string()][..])
        );
    }

    #[test]
    fn held_ports_cover_exactly_the_ports_the_proxy_keeps_past_sixteen() {
        let entries: Vec<String> = (9000..9020)
            .map(|port| format!("127.0.0.1:{port}"))
            .collect();
        let rules = crate::network_policy::parse_network_allow_rules(&entries).unwrap();
        let kept = crate::egress_proxy::egress_listen_ports(&rules);
        assert_eq!(kept.len(), crate::egress_proxy::MAX_EGRESS_TCP_PORTS);

        let held = HeldPorts::from_allowlist(&entries, &kept);
        assert_eq!(held.ports(), kept);
        assert!(!held.contains(9019));
        assert_eq!(held.line(9019), None);
    }

    #[test]
    fn held_ports_of_an_empty_allowlist_are_empty() {
        let held = held_ports(&[]);
        assert!(held.is_empty());
        assert_eq!(held, HeldPorts::default());
    }

    #[test]
    fn held_port_line_names_the_port_and_its_one_entry() {
        let line = held_port_line(8091, &["127.0.0.1:8091"]);
        assert_eq!(
            line,
            "held_port: 8091 \u{2014} a process in this session tried to bind port 8091, which \
             this session's egress proxy holds for the capabilities.network.allow entry \
             \"127.0.0.1:8091\", so no server inside the capsule can listen on it. Serve on a \
             port no allow entry names; the allow entry is for connecting to a server outside \
             the capsule on that port."
        );
        assert_eq!(line.lines().count(), 1);
        assert!(line.starts_with("held_port: 8091 — "));
    }

    #[test]
    fn held_port_line_lists_two_entries_in_order() {
        let line = held_port_line(443, &["https://b.example.com", "a.example.com"]);
        assert_eq!(
            line,
            "held_port: 443 \u{2014} a process in this session tried to bind port 443, which \
             this session's egress proxy holds for the capabilities.network.allow entries \
             \"https://b.example.com\", \"a.example.com\", so no server inside the capsule can \
             listen on it. Serve on a port no allow entry names; the allow entries are for \
             connecting to a server outside the capsule on that port."
        );
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn held_port_records_drain_held_ports_once_ascending_and_deduplicated() {
        let records = HeldPortRecords::new(held_ports(&["127.0.0.1:8091", "127.0.0.1:8090"]));
        assert!(records.record(8091));
        assert!(records.record(8090));
        assert!(records.record(8091));
        assert!(
            !records.record(8092),
            "a port nothing holds is never recorded"
        );

        assert_eq!(
            records.take_lines(),
            vec![
                held_port_line(8090, &["127.0.0.1:8090"]),
                held_port_line(8091, &["127.0.0.1:8091"]),
            ]
        );
        assert!(records.take_lines().is_empty());
    }

    #[test]
    fn a_stopped_observer_drops_its_pending_records() {
        let observer = BindObserver::new(held_ports(&["127.0.0.1:8091"]));
        assert!(observer.record_for_test(8091));
        observer.stop();
        assert!(observer.take_lines().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn held_ports_read_the_port_of_inet_sockaddrs_only() {
        // SAFETY: `sockaddr_in`/`sockaddr_in6` are plain C structs for which all-zero is valid.
        let mut v4: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        v4.sin_family = libc::AF_INET as libc::sa_family_t;
        v4.sin_port = 8091u16.to_be();
        // SAFETY: as above.
        let mut v6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        v6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        v6.sin6_port = 8091u16.to_be();
        let bytes = |value: *const u8, len: usize| -> Vec<u8> {
            // SAFETY: `value` points at a live local struct of at least `len` bytes.
            unsafe { std::slice::from_raw_parts(value, len) }.to_vec()
        };
        let v4 = bytes(std::ptr::addr_of!(v4).cast(), std::mem::size_of_val(&v4));
        let v6 = bytes(std::ptr::addr_of!(v6).cast(), std::mem::size_of_val(&v6));

        assert_eq!(linux::port_of_sockaddr(&v4), Some(8091));
        assert_eq!(linux::port_of_sockaddr(&v6), Some(8091));
        assert_eq!(linux::port_of_sockaddr(&v4[..8]), None, "short AF_INET");
        assert_eq!(linux::port_of_sockaddr(&v6[..16]), None, "short AF_INET6");
        let mut unix = v4.clone();
        unix[..2].copy_from_slice(&(libc::AF_UNIX as u16).to_ne_bytes());
        assert_eq!(linux::port_of_sockaddr(&unix), None, "AF_UNIX");
    }
}
