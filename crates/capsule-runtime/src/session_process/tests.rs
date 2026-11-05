use super::*;

use crate::bindings::tool_host::murmur::process::process::Host as _;

/// A registry over an environment-only enforcement (no kernel sandbox, so these run on any
/// host), allowing `allow`, with a scratch work folder.
fn registry(allow: &[&str]) -> (Arc<SessionProcesses>, tempfile::TempDir) {
    let workdir = tempfile::tempdir().unwrap();
    let policy = CapabilityPolicy {
        shell_allow: allow.iter().map(|name| (*name).to_string()).collect(),
        ..CapabilityPolicy::default()
    };
    let registry = SessionProcesses::new(
        ShellEnforcement::environment_only().with_test_session_dirs(),
        policy,
        WorkdirRoot::open(workdir.path()).unwrap(),
        Vec::new(),
        None,
    );
    registry.set_hooks_gate_calls(false);
    (Arc::new(registry), workdir)
}

fn ctx(registry: &Arc<SessionProcesses>, tool: &str, gate: GateRoute) -> ProcessAccess {
    ProcessAccess::Available(ProcessCallCtx::new(Arc::clone(registry), tool, None, gate))
}

fn run_request(program: &str, args: &[&str], stdin: Option<&[u8]>) -> wit::RunRequest {
    wit::RunRequest {
        program: program.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: Vec::new(),
        stdin: stdin.map(<[u8]>::to_vec),
        timeout_ms: None,
    }
}

fn start_request(name: &str, program: &str, args: &[&str]) -> wit::StartRequest {
    wit::StartRequest {
        name: name.to_string(),
        program: program.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: Vec::new(),
    }
}

fn rule_of(error: &wit::ProcessError) -> Option<&str> {
    match error {
        wit::ProcessError::Refused(refusal) => Some(refusal.rule.as_str()),
        _ => None,
    }
}

fn pid_alive(pid: i32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test]
async fn every_function_refuses_as_unavailable_without_a_process_context() {
    let access = ProcessAccess::Unavailable(unavailable::DRIVER);
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let error = view.run(run_request("cat", &[], None)).await.unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::UNAVAILABLE));
    let error = view
        .start(start_request("x", "cat", &[]))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::UNAVAILABLE));
    assert_eq!(
        rule_of(&view.status("prc_1".into()).await.unwrap_err()),
        Some(rule::UNAVAILABLE)
    );
    assert_eq!(
        rule_of(&view.connect("prc_1".into(), 80).await.unwrap_err()),
        Some(rule::UNAVAILABLE)
    );
    assert_eq!(view.find("x".into()).await, None);
}

#[tokio::test]
async fn a_gating_session_with_no_decision_route_refuses_every_program() {
    let (registry, workdir) = registry(&["touch"]);
    registry.set_hooks_gate_calls(true);
    let route = registry.route_without_gate(false);
    assert!(matches!(route, GateRoute::Unreachable));
    let access = ctx(&registry, "probe", route);
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let error = view
        .run(run_request("touch", &["ran"], None))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::ON_SHELL));
    let error = view
        .start(start_request("t", "touch", &["started"]))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::ON_SHELL));
    assert!(!workdir.path().join("ran").exists());
    assert!(!workdir.path().join("started").exists());
    // A session with no hook and no read-only path needs no route.
    registry.set_hooks_gate_calls(false);
    assert!(matches!(
        registry.route_without_gate(false),
        GateRoute::Ungated
    ));
    assert!(matches!(
        registry.route_without_gate(true),
        GateRoute::Unreachable
    ));
}

#[tokio::test]
async fn the_decision_point_sees_the_argv_that_runs_and_its_refusal_names_its_rule() {
    let (registry, workdir) = registry(&["touch"]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let send: GateSender = {
        let seen = Arc::clone(&seen);
        Arc::new(move |request: ProcessGateRequest| {
            if let ResolvedCall::Shell {
                argv,
                command,
                script,
                ..
            } = &request.resolved
            {
                lock(&seen).push((argv.clone(), command.clone(), script.clone()));
            }
            let _ = request.reply.send(Some(ProcessRefusal {
                rule: crate::agent::PROTECTED_PATH_RULE,
                reason: "no".to_string(),
            }));
            true
        })
    };
    let access = ctx(&registry, "probe", GateRoute::Route(send));
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let error = view
        .run(run_request("touch", &["a b", "c"], None))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(crate::agent::PROTECTED_PATH_RULE));
    assert!(!workdir.path().join("c").exists());
    let seen = lock(&seen);
    let (argv, command, script) = &seen[0];
    assert_eq!(argv, &vec!["a b".to_string(), "c".to_string()]);
    assert_eq!(command, "'a b' c");
    assert_eq!(script, &None);
}

#[tokio::test]
async fn a_program_outside_shell_allow_is_refused_before_anything_is_asked() {
    let (registry, _workdir) = registry(&["cat"]);
    let asked = Arc::new(AtomicBool::new(false));
    let send: GateSender = {
        let asked = Arc::clone(&asked);
        Arc::new(move |request: ProcessGateRequest| {
            asked.store(true, Ordering::SeqCst);
            let _ = request.reply.send(None);
            true
        })
    };
    let access = ctx(&registry, "probe", GateRoute::Route(send));
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let error = view.run(run_request("ls", &[], None)).await.unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::SHELL_ALLOW));
    assert!(!asked.load(Ordering::SeqCst));
}

#[test]
fn process_names_are_one_to_sixty_four_safe_characters() {
    assert!(valid_process_name("web"));
    assert!(valid_process_name("a.b_c-D9"));
    assert!(valid_process_name(&"x".repeat(MAX_PROCESS_NAME_CHARS)));
    assert!(!valid_process_name(""));
    assert!(!valid_process_name(&"x".repeat(MAX_PROCESS_NAME_CHARS + 1)));
    assert!(!valid_process_name("a b"));
    assert!(!valid_process_name("../x"));
    assert!(!valid_process_name("é"));
}

#[tokio::test]
async fn an_invalid_name_is_refused_with_rule_name() {
    let (registry, _workdir) = registry(&["cat"]);
    let access = ctx(&registry, "probe", GateRoute::Ungated);
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let error = view
        .start(start_request("no/slash", "cat", &[]))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_starts_hold_the_cap_and_one_name_starts_one_process() {
    let (registry, _workdir) = registry(&["sleep"]);
    let mut starts = Vec::new();
    for index in 0..12 {
        let registry = Arc::clone(&registry);
        starts.push(tokio::spawn(async move {
            let access = ctx(&registry, "probe", GateRoute::Ungated);
            let ProcessAccess::Available(ctx) = &access else {
                unreachable!()
            };
            // Two tasks per name for the first four names; one each after.
            let name = if index < 8 {
                format!("same-{}", index / 2)
            } else {
                format!("own-{index}")
            };
            registry
                .start(ctx, start_request(&name, "sleep", &["37"]))
                .await
        }));
    }
    let mut handles = std::collections::HashSet::new();
    let mut capped = 0;
    let mut again = 0;
    for start in starts {
        match start.await.unwrap() {
            Ok(started) => {
                again += usize::from(started.already_running);
                handles.insert(started.handle);
            }
            Err(error) => {
                assert_eq!(rule_of(&error), Some(rule::CAP), "{error:?}");
                capped += 1;
            }
        }
    }
    // Four shared names and four own names: eight processes, never more.
    assert_eq!(handles.len(), 8);
    assert_eq!(again, 4);
    assert_eq!(capped, 0);
    assert_eq!(registry.running_pids().len(), MAX_SESSION_PROCESSES);

    let access = ctx(&registry, "probe", GateRoute::Ungated);
    let ProcessAccess::Available(ninth) = &access else {
        unreachable!()
    };
    let error = registry
        .start(ninth, start_request("ninth", "sleep", &["37"]))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::CAP));

    let pids = registry.running_pids();
    registry.end_session().await;
    for pid in pids {
        assert!(!pid_alive(pid), "pid {pid} outlived the session");
    }
    let error = registry
        .start(ninth, start_request("after", "sleep", &["37"]))
        .await
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::SESSION_ENDED));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_resolves_only_for_the_tool_that_started_it() {
    let (registry, _workdir) = registry(&["cat"]);
    let owner = ctx(&registry, "probe", GateRoute::Ungated);
    let other = ctx(&registry, "probe-b", GateRoute::Ungated);
    let mut table = ResourceTable::new();
    let started = {
        let mut view = ProcessView {
            access: &owner,
            table: &mut table,
        };
        view.start(start_request("echo", "cat", &[])).await.unwrap()
    };
    assert!(started.handle.starts_with(HANDLE_PREFIX));
    assert!(!started.already_running);
    let mut view = ProcessView {
        access: &other,
        table: &mut table,
    };
    assert_eq!(view.find("echo".into()).await, None);
    assert!(matches!(
        view.status(started.handle.clone()).await,
        Err(wit::ProcessError::UnknownHandle)
    ));
    let own = view.start(start_request("echo", "cat", &[])).await.unwrap();
    assert_ne!(own.handle, started.handle);
    assert!(!own.already_running);
    registry.end_session().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_caps_output_and_kills_at_its_time_limit() {
    let (registry, _workdir) = registry(&["cat", "sleep"]);
    let access = ctx(&registry, "probe", GateRoute::Ungated);
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let output = view
        .run(run_request("cat", &[], Some(b"hello\n")))
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"hello\n");
    let big = vec![b'x'; MAX_PROCESS_OUTPUT_BYTES + MAX_PROCESS_OUTPUT_BYTES / 2];
    let output = view.run(run_request("cat", &[], Some(&big))).await.unwrap();
    assert_eq!(output.stdout.len(), MAX_PROCESS_OUTPUT_BYTES);
    assert!(output.stdout_truncated);
    let started = Instant::now();
    let mut request = run_request("sleep", &["31"], None);
    request.timeout_ms = Some(300);
    let output = view.run(request).await.unwrap();
    assert!(output.timed_out);
    assert_eq!(output.exit_code, 137);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn the_output_ring_keeps_the_newest_bytes_and_counts_the_rest() {
    let mut ring = OutputRing::default();
    ring.push(&vec![b'a'; MAX_PROCESS_OUTPUT_BYTES]);
    ring.push(b"bc");
    let (first, dropped) = ring.take(1);
    assert_eq!(dropped, 2);
    assert_eq!(first, b"a");
    let (_, dropped) = ring.take(usize::MAX);
    assert_eq!(dropped, 0);
    assert!(ring.take(10).0.is_empty());
}

#[test]
fn stat_pgrp_reads_past_a_command_name_with_spaces_and_parentheses() {
    assert_eq!(stat_pgrp("42 (a (b) c) S 1 4242 4242 0"), Some(4242));
}

/// A `/proc`-shaped tree: pid 100 leads group 100 and holds socket 555; pid 200 is in group 100
/// too; pid 300 is in group 300 and holds socket 777.
fn constructed_proc(tcp: &str, tcp6: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (pid, pgrp, sockets) in [
        (100, 100, vec![555u64]),
        (200, 100, vec![]),
        (300, 300, vec![777]),
    ] {
        let dir = root.path().join(pid.to_string());
        std::fs::create_dir_all(dir.join("fd")).unwrap();
        std::fs::create_dir_all(dir.join("net")).unwrap();
        std::fs::write(dir.join("stat"), format!("{pid} (x y) S 1 {pgrp} {pgrp} 0")).unwrap();
        std::fs::write(dir.join("net/tcp"), tcp).unwrap();
        std::fs::write(dir.join("net/tcp6"), tcp6).unwrap();
        for (index, inode) in sockets.into_iter().enumerate() {
            std::os::unix::fs::symlink(
                format!("socket:[{inode}]"),
                dir.join("fd").join(index.to_string()),
            )
            .unwrap();
        }
    }
    root
}

const TCP_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

fn tcp_line(address: &str, port: u16, state: &str, inode: u64) -> String {
    format!("   0: {address}:{port:04X} 00000000:0000 {state} 00000000:00000000 00:00000000 00000000  1000        0 {inode} 1 0000000000000000 100 0 0 10 0\n")
}

#[test]
fn connect_reaches_only_a_port_the_handles_group_listens_on() {
    let loopback = "0100007F";
    let tcp = format!(
        "{TCP_HEADER}{}{}{}{}",
        tcp_line(loopback, 8000, "0A", 555),
        tcp_line(loopback, 9000, "0A", 777),
        tcp_line("00000000", 443, "0A", 999),
        tcp_line(loopback, 8001, "01", 555),
    );
    let root = constructed_proc(&tcp, TCP_HEADER);
    let proc_fs = ProcFs::at(root.path());
    assert_eq!(
        owned_listener(&proc_fs, 100, 8000),
        Some(ListenerFamily::V4)
    );
    // Another group's listener, a listener nobody in the group holds, a socket not listening,
    // and a port nothing listens on.
    assert_eq!(owned_listener(&proc_fs, 100, 9000), None);
    assert_eq!(owned_listener(&proc_fs, 100, 443), None);
    assert_eq!(owned_listener(&proc_fs, 100, 8001), None);
    assert_eq!(owned_listener(&proc_fs, 100, 1234), None);
    assert_eq!(
        owned_listener(&proc_fs, 300, 9000),
        Some(ListenerFamily::V4)
    );
}

#[test]
fn connect_ignores_a_listener_bound_to_a_non_loopback_address() {
    let tcp = format!("{TCP_HEADER}{}", tcp_line("0102A8C0", 8000, "0A", 555));
    let tcp6 = format!(
        "{TCP_HEADER}{}",
        tcp_line("00000000000000000000000001000000", 8100, "0A", 555)
    );
    let root = constructed_proc(&tcp, &tcp6);
    let proc_fs = ProcFs::at(root.path());
    assert_eq!(owned_listener(&proc_fs, 100, 8000), None);
    assert_eq!(
        owned_listener(&proc_fs, 100, 8100),
        Some(ListenerFamily::V6)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_reaches_a_real_listener_of_the_handles_own_group() {
    if std::process::Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_err()
    {
        crate::runtime_err!("[SKIP-HOST] no python3 on this host");
        return;
    }
    let (registry, _workdir) = registry(&["python3"]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_port = listener.local_addr().unwrap().port();
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let access = ctx(&registry, "probe", GateRoute::Ungated);
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };
    let script = format!(
        "import socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',{port}))\ns.listen()\nprint('up',flush=True)\nc,_=s.accept()\nc.sendall(b'pong')\nc.close()\ntime.sleep(30)\n"
    );
    let started = view
        .start(start_request("srv", "python3", &["-c", &script]))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("up") {
        assert!(Instant::now() < deadline, "the listener never came up");
        seen.extend(
            wit::Host::read(&mut view, started.handle.clone(), 4096)
                .await
                .unwrap()
                .stdout,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A port another process of the host holds is not this group's.
    let error = view
        .connect(started.handle.clone(), busy_port)
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(rule_of(&error), Some(rule::CONNECT));
    let connection = view.connect(started.handle.clone(), port).await.unwrap();
    let bytes =
        wit::HostConnection::read(&mut view, Resource::new_borrow(connection.rep()), 16, 5000)
            .await;
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => panic!("{error:?}"),
    };
    assert_eq!(bytes, b"pong");
    drop(listener);
    registry.end_session().await;
}

/// Whether `pid` has died (gone, or a zombie awaiting its reaper), waiting up to five seconds.
async fn dies(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let close = stat.rfind(')')?;
                stat[close + 1..]
                    .split_whitespace()
                    .next()
                    .map(str::to_owned)
            });
        if state.is_none_or(|state| state == "Z") {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The pid a `sh -c '<child> & echo $!'` program printed.
fn printed_pid(stdout: &[u8]) -> i32 {
    String::from_utf8_lossy(stdout).trim().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_program_that_exits_takes_what_it_forked_with_it() {
    let (registry, _workdir) = registry(&["sh"]);
    let access = ctx(&registry, "probe", GateRoute::Ungated);
    let mut table = ResourceTable::new();
    let mut view = ProcessView {
        access: &access,
        table: &mut table,
    };

    let output = view
        .run(run_request("sh", &["-c", "sleep 34 & echo $!"], None))
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    let forked = printed_pid(&output.stdout);
    assert!(
        dies(forked).await,
        "the run's forked pid {forked} outlived it"
    );

    let started = view
        .start(start_request("fork", "sh", &["-c", "sleep 33 & echo $!"]))
        .await
        .unwrap();
    let leader = registry.lookup("probe", &started.handle).unwrap();
    assert!(leader.wait_exit(Duration::from_secs(10)).await);
    assert!(matches!(
        view.status(started.handle.clone()).await,
        Ok(wit::State::Exited(0))
    ));
    let output = view.read(started.handle.clone(), 4096).await.unwrap();
    let forked = printed_pid(&output.stdout);
    assert!(
        dies(forked).await,
        "pid {forked} outlived its exited leader {}",
        leader.pgid()
    );
    assert!(lock(&registry.state).live_count() == 0);

    registry.end_session().await;
    let pgid = leader.pgid().to_string();
    let left: Vec<_> = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("stat")).is_ok_and(|stat| {
                stat.rfind(')').is_some_and(|close| {
                    let fields: Vec<_> = stat[close + 1..].split_whitespace().collect();
                    fields.first() != Some(&"Z") && fields.get(2) == Some(&pgid.as_str())
                })
            })
        })
        .map(|entry| entry.file_name())
        .collect();
    assert!(left.is_empty(), "group {pgid} still holds {left:?}");
}
