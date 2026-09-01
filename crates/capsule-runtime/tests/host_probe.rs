//! Freshness of the host capability probe.
//!
//! The unit under test is not what the probe detects — `sandbox`'s own unit tests cover that —
//! but how long an answer is trusted: every entry point that reports a host fact reads the host
//! at the moment it is called, so nothing in a long-lived process is judged against a reading
//! taken before it started.

use capsule_runtime::{
    detect_achieved_containment, detect_sealed_blocker, detect_userns_grant, HostProbe,
};

/// `HostProbe::probes_taken` counts the whole process, so the two tests that assert on deltas
/// must not overlap: without this they see each other's probes and read a correct implementation
/// as a wrong count.
static COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn two_probes_are_two_readings() {
    let _serial = COUNTER.lock().expect("counter lock");
    let before = HostProbe::probes_taken();
    let _first = HostProbe::probe();
    let _second = HostProbe::probe();
    assert_eq!(
        HostProbe::probes_taken() - before,
        2,
        "each `HostProbe::probe` call must read the host, not a value cached for the process",
    );
}

/// `detect_enforcement_tier` is crate-private, so the tier question is asked here through
/// `detect_achieved_containment`, the public wrapper `mur doctor` and the escape-conformance
/// harness call.
#[test]
fn each_standalone_host_question_takes_its_own_reading() {
    let _serial = COUNTER.lock().expect("counter lock");

    fn probes_during(ask: impl FnOnce()) -> u64 {
        let before = HostProbe::probes_taken();
        ask();
        HostProbe::probes_taken() - before
    }

    assert_eq!(
        probes_during(|| {
            detect_sealed_blocker();
        }),
        1,
        "detect_sealed_blocker must read the host once per call",
    );
    assert_eq!(
        probes_during(|| {
            detect_userns_grant();
        }),
        1,
        "detect_userns_grant must read the host once per call",
    );
    assert_eq!(
        probes_during(|| {
            detect_achieved_containment();
        }),
        1,
        "detect_achieved_containment must read the host once per call",
    );
}

/// Hand-run, on a bare Linux host where an unprivileged user can create a user namespace:
///
/// ```text
/// unshare --user --map-root-user cargo test -p capsule-runtime --test host_probe \
///     -- --ignored reprobe_observes_a_change_in_userns_availability
/// ```
///
/// Inside that namespace the process owns `/proc/sys/user/max_user_namespaces` and can take the
/// capability away mid-run, which is the only way to prove a second probe in one process really
/// re-reads the host rather than replaying the first answer — and, on the restore, that a host
/// condition which has since cleared stops being reported.
#[test]
#[ignore = "mutates /proc/sys/user/max_user_namespaces; hand-run under `unshare --user --map-root-user`"]
#[cfg(target_os = "linux")]
fn reprobe_observes_a_change_in_userns_availability() {
    use capsule_runtime::SealedBlocker;

    const SYSCTL: &str = "/proc/sys/user/max_user_namespaces";
    const INVOCATION: &str = "unshare --user --map-root-user cargo test -p capsule-runtime \
                              --test host_probe -- --ignored \
                              reprobe_observes_a_change_in_userns_availability";

    // A blocker of `None` or `LandlockUnavailable` is exactly the set in which the namespace half
    // of the probe reported `Ok`: every other variant names a namespace step that failed, or a
    // grant that was withheld before the namespace was ever attempted.
    fn namespace_ok(blocker: Option<SealedBlocker>) -> bool {
        matches!(blocker, None | Some(SealedBlocker::LandlockUnavailable))
    }

    let first = HostProbe::probe().sealed_blocker();
    println!("probe #1: sealed_blocker {first:?}");
    if !namespace_ok(first) {
        // The recorded waiver: this host cannot create an unprivileged user namespace at all, so
        // there is no capability present to take away.
        panic!(
            "this host reports {first:?} before any change, so it has no user-namespace \
             capability to withdraw. Run on a bare Linux host as: {INVOCATION}"
        );
    }

    let original = std::fs::read_to_string(SYSCTL)
        .unwrap_or_else(|error| panic!("cannot read {SYSCTL}: {error}. Run as: {INVOCATION}"));
    std::fs::write(SYSCTL, "0")
        .unwrap_or_else(|error| panic!("cannot write {SYSCTL}: {error}. Run as: {INVOCATION}"));

    let second = HostProbe::probe();
    let withdrawn = second.sealed_blocker();
    println!("probe #2 (max_user_namespaces=0): sealed_blocker {withdrawn:?}");
    let restore = || std::fs::write(SYSCTL, original.trim());

    if namespace_ok(withdrawn) {
        let _ = restore();
        panic!("probe #2 still reports {withdrawn:?} after {SYSCTL} was zeroed");
    }
    assert_ne!(
        detect_achieved_containment(),
        murmur_artifact::ContainmentClass::Sealed,
        "a host that cannot create a user namespace must not report a sealed ceiling",
    );

    restore().unwrap_or_else(|error| {
        panic!("the kernel refused to restore {SYSCTL} to {original:?}: {error}")
    });
    let third = HostProbe::probe().sealed_blocker();
    println!("probe #3 (restored): sealed_blocker {third:?}");
    assert_eq!(
        third, first,
        "a condition that has cleared must stop being reported without restarting the process",
    );
}

/// Hand-run:
///
/// ```text
/// cargo test -p capsule-runtime --test host_probe -- --ignored --nocapture probe_cost
/// ```
///
/// The ceiling is far above any plausible cost for two `fork`/`waitpid` pairs and three small
/// `/proc` reads, so a failure here means something pathological rather than merely slow.
#[test]
#[ignore = "measures host syscall cost; hand-run with --nocapture"]
fn probe_cost() {
    const ITERATIONS: u32 = 100;

    let start = std::time::Instant::now();
    for _ in 0..ITERATIONS {
        let _ = HostProbe::probe();
    }
    let total = start.elapsed();
    let mean = total / ITERATIONS;
    println!("HostProbe::probe x{ITERATIONS}: total {total:?}, mean {mean:?}");

    assert!(
        mean < std::time::Duration::from_millis(50),
        "mean probe cost {mean:?} is pathological, not merely slow",
    );
}
