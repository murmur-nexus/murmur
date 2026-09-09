//! The one name resolver this runtime uses, for every name it looks up.
//!
//! Three callers share it: the egress proxy's DNS half ([`crate::egress_proxy`]), which resolves
//! on a capsule's behalf; the runtime's own outbound HTTP ([`crate::http_client`]); and the
//! launch-time resolution of `capabilities.network.allow`
//! ([`crate::sandbox::resolve_network_allowlist_ips`]). None of them calls the C library's
//! resolver, and none of them can.
//!
//! ## Why not `getaddrinfo`
//!
//! `getaddrinfo(3)` has no deadline and no cancellation. The only way to bound it is to abandon
//! the thread running it, which throws away the one fact that matters: *why* there was no answer.
//! A name that does not exist and a resolver that did not reply are different situations with
//! different remedies, and collapsing them tells a capsule "that name does not exist" whenever the
//! upstream is merely slow — a claim a resolver client has no reason to retry.
//!
//! A lookup here is an ordinary cancellable future under [`LOOKUP_DEADLINE`]. Its expiry is a
//! named outcome, [`NoAnswer::DeadlineElapsed`], not the absence of one.
//!
//! ## The three outcomes
//!
//! [`Resolution`] has exactly three variants, and [`crate::egress_proxy::answer_dns_query`] maps
//! them onto exactly three rcodes: `NOERROR`, `NXDOMAIN` and `SERVFAIL`. `SERVFAIL` is what
//! `getaddrinfo` inside a capsule reports as `EAI_AGAIN` / "Temporary failure in name resolution",
//! which is the truthful thing to say when nothing answered.
//!
//! ## What this does not do that glibc does
//!
//! * `/etc/nsswitch.conf` is not read, and its ordering is not honoured. The order here is always
//!   the hosts file, then DNS.
//! * No non-DNS name source is consulted: no mDNS (`.local`), no `myhostname`, no `nis`, and not
//!   systemd-resolved's own `resolve` NSS module. A name that only exists in one of those does not
//!   resolve here. Note that systemd-resolved's *stub listener* is an ordinary nameserver in
//!   `/etc/resolv.conf` and is used as one, so a host whose `resolv.conf` names `127.0.0.53`
//!   still reaches systemd-resolved over DNS — it is only the NSS module that is bypassed.
//! * `/etc/resolv.conf` and `/etc/hosts` are read once, when the process's resolver is first
//!   needed, and not re-read when they change. A host that rewrites either mid-run needs the
//!   runtime restarted for the change to take effect.

use std::fmt;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

use hickory_resolver::config::ResolverOpts;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::TokioResolver;
use tokio::runtime::{Builder, Handle, Runtime};

#[cfg(test)]
use hickory_resolver::config::ResolverConfig;
#[cfg(test)]
use hickory_resolver::net::runtime::TokioRuntimeProvider;
#[cfg(test)]
use hickory_resolver::Hosts;
#[cfg(test)]
use std::sync::Arc;

/// Total budget for one name, across every search suffix tried and every nameserver asked.
///
/// The DNS-side counterpart of `egress_proxy`'s `CONNECT_TIMEOUT`. A capsule's own resolver
/// client retransmits on its own schedule — glibc's default is 5 s per attempt — so a bound much
/// longer than this would be answered by nothing the client is still listening for.
pub(crate) const LOOKUP_DEADLINE: Duration = Duration::from_secs(5);

/// End-to-end bound on one round of nameservers, enforced by the resolver's own pool.
///
/// Deliberately well under [`LOOKUP_DEADLINE`]: a host whose first nameserver is a blackhole must
/// still have its second one asked inside the total budget, which a per-name-server timeout equal
/// to the budget would make impossible.
const NAMESERVER_TIMEOUT: Duration = Duration::from_secs(2);

/// Retries of a query that got no response at all, on top of the first send.
///
/// Only failures to *get* a response are retried — a negative answer is an answer and is returned
/// as it stands.
const NAMESERVER_ATTEMPTS: usize = 2;

/// Threads driving in-process lookups. A lookup is pure I/O with no work between packets, so one
/// spare thread is headroom rather than parallelism.
const RESOLVER_WORKER_THREADS: usize = 2;

/// What one lookup found.
///
/// The distinction between the last two variants is the whole reason this module exists: a name
/// that does not exist is a settled fact, and nothing answering is a transient one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// The name's addresses, deduplicated, across both families, in the order the resolver
    /// returned them. Empty when the name exists but carries no address record at all — which is
    /// `NOERROR` with no answers on the wire, not `NXDOMAIN`.
    Resolved(Vec<IpAddr>),
    /// An upstream answered authoritatively that there is no such name.
    DoesNotExist,
    /// Nothing answered. Carries which way that happened, because the three ways read very
    /// differently in a log.
    DidNotAnswer(NoAnswer),
}

/// How a lookup came back with no answer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoAnswer {
    /// [`LOOKUP_DEADLINE`] elapsed with the lookup still in flight. The future was dropped at that
    /// point, so no thread and no socket outlives the deadline.
    DeadlineElapsed,
    /// Every configured nameserver was asked and none produced an answer, or none was reachable
    /// to ask.
    NoNameserverAnswered,
    /// This process has no resolver: `/etc/resolv.conf` could not be read, or the runtime the
    /// lookups are driven on could not be built.
    ResolverUnavailable,
}

impl fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeadlineElapsed => write!(
                f,
                "the resolver did not answer within {}s",
                LOOKUP_DEADLINE.as_secs()
            ),
            Self::NoNameserverAnswered => write!(f, "no nameserver answered the query"),
            Self::ResolverUnavailable => write!(
                f,
                "this process has no resolver: the system resolver configuration could not be read"
            ),
        }
    }
}

/// A resolver over one configuration.
///
/// The process shares a single instance, [`shared`]; a test builds its own against a scripted
/// nameserver. Every instance drives its lookups on the one runtime [`runtime_handle`] owns, so
/// building one is cheap and building several is not a several-runtimes problem.
#[derive(Clone)]
pub(crate) struct DnsResolver {
    inner: TokioResolver,
}

impl DnsResolver {
    /// The resolver the runtime ships with: `/etc/resolv.conf` for its nameservers, search list
    /// and `ndots`, and `/etc/hosts` for its static names.
    ///
    /// The hosts file is honoured because [`ResolverOpts::use_hosts_file`] is left at
    /// [`hickory_resolver::config::ResolveHosts::Auto`], which reads the system file.
    pub(crate) fn from_system() -> Result<Self, String> {
        let handle = runtime_handle().ok_or_else(|| {
            "the runtime that drives name resolution could not be built".to_string()
        })?;
        let _guard = handle.enter();
        let mut builder = TokioResolver::builder_tokio()
            .map_err(|error| format!("/etc/resolv.conf could not be read: {error}"))?;
        bound(builder.options_mut());
        builder
            .build()
            .map(|inner| Self { inner })
            .map_err(|error| format!("the system resolver could not be built: {error}"))
    }

    /// One name, looked up under [`LOOKUP_DEADLINE`].
    ///
    /// An address literal is answered from the text itself: `getaddrinfo` returns it unchanged and
    /// so does this, with no packet leaving the process.
    pub(crate) async fn lookup(&self, name: &str) -> Resolution {
        if let Ok(address) = name.parse::<IpAddr>() {
            return Resolution::Resolved(vec![address]);
        }
        match tokio::time::timeout(LOOKUP_DEADLINE, self.inner.lookup_ip(name)).await {
            Ok(Ok(found)) => {
                let mut addresses: Vec<IpAddr> = Vec::new();
                for address in found.iter() {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
                Resolution::Resolved(addresses)
            }
            Ok(Err(error)) => classify(&error),
            // The future is dropped here, which cancels the lookup outright — there is no
            // abandoned thread and no socket left waiting for a reply nobody will read.
            Err(_elapsed) => Resolution::DidNotAnswer(NoAnswer::DeadlineElapsed),
        }
    }

    /// [`Self::lookup`] for a caller that is not itself async.
    ///
    /// Dispatches onto the resolver runtime and waits for the result rather than driving a runtime
    /// on the calling thread: `sandbox::start_namespace_proxy` runs on a plain `std::thread` with
    /// no ambient runtime, while the staging path that resolves `capabilities.network.allow` may
    /// well be inside one, and starting a runtime from within a runtime panics. The wait is
    /// bounded by [`LOOKUP_DEADLINE`] because the task it waits on is. An async caller must use
    /// [`Self::lookup`]: blocking a resolver worker thread from inside it would deadlock.
    pub(crate) fn resolve(&self, name: &str) -> Resolution {
        let Some(handle) = runtime_handle() else {
            return Resolution::DidNotAnswer(NoAnswer::ResolverUnavailable);
        };
        let resolver = self.clone();
        let name = name.to_string();
        let (answer_tx, answer_rx) = std::sync::mpsc::sync_channel(1);
        handle.spawn(async move {
            // The receiver is still there — this task is the only thing it waits on — but a send
            // to a dropped receiver is not an error worth panicking over either.
            let _ = answer_tx.send(resolver.lookup(&name).await);
        });
        answer_rx
            .recv()
            .unwrap_or(Resolution::DidNotAnswer(NoAnswer::ResolverUnavailable))
    }
}

#[cfg(test)]
impl Resolution {
    /// The addresses this outcome carries; empty for either failure.
    fn addresses(&self) -> &[IpAddr] {
        match self {
            Self::Resolved(addresses) => addresses,
            _ => &[],
        }
    }
}

/// The seams the scripted-resolver tests build a resolver through. Nothing in the shipped paths
/// uses them: production has exactly one resolver, over exactly the system configuration.
#[cfg(test)]
impl DnsResolver {
    /// A resolver over an explicit configuration, with the same bounds as the shipped one.
    ///
    /// The seam every test that must not reach a real nameserver goes through.
    pub(crate) fn from_config(config: ResolverConfig) -> Result<Self, String> {
        let handle = runtime_handle().ok_or_else(|| {
            "the runtime that drives name resolution could not be built".to_string()
        })?;
        let _guard = handle.enter();
        let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::new());
        bound(builder.options_mut());
        builder
            .build()
            .map(|inner| Self { inner })
            .map_err(|error| format!("the resolver could not be built: {error}"))
    }

    /// Replaces the hosts table, in place of the system `/etc/hosts` this would otherwise carry.
    pub(crate) fn set_hosts(&mut self, hosts: Hosts) {
        self.inner.set_hosts(Arc::new(hosts));
    }

    /// The options this resolver was built with, including which hosts file it reads.
    pub(crate) fn options(&self) -> &ResolverOpts {
        self.inner.options()
    }
}

/// The bounds every resolver in this process carries, applied over whatever `/etc/resolv.conf`
/// asked for.
///
/// `ndots`, the search list and the nameserver addresses are left exactly as the file states them.
/// Only the two timing fields are overridden, and only downwards.
fn bound(options: &mut ResolverOpts) {
    options.timeout = NAMESERVER_TIMEOUT;
    options.attempts = NAMESERVER_ATTEMPTS;
}

/// Reads one failed lookup as one of the three outcomes.
///
/// The classification lives in [`hickory_resolver::net::NoRecords::response_code`]: `NXDOMAIN`
/// there means the name does not exist, and `NOERROR` means it exists but carries no record of
/// the type asked for — which is not an error to a caller asking for addresses, just an empty
/// answer. Everything else is something not answering.
fn classify(error: &NetError) -> Resolution {
    match error {
        NetError::Dns(DnsError::NoRecordsFound(no_records)) => match no_records.response_code {
            ResponseCode::NXDomain => Resolution::DoesNotExist,
            ResponseCode::NoError => Resolution::Resolved(Vec::new()),
            _ => Resolution::DidNotAnswer(NoAnswer::NoNameserverAnswered),
        },
        NetError::Dns(DnsError::ResponseCode(ResponseCode::NXDomain)) => Resolution::DoesNotExist,
        NetError::Timeout => Resolution::DidNotAnswer(NoAnswer::DeadlineElapsed),
        _ => Resolution::DidNotAnswer(NoAnswer::NoNameserverAnswered),
    }
}

/// The runtime every in-process lookup is driven on, built once and owned for the life of the
/// process.
///
/// Separate from whatever runtime a capsule's execution uses: the DNS receive loop spawns onto
/// this from a plain `std::thread`, and a lookup left in flight when a session ends has somewhere
/// to finish that does not outlive it by being joined.
fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            Builder::new_multi_thread()
                .worker_threads(RESOLVER_WORKER_THREADS)
                .thread_name("murmur-dns")
                .enable_all()
                .build()
                .ok()
        })
        .as_ref()
}

/// A handle onto [`runtime`], for a caller that has a lookup to spawn rather than one to wait on.
pub(crate) fn runtime_handle() -> Option<&'static Handle> {
    runtime().map(Runtime::handle)
}

/// The process's resolver, built on first use from `/etc/resolv.conf` and `/etc/hosts`.
///
/// `None` only when the system configuration could not be read at all, which every caller reports
/// as [`NoAnswer::ResolverUnavailable`] rather than as a name that does not exist.
pub(crate) fn shared() -> Option<&'static DnsResolver> {
    static SHARED: OnceLock<Option<DnsResolver>> = OnceLock::new();
    SHARED
        .get_or_init(|| match DnsResolver::from_system() {
            Ok(resolver) => Some(resolver),
            Err(error) => {
                eprintln!(
                    "[capsule-runtime] warning: no name resolver could be built: {error} \
                     (every name lookup this process makes reports that nothing answered)"
                );
                None
            }
        })
        .as_ref()
}

/// One name, resolved by the process's resolver, for a caller that is not async.
pub(crate) fn resolve(name: &str) -> Resolution {
    match shared() {
        Some(resolver) => resolver.resolve(name),
        None => Resolution::DidNotAnswer(NoAnswer::ResolverUnavailable),
    }
}

#[cfg(test)]
pub(crate) mod scripted {
    //! A nameserver on loopback that answers exactly what a test tells it to.
    //!
    //! Every DNS test in this crate points a [`DnsResolver`](super::DnsResolver) at one of these
    //! rather than at a real nameserver, so no test needs a network, a container or root. Build
    //! one with [`ScriptedResolver::start`], turn it into a resolver configuration with
    //! [`ScriptedResolver::config`], and read back what it was asked with
    //! [`ScriptedResolver::asked`].

    use std::io;
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hickory_resolver::config::{NameServerConfig, ResolverConfig};

    use crate::egress_proxy::{
        build_dns_response, parse_dns_query, DNS_RCODE_NOERROR, DNS_RCODE_NXDOMAIN, DNS_TYPE_A,
        DNS_TYPE_AAAA,
    };

    /// How often the server wakes to notice it has been dropped.
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    /// What the scripted server does with one query.
    #[derive(Debug, Clone)]
    pub(crate) enum Behaviour {
        /// `NOERROR` carrying whichever of `addresses` match the queried family. A query for any
        /// other record type gets `NOERROR` with no answers, which is what a real nameserver says
        /// for a name that exists and has nothing of that type.
        Answer(Vec<IpAddr>),
        /// `NXDOMAIN`: this name does not exist.
        DoesNotExist,
        /// Receive the query, record it, and never reply. The blackhole the 5-second deadline
        /// exists for.
        Blackhole,
    }

    /// A running scripted nameserver. Stops and joins its thread when dropped.
    pub(crate) struct ScriptedResolver {
        port: u16,
        asked: Arc<Mutex<Vec<(String, u16)>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl ScriptedResolver {
        /// Binds an ephemeral port on `127.0.0.1` and starts serving.
        ///
        /// `script` maps an exact QNAME — written the way it arrives on the wire, with its
        /// trailing dot — to what to do with a query for it. `otherwise` covers every name the
        /// script does not name.
        pub(crate) fn start(
            script: &[(&str, Behaviour)],
            otherwise: Behaviour,
        ) -> io::Result<Self> {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
            socket.set_read_timeout(Some(POLL_INTERVAL))?;
            let port = socket.local_addr()?.port();

            let script: Vec<(String, Behaviour)> = script
                .iter()
                .map(|(name, behaviour)| ((*name).to_string(), behaviour.clone()))
                .collect();
            let asked = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));

            let thread = {
                let asked = Arc::clone(&asked);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || serve(socket, script, otherwise, asked, stop))
            };

            Ok(Self {
                port,
                asked,
                stop,
                thread: Some(thread),
            })
        }

        /// A resolver configuration naming this server and nothing else, with no search list.
        pub(crate) fn config(&self) -> ResolverConfig {
            let mut name_server = NameServerConfig::udp(IpAddr::V4(Ipv4Addr::LOCALHOST));
            for connection in &mut name_server.connections {
                connection.port = self.port;
            }
            ResolverConfig::from_name_servers(vec![name_server])
        }

        /// Every query received, in arrival order, as `(QNAME with its trailing dot, QTYPE)`.
        pub(crate) fn asked(&self) -> Vec<(String, u16)> {
            self.asked.lock().unwrap().clone()
        }

        /// The QNAMEs received, in first-arrival order and without repeats.
        ///
        /// A dual-stack lookup sends `A` and `AAAA` for one name in parallel, so the raw arrival
        /// order carries each name twice in an order the network decides. The sequence of *names*
        /// is what `resolv.conf(5)`'s search and `ndots` rules determine, and it is deterministic.
        pub(crate) fn names_asked(&self) -> Vec<String> {
            let mut names: Vec<String> = Vec::new();
            for (name, _) in self.asked.lock().unwrap().iter() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            names
        }
    }

    impl Drop for ScriptedResolver {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn serve(
        socket: UdpSocket,
        script: Vec<(String, Behaviour)>,
        otherwise: Behaviour,
        asked: Arc<Mutex<Vec<(String, u16)>>>,
        stop: Arc<AtomicBool>,
    ) {
        let mut buffer = [0u8; 4096];
        while !stop.load(Ordering::SeqCst) {
            let Ok((len, peer)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            let Some(query) = parse_dns_query(&buffer[..len]) else {
                continue;
            };
            let qname = format!("{}.", query.name);
            asked.lock().unwrap().push((qname.clone(), query.qtype));

            let behaviour = script
                .iter()
                .find(|(name, _)| *name == qname)
                .map(|(_, behaviour)| behaviour)
                .unwrap_or(&otherwise);

            let reply = match behaviour {
                Behaviour::Blackhole => continue,
                Behaviour::DoesNotExist => build_dns_response(&query, DNS_RCODE_NXDOMAIN, &[]),
                Behaviour::Answer(addresses) => {
                    let matching: Vec<IpAddr> = addresses
                        .iter()
                        .copied()
                        .filter(|address| match query.qtype {
                            DNS_TYPE_A => address.is_ipv4(),
                            DNS_TYPE_AAAA => address.is_ipv6(),
                            _ => false,
                        })
                        .collect();
                    build_dns_response(&query, DNS_RCODE_NOERROR, &matching)
                }
            };
            let _ = socket.send_to(&reply, peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::str::FromStr;

    use hickory_resolver::config::ResolveHosts;
    use hickory_resolver::proto::rr::Name;
    use hickory_resolver::Hosts;

    use super::scripted::{Behaviour, ScriptedResolver};
    use super::{DnsResolver, NoAnswer, Resolution};
    use crate::egress_proxy::DNS_TYPE_AAAA;

    const ANSWER: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    const PINNED: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9));

    #[test]
    fn a_name_the_upstream_answers_resolves_to_exactly_its_address() {
        let upstream = ScriptedResolver::start(
            &[("api.example.test.", Behaviour::Answer(vec![ANSWER]))],
            Behaviour::DoesNotExist,
        )
        .unwrap();
        let resolver = DnsResolver::from_config(upstream.config()).unwrap();

        assert_eq!(
            resolver.resolve("api.example.test"),
            Resolution::Resolved(vec![ANSWER])
        );
        assert!(
            upstream
                .names_asked()
                .contains(&"api.example.test.".to_string()),
            "the scripted upstream must have been the one asked, got {:?}",
            upstream.names_asked()
        );
    }

    fn resolver_with_hosts(upstream: &ScriptedResolver, hosts_file: &str) -> DnsResolver {
        let mut hosts = Hosts::default();
        hosts.read_hosts_conf(hosts_file.as_bytes()).unwrap();
        let mut resolver = DnsResolver::from_config(upstream.config()).unwrap();
        resolver.set_hosts(hosts);
        resolver
    }

    #[test]
    fn a_name_in_the_hosts_file_resolves_without_a_query_leaving_the_process() {
        let upstream = ScriptedResolver::start(&[], Behaviour::DoesNotExist).unwrap();
        let resolver = resolver_with_hosts(
            &upstream,
            "198.51.100.9 pinned.example.test\n2001:db8::9 pinned.example.test\n",
        );

        assert!(resolver
            .resolve("pinned.example.test")
            .addresses()
            .contains(&PINNED));
        assert_eq!(
            upstream.asked().len(),
            0,
            "a name the hosts file answers in both families must not reach a nameserver"
        );
    }

    #[test]
    fn a_hosts_entry_in_one_family_only_still_asks_upstream_for_the_other() {
        // The hosts table is consulted per record type, so an IPv4-only entry answers the `A`
        // query from the file and lets the `AAAA` query go out. The address the caller gets is
        // still the file's, and no `A` query leaves the process.
        let upstream = ScriptedResolver::start(&[], Behaviour::DoesNotExist).unwrap();
        let resolver = resolver_with_hosts(&upstream, "198.51.100.9 pinned.example.test\n");

        assert_eq!(
            resolver.resolve("pinned.example.test"),
            Resolution::Resolved(vec![PINNED])
        );
        assert_eq!(
            upstream.asked(),
            vec![("pinned.example.test.".to_string(), DNS_TYPE_AAAA)]
        );
    }

    #[test]
    fn the_shipped_resolver_reads_the_system_hosts_file() {
        let resolver = DnsResolver::from_system().unwrap();
        assert_ne!(
            resolver.options().use_hosts_file,
            ResolveHosts::Never,
            "the shipped resolver must consult /etc/hosts"
        );
        assert!(
            resolver
                .resolve("localhost")
                .addresses()
                .contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "localhost must resolve to the loopback address the system files name"
        );
    }

    /// The configuration the search and `ndots` tests share: one search domain, `ndots` at the
    /// `resolv.conf(5)` default of 1.
    fn with_search(upstream: &ScriptedResolver) -> DnsResolver {
        let mut config = upstream.config();
        config.add_search(Name::from_str("example.test.").unwrap());
        let resolver = DnsResolver::from_config(config).unwrap();
        assert_eq!(resolver.options().ndots, 1);
        resolver
    }

    #[test]
    fn a_single_label_name_is_tried_suffixed_before_it_is_tried_bare() {
        let upstream = ScriptedResolver::start(&[], Behaviour::DoesNotExist).unwrap();
        let resolver = with_search(&upstream);

        assert_eq!(resolver.resolve("web"), Resolution::DoesNotExist);
        assert_eq!(
            upstream.names_asked(),
            vec!["web.example.test.".to_string(), "web.".to_string()],
        );
    }

    #[test]
    fn a_single_label_name_resolves_through_the_search_suffix() {
        let upstream = ScriptedResolver::start(
            &[("web.example.test.", Behaviour::Answer(vec![ANSWER]))],
            Behaviour::DoesNotExist,
        )
        .unwrap();
        let resolver = with_search(&upstream);

        assert_eq!(resolver.resolve("web"), Resolution::Resolved(vec![ANSWER]));
        assert_eq!(
            upstream.names_asked(),
            vec!["web.example.test.".to_string()]
        );
    }

    #[test]
    fn a_name_carrying_ndots_dots_is_tried_absolute_before_it_is_suffixed() {
        let upstream = ScriptedResolver::start(
            &[(
                "api.example.test.example.test.",
                Behaviour::Answer(vec![ANSWER]),
            )],
            Behaviour::DoesNotExist,
        )
        .unwrap();
        let resolver = with_search(&upstream);

        assert_eq!(
            resolver.resolve("api.example.test"),
            Resolution::Resolved(vec![ANSWER])
        );
        assert_eq!(
            upstream.names_asked(),
            vec![
                "api.example.test.".to_string(),
                "api.example.test.example.test.".to_string(),
            ],
        );
    }

    #[test]
    fn an_address_literal_is_returned_without_a_lookup() {
        let upstream = ScriptedResolver::start(&[], Behaviour::Blackhole).unwrap();
        let resolver = DnsResolver::from_config(upstream.config()).unwrap();

        assert_eq!(
            resolver.resolve("127.0.0.1"),
            Resolution::Resolved(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        );
        assert_eq!(upstream.asked().len(), 0);
    }

    #[test]
    fn the_two_failure_outcomes_read_differently() {
        assert_ne!(
            NoAnswer::DeadlineElapsed.to_string(),
            NoAnswer::NoNameserverAnswered.to_string()
        );
        assert!(NoAnswer::DeadlineElapsed
            .to_string()
            .contains("did not answer within 5s"));
    }
}
