//! What this host is, recorded so a run can never be cited out of context.
//!
//! Critical execution requirement: the suite runs on bare metal. A
//! container previously masked three separate findings — the raw-disk escape, the `docker.sock`
//! escape, and the entire syscall surface all looked closed inside Docker and were wide open
//! outside it. A suite that only ran in a container would have certified a broken boundary. So
//! container detection is not advisory here; by default it is a refusal, and the flag that
//! overrides it stamps the record in a way nobody can cite by accident.

use std::fmt;
use std::path::Path;
use std::process::Command;

/// Facts about the machine a run happened on. Every field lands in the dated record.
#[derive(Debug, Clone)]
pub struct HostFacts {
    /// `uname -r`.
    pub kernel_release: String,
    /// `uname -sm`, so the record is unambiguous about OS and architecture.
    pub kernel_system: String,
    pub arch: &'static str,
    pub os: &'static str,
    /// Effective uid. Several cases can only be attributed on a root run (`mknod`, `bpf`,
    /// `open_by_handle_at`), so this is not a curiosity — it changes what the results mean.
    pub euid: String,
    pub container: ContainerDetection,
    /// `stat -fc %T /sys/fs/cgroup` equivalent: whether this host is cgroup v2 unified. The
    /// resource-exhaustion category needs a delegated cgroup v2 scope; without one `mur run`
    /// refuses with `E-RUN-012` and those cases cannot report anything.
    pub cgroup_v2: Option<bool>,
}

/// Whether this looks like a container, and what said so.
#[derive(Debug, Clone)]
pub struct ContainerDetection {
    pub detected: bool,
    /// Every signal checked, with its result — recorded whether or not anything fired, so a
    /// bare-metal record positively states that the checks ran and came back clean rather than
    /// merely omitting them.
    pub signals: Vec<(String, String)>,
}

impl ContainerDetection {
    /// The signals that fired, joined for a one-line summary.
    pub fn firing(&self) -> String {
        let fired: Vec<&str> = self
            .signals
            .iter()
            .filter(|(_, result)| result.starts_with("DETECTED"))
            .map(|(name, _)| name.as_str())
            .collect();
        if fired.is_empty() {
            "none".to_string()
        } else {
            fired.join(", ")
        }
    }
}

impl fmt::Display for ContainerDetection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detected {
            write!(f, "CONTAINER DETECTED ({})", self.firing())
        } else {
            f.write_str("no container signal")
        }
    }
}

/// Runs `program` with `args` and returns trimmed stdout, or `None` if it could not run.
fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The container heuristics, plus podman's equivalent of `/.dockerenv`.
///
/// Deliberately a fixed, short list rather than an exhaustive one: the point is that a record
/// states plainly which checks ran and what each said, so a reader can judge the evidence. A long
/// list of half-reliable heuristics would make a clean result harder to trust, not easier.
pub fn detect_container() -> ContainerDetection {
    let mut signals = Vec::new();
    let mut detected = false;

    for marker in ["/.dockerenv", "/run/.containerenv"] {
        let present = Path::new(marker).exists();
        detected |= present;
        signals.push((
            format!("{marker} present"),
            if present {
                "DETECTED".to_string()
            } else {
                "absent".to_string()
            },
        ));
    }

    // `/proc/1/cgroup` on a bare-metal systemd host reads `0::/init.scope`; inside a container
    // the path carries the runtime's own name.
    match std::fs::read_to_string("/proc/1/cgroup") {
        Ok(content) => {
            let lower = content.to_ascii_lowercase();
            let hits: Vec<&str> = ["docker", "containerd", "lxc", "kubepods", "podman"]
                .into_iter()
                .filter(|needle| lower.contains(needle))
                .collect();
            detected |= !hits.is_empty();
            signals.push((
                "/proc/1/cgroup substring".to_string(),
                if hits.is_empty() {
                    format!("clean ({})", content.trim().replace('\n', " | "))
                } else {
                    format!("DETECTED: {}", hits.join(", "))
                },
            ));
        }
        Err(err) => signals.push((
            "/proc/1/cgroup substring".to_string(),
            format!("unreadable ({err}) — not a container signal either way"),
        )),
    }

    ContainerDetection { detected, signals }
}

/// Probes this host. Cheap and read-only: nothing here launches a capsule or creates a workdir.
pub fn probe() -> HostFacts {
    let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
        .or_else(|| capture("uname", &["-r"]))
        .unwrap_or_else(|| "unknown".to_string());

    let cgroup_v2 = capture("stat", &["-fc", "%T", "/sys/fs/cgroup"]).map(|t| t == "cgroup2fs");

    HostFacts {
        kernel_release,
        kernel_system: capture("uname", &["-sm"]).unwrap_or_else(|| "unknown".to_string()),
        arch: std::env::consts::ARCH,
        os: std::env::consts::OS,
        euid: capture("id", &["-u"]).unwrap_or_else(|| "unknown".to_string()),
        container: detect_container(),
        cgroup_v2,
    }
}

impl HostFacts {
    /// True when the effective uid is definitely 0. Several cases say in their attribution note
    /// that only a root run makes their refusal attributable; the record repeats this once at the
    /// top rather than leaving a reader to work it out per row.
    pub fn is_root(&self) -> bool {
        self.euid == "0"
    }
}
