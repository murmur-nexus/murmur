//! Freshness of the host capability probe.
//!
//! The unit under test is not what the probe detects — `sandbox`'s own unit tests cover that —
//! but how long an answer is trusted: every entry point that reports a host fact reads the host
//! at the moment it is called, so nothing in a long-lived process is judged against a reading
//! taken before it started.

// Test output, not a capsule's: this target's stdout belongs to the `cargo test` harness.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use capsule_runtime::{
    detect_achieved_containment, detect_sealed_shortfall, detect_userns_grant, HostProbe,
    MountAttempt,
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
            detect_sealed_shortfall();
        }),
        1,
        "detect_sealed_shortfall must read the host once per call",
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

/// One reading answers every namespace question from its attempts, on whatever host this runs:
/// `/tmp` is mappable exactly when the network and mount attempts both created their namespaces,
/// `E-CAP-005` is reported exactly when the network attempt failed, a failed network attempt
/// attempts nothing after it, and `sealed` is backed only when the sealed root was built.
#[test]
fn one_reading_answers_every_namespace_question_from_its_attempts() {
    let probe = HostProbe::probe();
    let namespaces = probe.namespaces();
    println!("namespace reading: {namespaces:?}");
    assert_eq!(
        namespaces.mount_namespace_created(),
        namespaces.network.is_ok() && namespaces.mount == MountAttempt::Created
    );
    if cfg!(target_os = "linux") {
        assert_eq!(
            probe.egress_namespace_failure().is_some(),
            namespaces.network.is_err(),
            "E-CAP-005 must follow the network attempt"
        );
    }
    if namespaces.network.is_err() {
        assert_eq!(namespaces.mount, MountAttempt::NotAttempted);
        assert_eq!(namespaces.sealed_root, MountAttempt::NotAttempted);
    }
    if probe.sealed_shortfall().is_none() {
        assert!(namespaces.sealed_root_created());
    }
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
    use capsule_runtime::{SealedBlocker, SealedShortfall};

    const SYSCTL: &str = "/proc/sys/user/max_user_namespaces";
    const INVOCATION: &str = "unshare --user --map-root-user cargo test -p capsule-runtime \
                              --test host_probe -- --ignored \
                              reprobe_observes_a_change_in_userns_availability";

    // A shortfall of `None` or `LandlockUnavailable` is exactly the set in which every namespace
    // attempt of the probe created its namespace: every other blocker names an attempt that
    // failed.
    fn namespace_ok(shortfall: &Option<SealedShortfall>) -> bool {
        matches!(
            shortfall.as_ref().map(|shortfall| shortfall.blocker),
            None | Some(SealedBlocker::LandlockUnavailable)
        )
    }

    let first = HostProbe::probe();
    let first_shortfall = first.sealed_shortfall();
    println!(
        "probe #1: sealed_shortfall {first_shortfall:?}, network {:?}",
        first.namespaces().network
    );
    if !namespace_ok(&first_shortfall) || first.egress_namespace_failure().is_some() {
        // The recorded waiver: this host cannot create an unprivileged user namespace at all, so
        // there is no capability present to take away.
        panic!(
            "this host reports {first_shortfall:?} before any change, so it has no \
             user-namespace capability to withdraw. Run on a bare Linux host as: {INVOCATION}"
        );
    }

    let original = std::fs::read_to_string(SYSCTL)
        .unwrap_or_else(|error| panic!("cannot read {SYSCTL}: {error}. Run as: {INVOCATION}"));
    std::fs::write(SYSCTL, "0")
        .unwrap_or_else(|error| panic!("cannot write {SYSCTL}: {error}. Run as: {INVOCATION}"));

    let second = HostProbe::probe();
    let withdrawn = second.sealed_shortfall();
    let egress = second.egress_namespace_failure();
    println!("probe #2 (max_user_namespaces=0): sealed_shortfall {withdrawn:?}, egress {egress:?}");
    let restore = || std::fs::write(SYSCTL, original.trim());

    if namespace_ok(&withdrawn) || egress.is_none() || second.namespaces().network.is_ok() {
        let _ = restore();
        panic!("probe #2 still reports a namespace after {SYSCTL} was zeroed: {withdrawn:?}");
    }
    assert_eq!(
        withdrawn.as_ref().map(|shortfall| shortfall.blocker),
        Some(SealedBlocker::KernelUnsupported)
    );
    assert_eq!(
        egress.as_ref().and_then(|failure| failure.errno),
        Some(libc::ENOSPC)
    );
    assert_ne!(
        detect_achieved_containment(),
        murmur_artifact::ContainmentClass::Sealed,
        "a host that cannot create a user namespace must not report a sealed ceiling",
    );

    restore().unwrap_or_else(|error| {
        panic!("the kernel refused to restore {SYSCTL} to {original:?}: {error}")
    });
    let third = HostProbe::probe();
    println!(
        "probe #3 (restored): sealed_shortfall {:?}, network {:?}",
        third.sealed_shortfall(),
        third.namespaces().network
    );
    assert_eq!(
        third.sealed_shortfall(),
        first_shortfall,
        "a condition that has cleared must stop being reported without restarting the process",
    );
    assert!(third.namespaces().network.is_ok());
    assert_eq!(third.egress_namespace_failure(), None);
}

/// Hand-run:
///
/// ```text
/// cargo test -p capsule-runtime --test host_probe -- --ignored --nocapture probe_cost
/// ```
///
/// The ceiling is far above any plausible cost for four `fork`/`waitpid` pairs and three small
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
