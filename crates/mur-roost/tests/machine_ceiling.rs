//! The host's own ceiling: how many capsules one daemon may hold live across every formation.
//!
//! Every case drives the same two-request exchange a real launch makes, against real artifacts in
//! a real registry. The census this bound is decided from is the daemon's own record of what is
//! running, so each case builds that record by registering and deregistering rather than by
//! setting a counter.

#[path = "common/mod.rs"]
mod common;

use std::process::Command;

use common::{Daemon, PLAIN_WORKER_BODY};
use mur_roost::bounds::{MAX_MAX_LIVE_CAPSULES, MIN_MAX_LIVE_CAPSULES};

/// A capsule that may spawn `worker-a`, so a registered root can delegate.
const ORCHESTRATOR_BODY: &str = "artifacts: []\ncapabilities:\n  network:\n    allow: \
                                 [registry.internal]\n  spawn:\n    allow: [worker-a]\n";

/// Register a root of the given capsule with no approval, and return the credential it delegates
/// with.
fn root(daemon: &Daemon, capsule: &str) -> String {
    let session = daemon.child_session_id();
    let registered = daemon.register(&session, capsule, "0.1.0", None);
    assert_eq!(registered.status, 200, "{:?}", registered.body);
    registered.body["credential"].as_str().unwrap().to_string()
}

/// A daemon that publishes an orchestrator and a worker, and admits the orchestrator at the
/// top level.
fn orchestrating_daemon(max_depth: u32, max_concurrent: u32, max_live_capsules: u32) -> Daemon {
    let daemon = Daemon::capped(
        vec!["orchestrator".to_string()],
        max_depth,
        max_concurrent,
        max_live_capsules,
    );
    daemon.publish_body("orchestrator", "0.1.0", ORCHESTRATOR_BODY, "");
    daemon.publish_body("worker-a", "0.1.0", PLAIN_WORKER_BODY, "");
    daemon
}

// ── 1. The ceiling counts every formation, not the asking one ─────────────────

/// Three unrelated roots, none of them anybody's parent, fill a ceiling of three. The first root's
/// `POST /spawn` is refused even though it holds no children at all.
#[test]
fn unrelated_roots_exhaust_the_host_ceiling() {
    let daemon = orchestrating_daemon(3, 4, 3);
    let first = root(&daemon, "orchestrator");
    root(&daemon, "orchestrator");
    root(&daemon, "orchestrator");

    let refusal = daemon.permission("worker-a", "0.1.0", Some(&first));
    assert_eq!(refusal.status, 403);
    assert_eq!(
        refusal.error(),
        "machine capsule ceiling reached: this daemon allows 3 live capsules on this host across \
         every formation (--max-live-capsules 3), and it is already holding 3 — this is the host's \
         ceiling rather than this capsule's own --max-concurrent, so narrowing the formation does \
         not help; the same request may be granted once other capsules finish",
    );
    assert!(
        !refusal.error().contains("--max-concurrent 4"),
        "{}",
        refusal.error()
    );

    // The parent was inside its own ceiling and was refused anyway.
    let sessions = daemon.session_ids();
    assert_eq!(sessions.len(), 3, "{sessions:?}");
    let asking = &sessions[0];
    assert_eq!(daemon.status(asking).body["live_children"], 0);
    assert_eq!(daemon.status(asking).body["live_capsules"], 3);
    for session in &sessions {
        assert!(daemon
            .state
            .jobs
            .lock()
            .unwrap()
            .get(session)
            .unwrap()
            .pending
            .is_empty());
    }
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

// ── 2. The two refusals are distinguishable, and the machine one wins ─────────

/// A parent at its own `--max-concurrent` with the host far from full hears the per-parent
/// refusal; a parent with no children on a full host hears the machine one; a parent at both hears
/// the machine one.
#[test]
fn the_machine_refusal_wins_and_names_only_its_own_flag() {
    // (a) Room on the host, none in the formation.
    let daemon = orchestrating_daemon(3, 1, 32);
    let credential = root(&daemon, "orchestrator");
    let child = daemon.child_session_id();
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    assert_eq!(
        daemon
            .register(&child, "worker-a", "0.1.0", Some(&approval))
            .status,
        200,
    );
    let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert!(
        refusal.error().contains("--max-concurrent 1"),
        "{}",
        refusal.error()
    );
    assert!(
        !refusal.error().contains("--max-live-capsules"),
        "{}",
        refusal.error()
    );

    // (b) Room in the formation, none on the host.
    let daemon = orchestrating_daemon(3, 4, 2);
    let credential = root(&daemon, "orchestrator");
    root(&daemon, "orchestrator");
    let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert!(
        refusal.error().contains("--max-live-capsules 2"),
        "{}",
        refusal.error()
    );
    assert!(
        !refusal.error().contains("--max-concurrent 4"),
        "{}",
        refusal.error()
    );

    // (c) Neither has room: one root holding its single allowed child is also the whole host.
    let daemon = orchestrating_daemon(3, 1, 2);
    let credential = root(&daemon, "orchestrator");
    let child = daemon.child_session_id();
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    assert_eq!(
        daemon
            .register(&child, "worker-a", "0.1.0", Some(&approval))
            .status,
        200,
    );
    assert_eq!(daemon.status(&child).body["live_capsules"], 2);
    let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert!(
        refusal.error().contains("--max-live-capsules 2"),
        "{}",
        refusal.error()
    );
    // The contrast clause names --max-concurrent to say the bound is *not* that one; what it
    // never does is quote it as the figure to raise.
    assert!(
        !refusal.error().contains("--max-concurrent 1"),
        "{}",
        refusal.error()
    );
}

/// Depth still outranks both: no amount of waiting gives a spent session another level, so a
/// session at `0` hears the depth refusal even on a full host.
#[test]
fn depth_still_outranks_the_machine_ceiling() {
    let daemon = orchestrating_daemon(0, 4, 1);
    let credential = root(&daemon, "orchestrator");
    let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert_eq!(
        refusal.error(),
        "delegation depth bound reached: this daemon allows 0 levels of delegation below a \
         top-level capsule (--max-depth 0), and this session has none left to spend — a capsule \
         whose capabilities.spawn.allow names itself terminates here rather than recursing",
    );
}

// ── 3. The default is real, derived, and out of a small formation's way ───────

/// Started with no `--max-live-capsules`, the daemon reports the ceiling it derived, so an
/// operator who never set the flag can still read the number they are running under.
#[test]
fn the_daemon_reports_the_ceiling_it_derived() {
    let registry = tempfile::TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
        .args([
            "--port",
            "0",
            "--registry-path",
            registry.path().to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let stderr = read_startup_lines(&mut child);
    child.kill().unwrap();
    child.wait().unwrap();

    let reported = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("mur-roost: machine ceiling ")?
                .strip_suffix(" live capsules (--max-live-capsules)")
        })
        .unwrap_or_else(|| panic!("startup names its machine ceiling, got: {stderr}"))
        .parse::<u32>()
        .unwrap();
    assert!(
        (MIN_MAX_LIVE_CAPSULES..=MAX_MAX_LIVE_CAPSULES).contains(&reported),
        "{reported}",
    );
}

/// The flag reports its own name on a value that is not a number, as `--max-depth` does.
#[test]
fn the_ceiling_flag_rejects_a_value_that_is_not_a_number() {
    let registry = tempfile::TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
        .args([
            "--max-live-capsules",
            "notanumber",
            "--registry-path",
            registry.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mur-roost: invalid --max-live-capsules: "),
        "{stderr}",
    );
}

/// One small formation never meets the derived ceiling: a root and two full delegations under the
/// default a plain install gets.
#[test]
fn a_small_formation_never_meets_the_default_ceiling() {
    let daemon = Daemon::with_spawn_allow(vec!["orchestrator".to_string()]);
    daemon.publish_body("orchestrator", "0.1.0", ORCHESTRATOR_BODY, "");
    daemon.publish_body("worker-a", "0.1.0", PLAIN_WORKER_BODY, "");
    let credential = root(&daemon, "orchestrator");

    for _ in 0..2 {
        let permission = daemon.permission("worker-a", "0.1.0", Some(&credential));
        assert_eq!(permission.status, 200, "{:?}", permission.body);
        let approval = permission.body["approval"].as_str().unwrap();
        let registered = daemon.register(
            &daemon.child_session_id(),
            "worker-a",
            "0.1.0",
            Some(approval),
        );
        assert_eq!(registered.status, 200, "{:?}", registered.body);
    }
}

// ── 4. Slots come back ────────────────────────────────────────────────────────

/// Ten formations in sequence under a ceiling of two, then an eleventh. A daemon that has cycled a
/// hundred formations refuses no more than one that has cycled none, even though a `JobRecord` is
/// never removed from the map.
#[test]
fn a_finished_capsule_returns_its_machine_slot() {
    let daemon = orchestrating_daemon(3, 4, 2);

    for round in 0..11 {
        let credential = root(&daemon, "orchestrator");
        let permission = daemon.permission("worker-a", "0.1.0", Some(&credential));
        assert_eq!(
            permission.status, 200,
            "round {round}: {:?}",
            permission.body
        );
        let approval = permission.body["approval"].as_str().unwrap().to_string();
        let child = daemon.child_session_id();
        let registered = daemon.register(&child, "worker-a", "0.1.0", Some(&approval));
        assert_eq!(
            registered.status, 200,
            "round {round}: {:?}",
            registered.body
        );
        let child_credential = registered.body["credential"].as_str().unwrap().to_string();

        assert_eq!(daemon.status(&child).body["live_capsules"], 2);
        assert_eq!(daemon.deregister(&child_credential, "complete").status, 200);
        assert_eq!(daemon.deregister(&credential, "complete").status, 200);
    }

    // Eleven roots and eleven children are still in the map, and none of them holds a slot.
    assert_eq!(daemon.session_ids().len(), 22);
    let credential = root(&daemon, "orchestrator");
    let granted = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(granted.status, 200, "{:?}", granted.body);
}

/// An approval nobody has redeemed holds a machine slot, so two asks under a ceiling of two with
/// one session already running do not both pass.
#[test]
fn an_unredeemed_approval_holds_a_machine_slot() {
    let daemon = orchestrating_daemon(3, 4, 2);
    let credential = root(&daemon, "orchestrator");

    let first = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(
        daemon.status(&daemon.session_ids()[0]).body["live_capsules"],
        2
    );

    let second = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(second.status, 403);
    assert!(
        second.error().contains("already holding 2"),
        "{}",
        second.error()
    );
}

// ── 5. A zero ceiling, and no queueing ────────────────────────────────────────

/// `--max-live-capsules 0` refuses every delegation and no registration: the operator starting a
/// capsule from their own terminal is not what this bound admits.
#[test]
fn a_zero_ceiling_refuses_every_delegation_but_no_root() {
    let daemon = orchestrating_daemon(3, 4, 0);
    let session = daemon.child_session_id();
    let registered = daemon.register(&session, "orchestrator", "0.1.0", None);
    assert_eq!(registered.status, 200, "{:?}", registered.body);
    let credential = registered.body["credential"].as_str().unwrap().to_string();

    let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert!(
        refusal.error().contains("--max-live-capsules 0"),
        "{}",
        refusal.error()
    );
}

/// Nothing is queued behind a refusal. Five identical refused asks all answer immediately, none is
/// granted after the others, and the daemon holds nothing for any of them.
#[test]
fn a_refused_spawn_is_answered_and_forgotten() {
    let daemon = orchestrating_daemon(3, 4, 2);
    let credential = root(&daemon, "orchestrator");
    root(&daemon, "orchestrator");
    let before = daemon.session_ids();

    for attempt in 0..5 {
        let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
        assert_eq!(refusal.status, 403, "attempt {attempt}");
        assert!(
            refusal.error().contains("--max-live-capsules 2"),
            "attempt {attempt}: {}",
            refusal.error()
        );
    }

    assert_eq!(daemon.session_ids(), before);
    for record in daemon.state.jobs.lock().unwrap().values() {
        assert!(record.pending.is_empty());
    }
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

/// The daemon prints its startup lines and then blocks on the accept loop forever, so stderr is
/// read line by line and stops at the ceiling line rather than to end of stream.
fn read_startup_lines(child: &mut std::process::Child) -> String {
    use std::io::{BufRead, BufReader};

    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut collected = String::new();
    for _ in 0..8 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let ceiling = line.contains("machine ceiling");
        collected.push_str(&line);
        if ceiling {
            break;
        }
    }
    collected
}
