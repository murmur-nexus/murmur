//! The policy half of the capsule network boundary: a connection-level egress proxy and a DNS
//! resolver, both running in the **host's** network namespace and both serving sockets that live
//! in the capsule's.
//!
//! [`crate::network_namespace`] builds a namespace where every address is locally deliverable and
//! hands the listening sockets it created inside it back over `SCM_RIGHTS`. This module is what is
//! on the other end of them. Between the two, `capabilities.network.allow` is enforced
//! structurally: a subprocess cannot reach a destination the runtime did not itself open a
//! connection to.
//!
//! ## The two halves, and why they are two
//!
//! **DNS (UDP 53).** Every name a capsule looks up arrives here. A name in the allowlist is
//! resolved for real, by [`crate::dns_resolver`], and the answer is returned *and remembered* —
//! the address→name binding recorded in [`ResolvedNames`] is what lets the TCP half know which
//! host a later connection is for. A name that is not in the allowlist is answered `REFUSED`: a
//! real reply, immediately, not a dropped packet. That is the whole of "decide DNS deliberately"
//! — and a DNS-shaped exfiltration attempt (`nc -u <resolver> 53` with data in the QNAME)
//! terminates in this process, its payload never leaving the host.
//!
//! Every query gets one of four answers and never silence: `REFUSED` for a name the allowlist does
//! not carry, then `NOERROR`, `NXDOMAIN` or `SERVFAIL` for the resolver's three outcomes. A slow
//! or unreachable upstream is `SERVFAIL`, which a client reads as "ask again", and not `NXDOMAIN`,
//! which would tell it the name does not exist.
//!
//! **TCP.** A connection to any address on an allowlisted port is looped back by the namespace's
//! local route and accepted here, with the original destination recovered from `getsockname(2)`
//! (`TcpStream::local_addr`). The destination is checked, an upstream connection is opened from
//! the host namespace, and bytes are spliced. Nothing is parsed: TLS, HTTP/2, SSH and a raw
//! protocol all work identically, and — this is the point — the runtime never terminates or
//! inspects TLS, so the capsule's end-to-end encryption is untouched and no proxy environment
//! variable has to be honoured by anything.
//!
//! ## Why a name is checked, not just an address
//!
//! The retired seccomp-notify supervisor compared the destination *IP*, read out of the stopped
//! child's `/proc/<pid>/mem`, against a set resolved once at launch. That was weak in both
//! directions — a shared-address CDN made every tenant on that address reachable, and a legitimate
//! host that re-resolved elsewhere became unreachable — and unsound besides, because the kernel
//! re-read the same pointer after the decision was made. Here the ordinary path is name-keyed: the
//! name is checked when the capsule resolves it, the runtime performs that resolution itself, and
//! the resulting address is checked again when a connection to it is accepted.
//!
//! An address the capsule obtained some other way — a hardcoded literal — has no name bound to it
//! and falls back to the launch-time address set, through the same `sandbox::network_ip_allowed`
//! over `sandbox::resolve_network_allowlist_ips` as before. That case is therefore no weaker than
//! it was, and every other case is stronger.
//!
//! ## Deliberate limits, stated rather than hidden
//!
//! * **IPv4 only.** The namespace installs no IPv6 route, so an IPv6 destination fails with
//!   `ENETUNREACH` regardless of what DNS answered. This resolver does not withhold a name's real
//!   `AAAA` records to prevent that — [`answer_dns_query`] answers `AAAA` with no records only when
//!   the name has no IPv6 address upstream at all, so a genuinely dual-stack name's real IPv6
//!   addresses are returned unchanged. A client that tries one of those first pays the cost of a
//!   failed connection before falling back to IPv4, which is served end to end; it is not granted
//!   anything, since the missing route — not the DNS answer — is what makes IPv6 unreachable.
//! * **Only allowlisted ports are listened on.** A connection to a port no allow entry implies
//!   gets `ECONNREFUSED`: the route delivers it locally and nothing is listening. That is a
//!   refusal, not an escape.
//! * **UDP other than DNS goes nowhere.** No UDP socket is bound except 53, so a `sendto`
//!   anywhere else finds nothing. There is deliberately no generic UDP forwarder — nothing in the
//!   manifest schema expresses a UDP allowlist, so forwarding it would grant a capability no
//!   capsule ever declared.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::dns_resolver::Resolution;
use crate::network_policy::{NetworkAllowRule, RequestTarget};

/// The DNS port. Fixed by the protocol, not a choice.
pub(crate) const EGRESS_DNS_PORT: u16 = 53;

/// Ceiling on how many distinct TCP ports one capsule's allowlist opens listeners for.
///
/// Bounds both the `pre_exec` fd array — which has to be a fixed-size stack buffer, see
/// [`crate::network_namespace::MAX_NAMESPACE_SOCKETS`] — and the number of accept threads a
/// session starts. A manifest naming more distinct ports than this is not refused; the ports past
/// the ceiling simply get no listener, which fails closed.
pub(crate) const MAX_EGRESS_TCP_PORTS: usize = 16;

/// TTL, in seconds, on every record this resolver synthesises.
///
/// Short on purpose: the answer a capsule is given is the runtime's own view of the name, and a
/// capsule caching it for long would paper over both an allowlist change taking effect and the
/// upstream record moving. It is not the upstream record's own TTL — `crate::dns_resolver` caches
/// on its own terms behind this.
const DNS_TTL_SECONDS: u32 = 60;

/// How long an address→name binding stays usable.
///
/// Deliberately longer than [`DNS_TTL_SECONDS`]: a client that caches an answer slightly past its
/// TTL must not have the *connection* refused for a name it was legitimately told about, which
/// would present as a random, unreproducible failure rather than as a policy decision.
const NAME_BINDING_LIFETIME: Duration = Duration::from_secs(300);

/// Largest DNS message this resolver will read or produce. 4096 is the EDNS0 buffer size every
/// modern resolver advertises; anything larger is malformed for our purposes.
const MAX_DNS_MESSAGE_BYTES: usize = 4096;

// ---------------------------------------------------------------- listen ports

/// Every TCP port this capsule's allowlist implies a listener for, ascending and deduplicated.
///
/// A rule with an explicit port contributes exactly that port. A rule without one contributes
/// *both* scheme defaults, 80 and 443 — which the manifest schema defines as what a bare host
/// entry spans. Nothing else is opened, so a port no entry implies has no listener and a
/// connection to it is refused by the kernel with nothing in userspace consulted.
///
/// Only `rule.port` is consulted, deliberately: `network_policy::parse_url_allow_rule` already
/// resolves a scheme-bearing entry's port via `default_port_for_scheme` at parse time (an
/// `https://` or `http://` entry always carries `port: Some(_)` by the time it reaches here), so
/// `rule.port == None` only ever happens for a bare host entry, which has no `scheme` either and
/// is exactly the "both defaults" case below. There is no reachable state where `rule.scheme` is
/// `Some(_)` and `rule.port` is `None` — matching on `scheme` here would just re-encode
/// `default_port_for_scheme`'s mapping in a branch nothing can reach.
pub(crate) fn egress_listen_ports(rules: &[NetworkAllowRule]) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    for rule in rules {
        let implied: [Option<u16>; 2] = match rule.port {
            Some(port) => [Some(port), None],
            // A bare `example.com` entry spans both schemes and every port. The two scheme
            // defaults are what a client actually dials; opening every port would be a listener
            // population no allowlist asked for.
            None => [Some(80), Some(443)],
        };
        for port in implied.into_iter().flatten() {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    ports.sort_unstable();
    ports.truncate(MAX_EGRESS_TCP_PORTS);
    ports
}

// ---------------------------------------------------------------- the policy

/// The allow decision for one capsule session.
///
/// Holds both halves deliberately: `rules` answers "is this *name* allowed" (what a DNS query and
/// a name-keyed connection carry), while `allow_ips` answers "is this *address* allowed" (what a
/// capsule that hardcoded a literal asks). Neither subsumes the other — a name has no address
/// until something resolves it, and an address carries no name at all.
#[derive(Debug, Clone)]
pub(crate) struct EgressPolicy {
    rules: Vec<NetworkAllowRule>,
    allow_ips: Vec<IpAddr>,
}

impl EgressPolicy {
    pub(crate) fn new(rules: Vec<NetworkAllowRule>, allow_ips: Vec<IpAddr>) -> Self {
        Self { rules, allow_ips }
    }

    /// Whether the resolver may answer for `name`.
    ///
    /// Port and scheme are deliberately not consulted: a DNS query carries neither, and refusing
    /// to resolve a listed host because the manifest pinned it to `:443` would break the very
    /// request the manifest permits. The connection is gated separately by
    /// [`Self::allows_connection`], which does consult both — so resolving a name is never by
    /// itself a grant to reach it.
    pub(crate) fn allows_name(&self, name: &str) -> bool {
        let name = normalize_host(name);
        if name.parse::<IpAddr>().is_ok() {
            // Not a name at all. Nothing legitimate asks a resolver to resolve an address, and
            // answering would only widen what a query can carry.
            return false;
        }
        self.rules.iter().any(|rule| rule.host == name)
    }

    /// Whether a connection to `address:port` may be forwarded, given every name this session's
    /// resolver has bound to that address.
    ///
    /// Name-keyed when a binding exists, address-keyed when none does. The decision goes through
    /// the very same [`NetworkAllowRule::matches`] — and the very same rule set — that gates the
    /// WASI-HTTP path, so one manifest entry cannot mean two things depending on which half of the
    /// runtime reads it. See [`rule_permits_connection`] for where the scheme it matches on comes
    /// from, and why a connection cannot supply one.
    pub(crate) fn allows_connection(&self, names: &[String], address: IpAddr, port: u16) -> bool {
        for name in names {
            let host = normalize_host(name);
            if self
                .rules
                .iter()
                .any(|rule| rule_permits_connection(rule, &host, port))
            {
                return true;
            }
        }
        // No name: a hardcoded literal, or an address this session's resolver never handed out.
        // Falls back to exactly the check the retired seccomp path performed —
        // `sandbox::network_ip_allowed` over `sandbox::resolve_network_allowlist_ips`'s
        // launch-time resolution — reused rather than reimplemented.
        crate::sandbox::network_ip_allowed(address, &self.allow_ips)
    }
}

/// Whether `rule` permits a TCP connection to `host:port`.
///
/// The scheme handed to [`NetworkAllowRule::matches`] is taken from `rule` itself, which makes the
/// matcher's scheme clause a deliberate no-op and leaves host and port — the only two things a TCP
/// connection actually carries — as the whole decision. That is not a weakening, because the parser
/// has already folded every rule's scheme into its port: `parse_url_allow_rule` gives an `https://`
/// entry `port: Some(443)` and an `http://` entry `port: Some(80)` unless the entry pinned a port
/// explicitly, and a bare host entry carries neither scheme nor port and spans both defaults. So
/// `https://api.example.com` still refuses port 80 — its port is 443, and `egress_listen_ports`
/// opens no listener on 80 for it at all — while `https://api.example.com:8443` is permitted on
/// exactly the port its listener was opened for.
///
/// The alternative, deriving a scheme from the port number, is what this replaces: it silently
/// refused every connection to a scheme-bearing entry with a non-default port, because `8443 != 443`
/// guessed `http` and the rule said `https`. A listener was opened for a capability that then could
/// not be used. The runtime never terminates TLS — that is the point of splicing bytes rather than
/// proxying requests — so there is no honest way to observe a connection's scheme here, and
/// guessing one is worse than declining to.
fn rule_permits_connection(rule: &NetworkAllowRule, host: &str, port: u16) -> bool {
    let target = RequestTarget {
        scheme: rule.scheme.clone().unwrap_or_default(),
        host: host.to_string(),
        port: Some(port),
    };
    rule.matches(&target)
}

/// Lower-cases a host and drops the trailing dot a fully-qualified DNS name may carry, so
/// `API.Example.com.` and `api.example.com` are one host.
fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

// ---------------------------------------------------------------- address → name bindings

/// The addresses this session's resolver has handed out, and for which names.
///
/// This is what carries a name from the DNS half to the TCP half. It is a *capability* record
/// rather than a cache: an entry exists only because this resolver decided the name was allowed
/// and then looked it up itself, so a connection matching an entry is a connection to something
/// the manifest permitted and the runtime resolved.
///
/// One address can carry several names — shared hosting and CDNs guarantee it — so entries
/// accumulate per address rather than replacing each other, and [`EgressPolicy::allows_connection`]
/// accepts if *any* of them is allowed. That is not a widening: every name in the list got there
/// by passing [`EgressPolicy::allows_name`] first.
#[derive(Debug, Default)]
pub(crate) struct ResolvedNames {
    entries: Mutex<HashMap<IpAddr, Vec<(String, Instant)>>>,
}

impl ResolvedNames {
    pub(crate) fn record(&self, name: &str, addresses: &[IpAddr]) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let now = Instant::now();
        for address in addresses {
            let bindings = entries.entry(*address).or_default();
            bindings.retain(|(bound, at)| {
                bound != name && now.duration_since(*at) < NAME_BINDING_LIFETIME
            });
            bindings.push((name.to_string(), now));
        }
    }

    /// Every unexpired name bound to `address`.
    pub(crate) fn names_for(&self, address: IpAddr) -> Vec<String> {
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        entries
            .get(&address)
            .map(|bindings| {
                bindings
                    .iter()
                    .filter(|(_, at)| now.duration_since(*at) < NAME_BINDING_LIFETIME)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------- DNS

/// The parts of a DNS query this resolver acts on, plus the raw question section it echoes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DnsQuery {
    pub(crate) id: u16,
    /// The client's recursion-desired bit, echoed into the response as the protocol requires.
    pub(crate) recursion_desired: bool,
    /// The QNAME as dotted text, with the root label dropped.
    pub(crate) name: String,
    pub(crate) qtype: u16,
    /// The question section exactly as it arrived, so the response can echo it byte for byte.
    pub(crate) question: Vec<u8>,
}

pub(crate) const DNS_TYPE_A: u16 = 1;
pub(crate) const DNS_TYPE_AAAA: u16 = 28;
pub(crate) const DNS_RCODE_NOERROR: u8 = 0;
pub(crate) const DNS_RCODE_FORMERR: u8 = 1;
/// What a capsule is told when nothing answered the runtime's own lookup. A resolver client reads
/// it as "ask again" — `getaddrinfo` turns it into `EAI_AGAIN`, "Temporary failure in name
/// resolution" — where `NXDOMAIN` would tell it not to bother.
pub(crate) const DNS_RCODE_SERVFAIL: u8 = 2;
pub(crate) const DNS_RCODE_NXDOMAIN: u8 = 3;
pub(crate) const DNS_RCODE_REFUSED: u8 = 5;

/// Parses a single-question DNS query.
///
/// Deliberately strict — one question, no name compression (which a query never uses), no pointer
/// following. A message this rejects still gets an answer (`FORMERR`), because the whole point of
/// terminating DNS here is that a capsule learns the outcome of every query rather than watching
/// it disappear.
pub(crate) fn parse_dns_query(bytes: &[u8]) -> Option<DnsQuery> {
    if bytes.len() < 12 || bytes.len() > MAX_DNS_MESSAGE_BYTES {
        return None;
    }
    let id = u16::from_be_bytes([bytes[0], bytes[1]]);
    let flags = u16::from_be_bytes([bytes[2], bytes[3]]);
    if flags & 0x8000 != 0 {
        // QR set: a response, not a query.
        return None;
    }
    if u16::from_be_bytes([bytes[4], bytes[5]]) != 1 {
        return None;
    }

    let mut offset = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let length = *bytes.get(offset)? as usize;
        offset += 1;
        if length == 0 {
            break;
        }
        if length & 0xc0 != 0 {
            // A compression pointer in a question section is malformed.
            return None;
        }
        let label = bytes.get(offset..offset + length)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        offset += length;
    }
    let qtype_bytes = bytes.get(offset..offset + 4)?;
    let qtype = u16::from_be_bytes([qtype_bytes[0], qtype_bytes[1]]);
    offset += 4;

    Some(DnsQuery {
        id,
        recursion_desired: flags & 0x0100 != 0,
        name: labels.join("."),
        qtype,
        question: bytes[12..offset].to_vec(),
    })
}

/// Builds the reply to `query`: the echoed question, `rcode`, and one record per address.
///
/// Answers name the question through a compression pointer to offset 12 (`0xc00c`), which is what
/// every real resolver emits and what keeps the response inside one datagram.
pub(crate) fn build_dns_response(query: &DnsQuery, rcode: u8, answers: &[IpAddr]) -> Vec<u8> {
    let mut message = Vec::with_capacity(64 + answers.len() * 16);
    message.extend_from_slice(&query.id.to_be_bytes());
    // QR=1, opcode 0 (query), AA=0, TC=0, RD echoed, RA=1, Z=0, RCODE.
    let mut flags: u16 = 0x8000 | 0x0080;
    if query.recursion_desired {
        flags |= 0x0100;
    }
    flags |= u16::from(rcode & 0x0f);
    message.extend_from_slice(&flags.to_be_bytes());
    message.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    message.extend_from_slice(&(answers.len() as u16).to_be_bytes()); // ANCOUNT
    message.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    message.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    message.extend_from_slice(&query.question);

    for answer in answers {
        message.extend_from_slice(&[0xc0, 0x0c]);
        let (rtype, raw): (u16, Vec<u8>) = match answer {
            IpAddr::V4(v4) => (DNS_TYPE_A, v4.octets().to_vec()),
            IpAddr::V6(v6) => (DNS_TYPE_AAAA, v6.octets().to_vec()),
        };
        message.extend_from_slice(&rtype.to_be_bytes());
        message.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        message.extend_from_slice(&DNS_TTL_SECONDS.to_be_bytes());
        message.extend_from_slice(&(raw.len() as u16).to_be_bytes());
        message.extend_from_slice(&raw);
    }

    message
}

/// The whole resolver decision, with the lookup's outcome injected so it is testable without DNS.
///
/// Records the address→name binding for every address it resolves — that binding is what the TCP
/// half later uses to know which host a connection is for, so it is produced here, at the one
/// moment the runtime knows both halves of the pair. No other outcome records anything: a name the
/// allowlist refuses and a name nothing answered for both leave the TCP half knowing nothing.
///
/// `resolve` is [`FnOnce`] because the caller that matters has already awaited the lookup and has
/// one [`Resolution`] to hand over; the tests pass a closure returning a constant.
pub(crate) fn answer_dns_query(
    policy: &EgressPolicy,
    resolved: &ResolvedNames,
    query: &DnsQuery,
    resolve: impl FnOnce(&str) -> Resolution,
) -> Vec<u8> {
    if !policy.allows_name(&query.name) {
        return build_dns_response(query, DNS_RCODE_REFUSED, &[]);
    }
    if query.qtype != DNS_TYPE_A && query.qtype != DNS_TYPE_AAAA {
        // A listed name asked about through a record type this resolver does not synthesise (MX,
        // TXT, SRV…). `NOERROR` with no answers is the accurate reply — the name exists, this
        // resolver has nothing of that type — and TXT in particular is the classic DNS
        // exfiltration and ingest carrier, which this refuses to relay by construction.
        return build_dns_response(query, DNS_RCODE_NOERROR, &[]);
    }

    let all = match resolve(&query.name) {
        Resolution::Resolved(addresses) => addresses,
        // The upstream settled it: there is no such name.
        Resolution::DoesNotExist => return build_dns_response(query, DNS_RCODE_NXDOMAIN, &[]),
        // Nothing answered. `SERVFAIL` is the reply that leaves the capsule free to ask again;
        // `NXDOMAIN` would tell it the name is gone, which nobody established.
        Resolution::DidNotAnswer(_) => return build_dns_response(query, DNS_RCODE_SERVFAIL, &[]),
    };
    // Bind every address of the name, not just the family asked about: a client that queries AAAA
    // first and then connects over IPv4 must not be refused for a name it was legitimately told
    // about.
    resolved.record(&query.name, &all);

    let addresses: Vec<IpAddr> = all
        .into_iter()
        .filter(|address| match query.qtype {
            DNS_TYPE_A => address.is_ipv4(),
            _ => address.is_ipv6(),
        })
        .collect();
    // `NOERROR` with no answers when the name resolved only in the other family — the reply that
    // tells a dual-stack client to try the other one, which for `AAAA` is exactly what this
    // IPv4-routed namespace needs it to do.
    build_dns_response(query, DNS_RCODE_NOERROR, &addresses)
}

// ---------------------------------------------------------------- the running proxy

/// Non-Linux stub. The sockets this serves only exist inside a Linux network namespace, so there
/// is nothing to run here — the same permanence `EnforcementTier::EnvironmentOnly` carries.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct EgressProxyHandle;

#[cfg(not(target_os = "linux"))]
impl EgressProxyHandle {
    pub(crate) fn shutdown(self) {}
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{start_egress_proxy, EgressProxyHandle};

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::net::{IpAddr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        answer_dns_query, parse_dns_query, EgressPolicy, ResolvedNames, DNS_RCODE_FORMERR,
        MAX_DNS_MESSAGE_BYTES,
    };
    use crate::dns_resolver::{DnsResolver, NoAnswer, Resolution};

    /// Read/write timeout on a relayed connection, and how long an upstream connect may take.
    /// Both bound a capsule's ability to pin a proxy thread indefinitely.
    const IO_TIMEOUT: Duration = Duration::from_secs(300);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

    /// How often the accept/receive loops wake to notice a shutdown request. Only the *idle* path
    /// pays it: the wait is a `poll(2)` on the socket, so an arriving connection is served with no
    /// added latency.
    const POLL_INTERVAL_MS: libc::c_int = 200;

    /// Concurrent relayed connections one session will serve. A capsule that opens more has them
    /// closed immediately rather than growing the runtime's thread population without bound.
    const MAX_CONCURRENT_CONNECTIONS: usize = 128;

    /// Live handle to one session's proxy.
    #[derive(Debug)]
    pub(crate) struct EgressProxyHandle {
        stop: Arc<AtomicBool>,
        threads: Vec<std::thread::JoinHandle<()>>,
    }

    impl EgressProxyHandle {
        /// Stops accepting and joins the listener threads.
        ///
        /// Called once the subprocess tree has exited, from the same thread that ran the seccomp
        /// supervisor. In-flight relay threads are deliberately *not* joined: their capsule-side
        /// peer is already gone — a network namespace and every socket in it are destroyed when
        /// the last task in it exits — so each returns on its own next read, and blocking a tool
        /// call's return on them would trade a bounded wait for an unbounded one.
        pub(crate) fn shutdown(self) {
            self.stop.store(true, Ordering::SeqCst);
            for thread in self.threads {
                let _ = thread.join();
            }
        }
    }

    /// Takes ownership of the namespace's listening sockets and starts serving them.
    ///
    /// `sockets` arrives in the order `network_namespace::create_capsule_netns` sent it: one TCP
    /// listener per allowlisted port, in the plan's port order, then the UDP resolver socket.
    /// Every descriptor was created *inside* the capsule's network namespace — a socket belongs to
    /// the namespace it was created in, not to whichever task holds the descriptor — which is what
    /// lets this process accept the capsule's traffic while opening its own upstream connections
    /// in the host's namespace. That inversion is the whole mechanism, and it is why nothing here
    /// needs root.
    pub(crate) fn start_egress_proxy(
        sockets: Vec<OwnedFd>,
        policy: EgressPolicy,
    ) -> io::Result<EgressProxyHandle> {
        let Some((resolver, listeners)) = sockets.split_last() else {
            return Err(io::Error::other(
                "egress-proxy: the capsule namespace handed over no sockets at all",
            ));
        };

        let stop = Arc::new(AtomicBool::new(false));
        let resolved = Arc::new(ResolvedNames::default());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::with_capacity(sockets.len());

        for listener in listeners {
            let listener = TcpListener::from(listener.try_clone()?);
            let policy = policy.clone();
            let resolved = Arc::clone(&resolved);
            let stop = Arc::clone(&stop);
            let in_flight = Arc::clone(&in_flight);
            threads.push(std::thread::spawn(move || {
                serve_tcp(listener, policy, resolved, stop, in_flight);
            }));
        }

        {
            let resolver = UdpSocket::from(resolver.try_clone()?);
            let policy = policy.clone();
            let resolved = Arc::clone(&resolved);
            let stop = Arc::clone(&stop);
            let names = crate::dns_resolver::shared().cloned();
            threads.push(std::thread::spawn(move || {
                serve_dns(resolver, policy, resolved, stop, names);
            }));
        }

        Ok(EgressProxyHandle { stop, threads })
    }

    /// Blocks until `fd` is readable or the interval elapses.
    ///
    /// A `poll(2)` rather than a non-blocking loop with a sleep: an arriving connection is served
    /// with no added latency, and an idle proxy wakes five times a second only to ask whether the
    /// session has ended.
    fn wait_readable(fd: RawFd) -> bool {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` is a live local and the count matches it; `poll` dereferences nothing
        // else and cannot retain the pointer past the call.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::poll(&mut pollfd, 1, POLL_INTERVAL_MS) };
        rc > 0 && pollfd.revents & libc::POLLIN != 0
    }

    fn serve_tcp(
        listener: TcpListener,
        policy: EgressPolicy,
        resolved: Arc<ResolvedNames>,
        stop: Arc<AtomicBool>,
        in_flight: Arc<AtomicUsize>,
    ) {
        while !stop.load(Ordering::SeqCst) {
            if !wait_readable(listener.as_raw_fd()) {
                continue;
            }
            let Ok((client, _)) = listener.accept() else {
                continue;
            };
            // `local_addr` on the accepted socket is the address the capsule dialled, preserved by
            // the namespace's `local default` route. This is the whole reason the interception
            // needs no netfilter rule and no protocol parsing.
            let Ok(SocketAddr::V4(destination)) = client.local_addr() else {
                continue;
            };
            if in_flight.load(Ordering::SeqCst) >= MAX_CONCURRENT_CONNECTIONS {
                continue;
            }
            in_flight.fetch_add(1, Ordering::SeqCst);
            let policy = policy.clone();
            let resolved = Arc::clone(&resolved);
            let done = Arc::clone(&in_flight);
            std::thread::spawn(move || {
                relay_connection(client, destination, &policy, &resolved);
                done.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    /// Checks one accepted connection against the policy and, if it passes, splices it to an
    /// upstream connection opened in the host's network namespace.
    ///
    /// A refused connection is simply closed. There is deliberately no error payload: the capsule's
    /// client is speaking an arbitrary protocol (TLS, SSH, Postgres…), and injecting bytes into a
    /// stream whose framing this proxy does not parse would corrupt it in a way far harder to
    /// diagnose than a close. A close is what "connection reset by peer" is, and it is what the
    /// manual-verification document records as the observable outcome of a denial.
    fn relay_connection(
        client: TcpStream,
        destination: SocketAddrV4,
        policy: &EgressPolicy,
        resolved: &ResolvedNames,
    ) {
        let address = IpAddr::V4(*destination.ip());
        let names = resolved.names_for(address);
        if !policy.allows_connection(&names, address, destination.port()) {
            return;
        }

        let Ok(upstream) =
            TcpStream::connect_timeout(&SocketAddr::V4(destination), CONNECT_TIMEOUT)
        else {
            return;
        };
        // Both sides get the same bound, and a failure to set one closes the connection rather
        // than splicing it anyway. `IO_TIMEOUT` is what stops a relay thread from outliving the
        // session that started it; a thread whose timeouts silently did not take would hold both
        // sockets open indefinitely with nothing recording why, which is exactly the failure mode
        // the bound exists to prevent. Refusing here costs one connection the capsule can retry.
        let bounded = [
            client.set_read_timeout(Some(IO_TIMEOUT)),
            client.set_write_timeout(Some(IO_TIMEOUT)),
            upstream.set_read_timeout(Some(IO_TIMEOUT)),
            upstream.set_write_timeout(Some(IO_TIMEOUT)),
        ];
        if bounded.iter().any(|result| result.is_err()) {
            return;
        }
        splice(client, upstream);
    }

    /// Copies bytes both ways until either side closes, then tears the other down.
    fn splice(client: TcpStream, upstream: TcpStream) {
        let (Ok(client_read), Ok(upstream_read)) = (client.try_clone(), upstream.try_clone())
        else {
            return;
        };
        let mut client_write = client;
        let mut upstream_write = upstream;

        let uplink = std::thread::spawn(move || {
            let mut source = client_read;
            let _ = io::copy(&mut source, &mut upstream_write);
            // Half-close, so the upstream server sees the request end rather than waiting out its
            // own idle timeout.
            let _ = upstream_write.shutdown(std::net::Shutdown::Write);
        });

        let mut source = upstream_read;
        let _ = io::copy(&mut source, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Write);
        let _ = uplink.join();
    }

    // ------------------------------------------------------------ DNS transport

    /// A `cmsghdr` scratch buffer with the alignment the CMSG macros require.
    ///
    /// A bare `[u8; N]` is only 1-byte aligned and `CMSG_FIRSTHDR` casts it straight to
    /// `*mut cmsghdr`, which is a misaligned-pointer panic on a debug build and undefined
    /// behaviour on a release one.
    #[repr(C, align(8))]
    struct CmsgBuffer([u8; 64]);

    /// Receives queries and hands each one to `names`; nothing on this path performs a lookup.
    ///
    /// One query's lookup is a task on `crate::dns_resolver`'s runtime, which replies on the same
    /// socket when it finishes. That is what keeps a slow name off the critical path of every
    /// other one, and what lets `EgressProxyHandle::shutdown` join this thread inside one poll
    /// interval no matter how many lookups are still in flight — a task outliving the session it
    /// was started for writes its reply into a namespace that is already gone, which is a datagram
    /// nobody receives rather than a wait anybody pays for.
    ///
    /// The socket is shared with those tasks rather than lent as a raw descriptor, so the last
    /// task to finish is what closes it. A descriptor number outliving its socket is a write into
    /// whatever the process opened next.
    pub(crate) fn serve_dns(
        socket: UdpSocket,
        policy: EgressPolicy,
        resolved: Arc<ResolvedNames>,
        stop: Arc<AtomicBool>,
        names: Option<DnsResolver>,
    ) {
        let socket = Arc::new(socket);
        let fd = socket.as_raw_fd();
        let mut buffer = vec![0u8; MAX_DNS_MESSAGE_BYTES];
        while !stop.load(Ordering::SeqCst) {
            if !wait_readable(fd) {
                continue;
            }
            let Some((len, peer, local)) = recv_query(fd, &mut buffer) else {
                continue;
            };
            let Some(query) = parse_dns_query(&buffer[..len]) else {
                // Not a query this resolver understands. A `FORMERR` needs the message id, which
                // is the first field of every DNS message; without at least a header there is
                // nothing to reply to.
                if len < 12 {
                    continue;
                }
                let mut header = [0u8; 12];
                header.copy_from_slice(&buffer[..12]);
                send_reply(fd, &formerr_reply(&header), peer, local);
                continue;
            };

            match (names.as_ref(), crate::dns_resolver::runtime_handle()) {
                (Some(names), Some(runtime)) => {
                    let names = names.clone();
                    let policy = policy.clone();
                    let resolved = Arc::clone(&resolved);
                    let socket = Arc::clone(&socket);
                    runtime.spawn(async move {
                        let resolution = names.lookup(&query.name).await;
                        let reply = answer_dns_query(&policy, &resolved, &query, |_| resolution);
                        send_reply(socket.as_raw_fd(), &reply, peer, local);
                    });
                }
                // No resolver at all: every name is one nothing answered for, which is a reply the
                // capsule can act on rather than a query that disappears.
                _ => {
                    let reply = answer_dns_query(&policy, &resolved, &query, |_| {
                        Resolution::DidNotAnswer(NoAnswer::ResolverUnavailable)
                    });
                    send_reply(fd, &reply, peer, local);
                }
            }
        }
    }

    /// Receives one query, reporting both the sender and the address it was *sent to*.
    ///
    /// The destination matters because the socket is bound to the wildcard address and the
    /// namespace's local route delivers every address to it: a query to `1.1.1.1:53` and one to
    /// `8.8.8.8:53` both arrive here, and a reply whose source address is neither is discarded by
    /// the client's resolver as coming from a server it never asked. `IP_PKTINFO` is how the
    /// original destination is recovered.
    fn recv_query(
        fd: RawFd,
        buffer: &mut [u8],
    ) -> Option<(usize, libc::sockaddr_in, Option<libc::in_addr>)> {
        // SAFETY: every buffer below is a live local sized for what the kernel writes into it, and
        // the control buffer carries the alignment the CMSG macros require.
        #[allow(unsafe_code)]
        unsafe {
            let mut peer: libc::sockaddr_in = std::mem::zeroed();
            let mut iov = libc::iovec {
                iov_base: buffer.as_mut_ptr().cast(),
                iov_len: buffer.len(),
            };
            let mut control = CmsgBuffer([0u8; 64]);
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_name = std::ptr::addr_of_mut!(peer).cast();
            msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.0.as_mut_ptr().cast();
            msg.msg_controllen = control.0.len() as _;

            let received = libc::recvmsg(fd, &mut msg, 0);
            if received <= 0 {
                return None;
            }

            let mut local = None;
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::IPPROTO_IP && (*cmsg).cmsg_type == libc::IP_PKTINFO {
                    let info =
                        std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in_pktinfo>());
                    local = Some(info.ipi_addr);
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            Some((received as usize, peer, local))
        }
    }

    /// Sends the reply back to `peer`, sourced from the address the query was addressed to.
    fn send_reply(fd: RawFd, reply: &[u8], peer: libc::sockaddr_in, local: Option<libc::in_addr>) {
        // SAFETY: as `recv_query` — live locals, correctly sized, aligned control buffer.
        #[allow(unsafe_code)]
        unsafe {
            let mut peer = peer;
            let mut iov = libc::iovec {
                iov_base: reply.as_ptr() as *mut libc::c_void,
                iov_len: reply.len(),
            };
            let mut control = CmsgBuffer([0u8; 64]);
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_name = std::ptr::addr_of_mut!(peer).cast();
            msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;

            if let Some(address) = local {
                let bytes = std::mem::size_of::<libc::in_pktinfo>() as u32;
                msg.msg_control = control.0.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(bytes) as _;
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                if !cmsg.is_null() {
                    (*cmsg).cmsg_level = libc::IPPROTO_IP;
                    (*cmsg).cmsg_type = libc::IP_PKTINFO;
                    (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
                    let mut info: libc::in_pktinfo = std::mem::zeroed();
                    // `ipi_spec_dst` is the *source* address of an outgoing packet; setting it is
                    // what makes the reply appear to come from the resolver the client asked.
                    info.ipi_spec_dst = address;
                    std::ptr::copy_nonoverlapping(
                        std::ptr::addr_of!(info).cast::<u8>(),
                        libc::CMSG_DATA(cmsg),
                        bytes as usize,
                    );
                }
            }

            libc::sendmsg(fd, &msg, 0);
        }
    }

    /// A bare `FORMERR` built from a malformed message's header alone.
    fn formerr_reply(header: &[u8; 12]) -> Vec<u8> {
        let mut reply = header.to_vec();
        let flags = 0x8000u16 | 0x0080 | u16::from(DNS_RCODE_FORMERR);
        reply[2..4].copy_from_slice(&flags.to_be_bytes());
        reply[4..12].copy_from_slice(&[0; 8]);
        reply
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::dns_resolver::scripted::{Behaviour, ScriptedResolver};
    use crate::dns_resolver::{DnsResolver, NoAnswer};
    use crate::network_policy::parse_network_allow_rules;

    fn rules(entries: &[&str]) -> Vec<NetworkAllowRule> {
        let owned: Vec<String> = entries.iter().map(|entry| (*entry).to_string()).collect();
        parse_network_allow_rules(&owned).unwrap()
    }

    fn policy(entries: &[&str], ips: &[IpAddr]) -> EgressPolicy {
        EgressPolicy::new(rules(entries), ips.to_vec())
    }

    const ADDRESS: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7));
    const OTHER_ADDRESS: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 8));

    // ---- listen ports ----

    #[test]
    fn an_https_entry_opens_only_443() {
        assert_eq!(
            egress_listen_ports(&rules(&["https://api.example.com"])),
            vec![443]
        );
    }

    #[test]
    fn an_http_entry_opens_only_80() {
        assert_eq!(
            egress_listen_ports(&rules(&["http://api.example.com"])),
            vec![80]
        );
    }

    #[test]
    fn a_bare_host_entry_opens_both_scheme_defaults() {
        // The manifest schema defines a bare host as spanning both schemes, so both are dialled.
        assert_eq!(
            egress_listen_ports(&rules(&["api.example.com"])),
            vec![80, 443]
        );
    }

    #[test]
    fn an_explicit_port_opens_exactly_that_port() {
        assert_eq!(
            egress_listen_ports(&rules(&["api.example.com:8443"])),
            vec![8443]
        );
    }

    #[test]
    fn ports_are_deduplicated_and_sorted() {
        let ports = egress_listen_ports(&rules(&[
            "https://a.example.com",
            "https://b.example.com",
            "http://c.example.com",
        ]));
        assert_eq!(ports, vec![80, 443]);
    }

    #[test]
    fn an_empty_allowlist_opens_no_listener_at_all() {
        // The strongest configuration: no TCP listener exists in the namespace, so every
        // connection is refused by the kernel with nothing in userspace consulted.
        assert!(egress_listen_ports(&[]).is_empty());
    }

    #[test]
    fn the_port_count_is_capped_so_the_pre_exec_fd_array_cannot_overflow() {
        let entries: Vec<String> = (1..=40)
            .map(|port| format!("api.example.com:{}", 9000 + port))
            .collect();
        let parsed = parse_network_allow_rules(&entries).unwrap();
        assert_eq!(egress_listen_ports(&parsed).len(), MAX_EGRESS_TCP_PORTS);
    }

    // ---- the policy ----

    #[test]
    fn a_listed_name_bound_to_the_destination_is_allowed() {
        let policy = policy(&["https://api.example.com"], &[]);
        assert!(policy.allows_connection(&["api.example.com".to_string()], ADDRESS, 443));
    }

    #[test]
    fn an_unlisted_name_bound_to_the_destination_is_refused() {
        let policy = policy(&["https://api.example.com"], &[]);
        assert!(!policy.allows_connection(&["evil.example.com".to_string()], ADDRESS, 443));
    }

    #[test]
    fn a_scheme_pinned_entry_refuses_the_other_scheme() {
        // `https://api.example.com` permits the TLS port and not the plaintext one — exactly what
        // the same rule already means on the WASI-HTTP path.
        let policy = policy(&["https://api.example.com"], &[]);
        assert!(!policy.allows_connection(&["api.example.com".to_string()], ADDRESS, 80));
    }

    #[test]
    fn a_bare_host_entry_spans_both_schemes_and_ports() {
        let policy = policy(&["api.example.com"], &[]);
        let names = vec!["api.example.com".to_string()];
        assert!(policy.allows_connection(&names, ADDRESS, 443));
        assert!(policy.allows_connection(&names, ADDRESS, 80));
    }

    #[test]
    fn a_port_pinned_entry_refuses_another_port() {
        let policy = policy(&["api.example.com:8443"], &[]);
        let names = vec!["api.example.com".to_string()];
        assert!(policy.allows_connection(&names, ADDRESS, 8443));
        assert!(!policy.allows_connection(&names, ADDRESS, 443));
    }

    #[test]
    fn a_scheme_bearing_entry_with_an_explicit_port_is_reachable_on_that_port() {
        // The combination a port-derived scheme guess broke: `egress_listen_ports` opens 8443 for
        // this entry, so every connection to it must be permitted — a declared capability whose
        // listener exists but whose connections are all refused is a capability that does not work.
        let policy = policy(&["https://api.example.com:8443"], &[]);
        let names = vec!["api.example.com".to_string()];
        assert_eq!(
            egress_listen_ports(&rules(&["https://api.example.com:8443"])),
            vec![8443]
        );
        assert!(policy.allows_connection(&names, ADDRESS, 8443));
        // The pinned port is still the only one permitted, scheme default included.
        assert!(!policy.allows_connection(&names, ADDRESS, 443));
        assert!(!policy.allows_connection(&names, ADDRESS, 80));
    }

    #[test]
    fn an_http_entry_with_an_explicit_port_is_reachable_on_that_port() {
        // The same case on the plaintext side, where the old guess happened to agree by accident:
        // pinning it down so a future change to the matcher cannot regress one scheme silently.
        let policy = policy(&["http://api.example.com:8080"], &[]);
        let names = vec!["api.example.com".to_string()];
        assert!(policy.allows_connection(&names, ADDRESS, 8080));
        assert!(!policy.allows_connection(&names, ADDRESS, 80));
    }

    #[test]
    fn a_shared_address_is_decided_by_whichever_bound_name_is_listed() {
        // The CDN case the retired IP-only check could not express: one address, several names,
        // only one of them permitted. Every name in the list got there by passing `allows_name`
        // first, so accepting on any of them is not a widening.
        let policy = policy(&["https://api.example.com"], &[]);
        let both = vec![
            "api.example.com".to_string(),
            "other.example.com".to_string(),
        ];
        assert!(policy.allows_connection(&both, ADDRESS, 443));
        assert!(!policy.allows_connection(&["other.example.com".to_string()], ADDRESS, 443));
    }

    #[test]
    fn an_unbound_address_falls_back_to_the_launch_time_ip_set() {
        // A hardcoded literal: no name was ever resolved for it, so the check is the one the
        // retired seccomp path performed, reused rather than reimplemented.
        let policy = policy(&["api.example.com"], &[ADDRESS]);
        assert!(policy.allows_connection(&[], ADDRESS, 443));
        assert!(!policy.allows_connection(&[], OTHER_ADDRESS, 443));
    }

    #[test]
    fn an_empty_allowlist_permits_nothing() {
        let policy = policy(&[], &[]);
        assert!(!policy.allows_connection(&["api.example.com".to_string()], ADDRESS, 443));
        assert!(!policy.allows_connection(&[], ADDRESS, 443));
        assert!(!policy.allows_name("api.example.com"));
    }

    #[test]
    fn a_name_is_matched_case_insensitively_and_without_the_root_label() {
        let policy = policy(&["https://api.example.com"], &[]);
        assert!(policy.allows_name("API.Example.COM."));
        assert!(policy.allows_connection(&["API.Example.COM.".to_string()], ADDRESS, 443));
    }

    #[test]
    fn the_resolver_ignores_port_and_scheme_but_the_connection_does_not() {
        let policy = policy(&["https://api.example.com:443"], &[]);
        assert!(policy.allows_name("api.example.com"));
        assert!(!policy.allows_connection(&["api.example.com".to_string()], ADDRESS, 8443));
    }

    #[test]
    fn the_resolver_never_resolves_an_address_literal() {
        let policy = policy(&["203.0.113.7"], &[ADDRESS]);
        assert!(!policy.allows_name("203.0.113.7"));
    }

    // ---- address → name bindings ----

    #[test]
    fn a_recorded_binding_is_readable_for_every_address_of_the_name() {
        let resolved = ResolvedNames::default();
        resolved.record("api.example.com", &[ADDRESS, OTHER_ADDRESS]);
        assert_eq!(resolved.names_for(ADDRESS), vec!["api.example.com"]);
        assert_eq!(resolved.names_for(OTHER_ADDRESS), vec!["api.example.com"]);
    }

    #[test]
    fn an_address_no_query_produced_has_no_names() {
        let resolved = ResolvedNames::default();
        assert!(resolved.names_for(ADDRESS).is_empty());
    }

    #[test]
    fn recording_the_same_name_twice_does_not_duplicate_it() {
        let resolved = ResolvedNames::default();
        resolved.record("api.example.com", &[ADDRESS]);
        resolved.record("api.example.com", &[ADDRESS]);
        assert_eq!(resolved.names_for(ADDRESS).len(), 1);
    }

    // ---- DNS ----

    fn query_bytes(name: &str, qtype: u16) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&0x1234u16.to_be_bytes());
        message.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
        message.extend_from_slice(&1u16.to_be_bytes());
        message.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            message.push(label.len() as u8);
            message.extend_from_slice(label.as_bytes());
        }
        message.push(0);
        message.extend_from_slice(&qtype.to_be_bytes());
        message.extend_from_slice(&1u16.to_be_bytes()); // IN
        message
    }

    #[test]
    fn a_dns_query_parses_into_its_name_and_type() {
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_A)).unwrap();
        assert_eq!(query.id, 0x1234);
        assert!(query.recursion_desired);
        assert_eq!(query.name, "api.example.com");
        assert_eq!(query.qtype, DNS_TYPE_A);
    }

    #[test]
    fn a_truncated_or_response_shaped_message_is_rejected() {
        assert_eq!(parse_dns_query(&[0u8; 4]), None);
        let mut response = query_bytes("api.example.com", DNS_TYPE_A);
        response[2] |= 0x80; // QR
        assert_eq!(parse_dns_query(&response), None);
    }

    #[test]
    fn a_compression_pointer_in_a_question_is_rejected() {
        let mut message = query_bytes("api.example.com", DNS_TYPE_A);
        message[12] = 0xc0;
        assert_eq!(parse_dns_query(&message), None);
    }

    #[test]
    fn an_unlisted_name_is_refused_with_an_answer_not_silence() {
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("evil.example.com", DNS_TYPE_A)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(vec![ADDRESS])
        });
        assert_eq!(
            u16::from_be_bytes([reply[0], reply[1]]),
            0x1234,
            "the reply must answer the query it was asked"
        );
        assert!(reply[2] & 0x80 != 0, "QR must be set — this is a response");
        assert_eq!(reply[3] & 0x0f, DNS_RCODE_REFUSED);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0, "no answers");
    }

    #[test]
    fn an_unlisted_name_leaves_no_binding_behind() {
        // The exfiltration case: a refused query must teach the TCP half nothing, or a later
        // connection could inherit a name the manifest never permitted.
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("evil.example.com", DNS_TYPE_A)).unwrap();
        answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(vec![ADDRESS])
        });
        assert!(resolved.names_for(ADDRESS).is_empty());
    }

    #[test]
    fn a_listed_name_is_resolved_forwarded_and_bound() {
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_A)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(vec![ADDRESS])
        });
        assert_eq!(reply[3] & 0x0f, DNS_RCODE_NOERROR);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 1);
        assert_eq!(&reply[reply.len() - 4..], &[203, 0, 113, 7]);
        assert_eq!(resolved.names_for(ADDRESS), vec!["api.example.com"]);
    }

    #[test]
    fn a_listed_name_that_does_not_resolve_is_nxdomain() {
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_A)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| Resolution::DoesNotExist);
        assert_eq!(reply[3] & 0x0f, DNS_RCODE_NXDOMAIN);
    }

    #[test]
    fn a_listed_name_nothing_answered_for_is_servfail_and_leaves_no_binding() {
        // The distinction the whole module turns on: a capsule told SERVFAIL retries, a capsule
        // told NXDOMAIN does not. Nothing was learned about the name, so nothing is bound.
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_A)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::DidNotAnswer(NoAnswer::DeadlineElapsed)
        });
        assert_eq!(reply[3] & 0x0f, DNS_RCODE_SERVFAIL);
        assert_eq!(DNS_RCODE_SERVFAIL, 2);
        assert_ne!(DNS_RCODE_SERVFAIL, DNS_RCODE_NXDOMAIN);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0, "no answers");
        assert!(resolved.names_for(ADDRESS).is_empty());
    }

    #[test]
    fn a_listed_name_with_no_address_record_is_noerror_not_nxdomain() {
        // The name exists and carries something other than an address. `NXDOMAIN` would be a
        // claim the upstream never made.
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_A)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(Vec::new())
        });
        assert_eq!(reply[3] & 0x0f, DNS_RCODE_NOERROR);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0);
        assert!(resolved.names_for(ADDRESS).is_empty());
    }

    #[test]
    fn a_txt_query_for_a_listed_name_carries_no_answer() {
        // TXT is the classic DNS exfiltration and ingest carrier. A listed name still exists, so
        // the truthful reply is NOERROR — with nothing in it.
        let policy = policy(&["https://api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", 16)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(vec![ADDRESS])
        });
        assert_eq!(reply[3] & 0x0f, DNS_RCODE_NOERROR);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0);
    }

    #[test]
    fn an_aaaa_query_answers_empty_so_a_dual_stack_client_falls_back_to_ipv4() {
        let policy = policy(&["api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_AAAA)).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(vec![ADDRESS])
        });
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0);
        assert_eq!(
            reply[3] & 0x0f,
            DNS_RCODE_NOERROR,
            "the name exists, just not in a family this namespace routes"
        );
    }

    #[test]
    fn an_aaaa_query_still_binds_the_ipv4_address_it_learned() {
        // The binding covers every address of the name, not just the queried family, so a client
        // that asks AAAA first and then connects over IPv4 is not refused.
        let policy = policy(&["api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let query = parse_dns_query(&query_bytes("api.example.com", DNS_TYPE_AAAA)).unwrap();
        answer_dns_query(&policy, &resolved, &query, |_| {
            Resolution::Resolved(vec![ADDRESS])
        });
        assert_eq!(resolved.names_for(ADDRESS), vec!["api.example.com"]);
    }

    #[test]
    fn the_response_echoes_the_question_verbatim() {
        let policy = policy(&["api.example.com"], &[]);
        let resolved = ResolvedNames::default();
        let raw = query_bytes("api.example.com", DNS_TYPE_A);
        let query = parse_dns_query(&raw).unwrap();
        let reply = answer_dns_query(&policy, &resolved, &query, |_| Resolution::DoesNotExist);
        assert_eq!(&reply[12..12 + query.question.len()], &raw[12..]);
    }

    // ---- the three outcomes, over a scripted upstream ----

    /// The acceptance criterion this module's resolver exists for: a resolver that receives a
    /// query and never replies must be distinguishable, inside the deadline, from one that says
    /// the name does not exist.
    #[test]
    fn a_blackholed_name_is_servfail_and_a_missing_one_is_nxdomain() {
        let blackhole = ScriptedResolver::start(&[], Behaviour::Blackhole).unwrap();
        let missing = ScriptedResolver::start(&[], Behaviour::DoesNotExist).unwrap();
        let slow = DnsResolver::from_config(blackhole.config()).unwrap();
        let absent = DnsResolver::from_config(missing.config()).unwrap();

        let policy = policy(&["slow.example.test", "gone.example.test"], &[]);
        let resolved = ResolvedNames::default();

        let query = parse_dns_query(&query_bytes("slow.example.test", DNS_TYPE_A)).unwrap();
        let started = Instant::now();
        let blackholed = answer_dns_query(&policy, &resolved, &query, |name| slow.resolve(name));
        let waited = started.elapsed();

        assert_eq!(blackholed[3] & 0x0f, DNS_RCODE_SERVFAIL);
        assert!(
            waited > Duration::from_secs(4) && waited < Duration::from_secs(8),
            "the blackholed lookup must be given its whole deadline and no more, waited {waited:?}"
        );

        let query = parse_dns_query(&query_bytes("gone.example.test", DNS_TYPE_A)).unwrap();
        let started = Instant::now();
        let gone = answer_dns_query(&policy, &resolved, &query, |name| absent.resolve(name));
        let waited = started.elapsed();

        assert_eq!(gone[3] & 0x0f, DNS_RCODE_NXDOMAIN);
        assert!(
            waited < Duration::from_secs(1),
            "an answered lookup must not wait on a deadline, waited {waited:?}"
        );
        assert_ne!(blackholed[3] & 0x0f, gone[3] & 0x0f);
        assert!(
            blackholed.len() >= 12 && gone.len() >= 12,
            "neither is silence"
        );
    }

    // ---- the running DNS server ----

    /// Binds a loopback socket, runs [`linux::serve_dns`] over it against `upstream`, and returns
    /// the address to send queries to, the stop flag and the server thread.
    ///
    /// A plain loopback socket carries no `IP_PKTINFO`, so `recv_query` reports no local address
    /// and `send_reply` sends without one — the path a namespace socket does not take, kept
    /// working because this harness is the only way to drive the receive loop without root.
    #[cfg(target_os = "linux")]
    fn serve_dns_on_loopback(
        upstream: &ScriptedResolver,
        entries: &[&str],
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        use std::net::{Ipv4Addr, UdpSocket};
        use std::sync::atomic::AtomicBool;

        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = socket.local_addr().unwrap();
        let policy = policy(entries, &[]);
        let names = DnsResolver::from_config(upstream.config()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                linux::serve_dns(
                    socket,
                    policy,
                    Arc::new(ResolvedNames::default()),
                    stop,
                    Some(names),
                );
            })
        };
        (address, stop, thread)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_slow_name_does_not_hold_up_the_one_asked_after_it() {
        use std::net::{Ipv4Addr, UdpSocket};

        let upstream = ScriptedResolver::start(
            &[
                ("slow.example.test.", Behaviour::Blackhole),
                ("fast.example.test.", Behaviour::Answer(vec![ADDRESS])),
            ],
            Behaviour::DoesNotExist,
        )
        .unwrap();
        let (server, stop, thread) =
            serve_dns_on_loopback(&upstream, &["slow.example.test", "fast.example.test"]);

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let started = Instant::now();
        client
            .send_to(&query_bytes("slow.example.test", DNS_TYPE_A), server)
            .unwrap();
        client
            .send_to(&query_bytes("fast.example.test", DNS_TYPE_A), server)
            .unwrap();

        let mut buffer = [0u8; 512];
        let (len, _) = client.recv_from(&mut buffer).unwrap();
        let first = started.elapsed();
        assert_eq!(
            buffer[3] & 0x0f,
            DNS_RCODE_NOERROR,
            "the first reply back must be the name that resolved"
        );
        assert_eq!(u16::from_be_bytes([buffer[6], buffer[7]]), 1);
        assert_eq!(&buffer[len - 4..len], &[203, 0, 113, 7]);
        assert!(
            first < Duration::from_secs(1),
            "the fast name waited on the slow one, {first:?}"
        );

        let (_, _) = client.recv_from(&mut buffer).unwrap();
        let second = started.elapsed();
        assert_eq!(buffer[3] & 0x0f, DNS_RCODE_SERVFAIL);
        assert!(
            second > Duration::from_secs(4) && second < Duration::from_secs(8),
            "the blackholed name must answer at its deadline, {second:?}"
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        thread.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shutdown_does_not_wait_on_a_lookup_in_flight() {
        use std::net::{Ipv4Addr, UdpSocket};

        let upstream = ScriptedResolver::start(&[], Behaviour::Blackhole).unwrap();
        let (server, stop, thread) = serve_dns_on_loopback(&upstream, &["slow.example.test"]);

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client
            .send_to(&query_bytes("slow.example.test", DNS_TYPE_A), server)
            .unwrap();
        // Long enough for the receive loop to have taken the query and spawned its lookup.
        std::thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        thread.join().unwrap();
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(1),
            "teardown is bounded by the poll interval, not by the lookup deadline, {waited:?}"
        );
    }
}
