//! What this host is, recorded so a run can never be cited out of context.
//!
//! Critical execution requirement: `W-SEC-005` evidence comes from bare metal. A
//! container previously masked three separate findings — the raw-disk escape, the `docker.sock`
//! escape, and the entire syscall surface all looked closed inside Docker and were wide open
//! outside it. A suite that only ran in a container would have certified a broken boundary. So
//! container and pod detection is not advisory here; by default it is a refusal, and the flag that
//! overrides it stamps the record in a way nobody can cite by accident. A VM runs — the CI job is
//! one — and its record is stamped the same way.
//!
//! The work root's filesystem is recorded here too: a work root on a different filesystem from
//! `/etc` and `/var/tmp` turns two boundary cases into `EXDEV` false negatives, so it stamps the
//! record as well. See [`WorkRootFilesystem`].

use std::fmt;
use std::path::{Path, PathBuf};
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
    /// What kind of machine this is — bare host, VM, container or pod — and every signal that
    /// was read to decide it.
    pub environment: EnvironmentDetection,
    /// `stat -fc %T /sys/fs/cgroup` equivalent: whether this host is cgroup v2 unified. The
    /// resource-exhaustion category needs a delegated cgroup v2 scope; without one `mur run`
    /// refuses with `E-RUN-012` and those cases cannot report anything.
    pub cgroup_v2: Option<bool>,
}

/// The kind of machine a run happened on.
///
/// Only [`Environment::BareHost`] produces a record citable as `W-SEC-005` evidence. A container
/// or a pod is refused before any case runs unless `--allow-container` overrides it, because its
/// own boundary answers in place of this runtime's. A VM is not refused — the CI job that runs
/// this suite is one — but the hypervisor sits between the runtime and the hardware, so its
/// record is not the bare-host evidence `W-SEC-005` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Environment {
    BareHost,
    VirtualMachine,
    Container,
    Pod,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::BareHost => "bare host",
            Environment::VirtualMachine => "virtual machine",
            Environment::Container => "container",
            Environment::Pod => "pod",
        }
    }

    /// Whether a run here is refused unless `--allow-container` is passed.
    pub fn is_refused_by_default(self) -> bool {
        matches!(self, Environment::Container | Environment::Pod)
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One environment check and what it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    /// What was checked, as the record's signal table names it.
    pub name: String,
    /// What it read. Starts `DETECTED` when the signal fired.
    pub result: String,
    /// The environment this signal is evidence of, when it fired.
    pub fires: Option<Environment>,
}

impl Signal {
    fn new(name: &str, result: String, fires: Option<Environment>) -> Signal {
        Signal {
            name: name.to_string(),
            result,
            fires,
        }
    }
}

/// What kind of machine this looks like, and what said so.
#[derive(Debug, Clone)]
pub struct EnvironmentDetection {
    /// The most contained environment any signal fired for: a pod outranks a container, which
    /// outranks a VM. A bare host is what is left when nothing fired.
    pub environment: Environment,
    /// Every signal checked, with its result — recorded whether or not anything fired, so a
    /// bare-host record positively states that the checks ran and came back clean rather than
    /// merely omitting them.
    pub signals: Vec<Signal>,
}

impl EnvironmentDetection {
    /// The names of the signals that fired, joined for a one-line summary.
    pub fn firing(&self) -> String {
        let fired: Vec<&str> = self
            .signals
            .iter()
            .filter(|signal| signal.fires.is_some())
            .map(|signal| signal.name.as_str())
            .collect();
        if fired.is_empty() {
            "none".to_string()
        } else {
            fired.join(", ")
        }
    }

    /// True when no container, pod or VM signal fired.
    pub fn is_bare_host(&self) -> bool {
        self.environment == Environment::BareHost
    }
}

impl fmt::Display for EnvironmentDetection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.environment {
            Environment::BareHost => f.write_str("bare host (no container, pod or VM signal)"),
            other => write!(
                f,
                "{} DETECTED ({})",
                other.as_str().to_ascii_uppercase(),
                self.firing()
            ),
        }
    }
}

/// The readings [`classify`] decides on, gathered by [`read_environment`]. Kept apart from the
/// reading so the classification can be tested without the host.
#[derive(Debug, Clone)]
pub struct EnvironmentReadings {
    pub dockerenv: bool,
    pub containerenv: bool,
    /// `/proc/1/cgroup`, or why it could not be read.
    pub pid1_cgroup: Result<String, String>,
    /// The `/proc/self/ns/cgroup` link target, e.g. `cgroup:[4026531835]`.
    pub self_cgroup_ns: Result<String, String>,
    /// The `/proc/1/ns/cgroup` link target. Unreadable to a non-root run, since PID 1 belongs to
    /// root; the classifier then compares against the initial namespace's fixed inode.
    pub pid1_cgroup_ns: Result<String, String>,
    pub serviceaccount: bool,
    pub kubernetes_service_host: Option<String>,
    /// The filesystem type of the mount `/` resolves to, from `/proc/self/mountinfo`.
    pub root_fstype: Option<String>,
    /// `systemd-detect-virt --vm`: `Some(name)` when it ran, `None` when it is not installed.
    /// It prints `none` on a host with no hypervisor.
    pub detect_virt: Option<String>,
    /// `sys_vendor` and `product_name` from `/sys/class/dmi/id`, when readable.
    pub dmi: Option<(String, String)>,
}

/// The inode of the initial cgroup namespace, `PROC_CGROUP_INIT_INO` in the kernel's
/// `include/linux/proc_ns.h`. Every process outside a cgroup namespace reads
/// `cgroup:[4026531835]` from `/proc/self/ns/cgroup`.
pub const INITIAL_CGROUP_NS: &str = "cgroup:[4026531835]";

/// `/proc/1/cgroup` substrings that name a container runtime, and the environment each means.
const CGROUP_RUNTIMES: [(&str, Environment); 5] = [
    ("kubepods", Environment::Pod),
    ("docker", Environment::Container),
    ("containerd", Environment::Container),
    ("lxc", Environment::Container),
    ("podman", Environment::Container),
];

/// DMI strings, lowercased, that name a hypervisor. `amazon ec2` is matched separately: an EC2
/// `.metal` instance reports the same vendor with no hypervisor under it.
const DMI_HYPERVISORS: [&str; 11] = [
    "kvm",
    "qemu",
    "vmware",
    "virtualbox",
    "innotek",
    "xen",
    "bochs",
    "virtual machine",
    "google compute engine",
    "openstack",
    "parallels",
];

fn presence(name: &str, present: bool, fires: Environment) -> Signal {
    if present {
        Signal::new(name, "DETECTED".to_string(), Some(fires))
    } else {
        Signal::new(name, "absent".to_string(), None)
    }
}

/// Classifies the readings into an [`Environment`], recording every signal.
pub fn classify(readings: &EnvironmentReadings) -> EnvironmentDetection {
    let mut signals = vec![
        presence(
            "/.dockerenv present",
            readings.dockerenv,
            Environment::Container,
        ),
        presence(
            "/run/.containerenv present",
            readings.containerenv,
            Environment::Container,
        ),
    ];

    // `/proc/1/cgroup` on a bare-metal systemd host reads `0::/init.scope`; inside a container
    // the path carries the runtime's own name.
    signals.push(match &readings.pid1_cgroup {
        Ok(content) => {
            let lower = content.to_ascii_lowercase();
            let hits: Vec<(&str, Environment)> = CGROUP_RUNTIMES
                .into_iter()
                .filter(|(needle, _)| lower.contains(needle))
                .collect();
            match hits.iter().map(|(_, env)| *env).max() {
                Some(env) => Signal::new(
                    "/proc/1/cgroup substring",
                    format!(
                        "DETECTED: {}",
                        hits.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                    ),
                    Some(env),
                ),
                None => Signal::new(
                    "/proc/1/cgroup substring",
                    format!("clean ({})", content.trim().replace('\n', " | ")),
                    None,
                ),
            }
        }
        Err(err) => Signal::new(
            "/proc/1/cgroup substring",
            format!("unreadable ({err}) — not a container signal either way"),
            None,
        ),
    });

    // A cgroup namespace: this process's differs from PID 1's, or, when PID 1's cannot be read,
    // from the initial namespace every process outside one shares.
    let name = "cgroup namespace";
    signals.push(match (&readings.self_cgroup_ns, &readings.pid1_cgroup_ns) {
        (Ok(own), Ok(pid1)) if own != pid1 => Signal::new(
            name,
            format!("DETECTED: own {own}, PID 1 {pid1}"),
            Some(Environment::Container),
        ),
        (Ok(own), Ok(_)) => Signal::new(name, format!("shared with PID 1 ({own})"), None),
        (Ok(own), Err(_)) if own != INITIAL_CGROUP_NS => Signal::new(
            name,
            format!("DETECTED: own {own}, not the initial {INITIAL_CGROUP_NS}"),
            Some(Environment::Container),
        ),
        (Ok(own), Err(_)) => Signal::new(name, format!("the initial namespace ({own})"), None),
        (Err(err), _) => Signal::new(
            name,
            format!("unreadable ({err}) — not a container signal either way"),
            None,
        ),
    });

    signals.push(presence(
        "/var/run/secrets/kubernetes.io/serviceaccount present",
        readings.serviceaccount,
        Environment::Pod,
    ));
    signals.push(match &readings.kubernetes_service_host {
        Some(value) => Signal::new(
            "KUBERNETES_SERVICE_HOST set",
            format!("DETECTED: {value}"),
            Some(Environment::Pod),
        ),
        None => Signal::new("KUBERNETES_SERVICE_HOST set", "unset".to_string(), None),
    });

    signals.push(match readings.root_fstype.as_deref() {
        Some("overlay") => Signal::new(
            "overlay root filesystem",
            "DETECTED: / is an overlay mount".to_string(),
            Some(Environment::Container),
        ),
        Some(other) => Signal::new(
            "overlay root filesystem",
            format!("clean (/ is {other})"),
            None,
        ),
        None => Signal::new(
            "overlay root filesystem",
            "unknown (/proc/self/mountinfo names no / mount)".to_string(),
            None,
        ),
    });

    signals.push(match readings.detect_virt.as_deref() {
        Some("none") => Signal::new("systemd-detect-virt --vm", "none".to_string(), None),
        Some(virt) => Signal::new(
            "systemd-detect-virt --vm",
            format!("DETECTED: {virt}"),
            Some(Environment::VirtualMachine),
        ),
        None => Signal::new(
            "systemd-detect-virt --vm",
            "not installed — the DMI check below decides".to_string(),
            None,
        ),
    });

    signals.push(match &readings.dmi {
        Some((vendor, product)) => {
            let text = format!("{vendor} / {product}");
            let lower = text.to_ascii_lowercase();
            let ec2_vm = lower.contains("amazon ec2") && !lower.contains(".metal");
            if ec2_vm || DMI_HYPERVISORS.iter().any(|needle| lower.contains(needle)) {
                Signal::new(
                    "DMI vendor / product",
                    format!("DETECTED: {text}"),
                    Some(Environment::VirtualMachine),
                )
            } else {
                Signal::new("DMI vendor / product", format!("clean ({text})"), None)
            }
        }
        None => Signal::new(
            "DMI vendor / product",
            "unreadable — not a VM signal either way".to_string(),
            None,
        ),
    });

    let environment = signals
        .iter()
        .filter_map(|signal| signal.fires)
        .max()
        .unwrap_or(Environment::BareHost);
    EnvironmentDetection {
        environment,
        signals,
    }
}

/// The filesystem type of the mount `/` resolves to: the last `/` entry in `mountinfo`, since a
/// later mount on the same point hides an earlier one.
pub fn root_fstype(mountinfo: &str) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(parse_mountinfo_line)
        .rfind(|entry| entry.mount_point == "/")
        .map(|entry| entry.fstype)
}

/// One `/proc/self/mountinfo` line, as far as this file reads it.
struct MountEntry {
    mount_point: String,
    fstype: String,
}

/// `36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw`: the fifth field is the mount
/// point, octal-escaped; the first field after the ` - ` separator is the filesystem type.
fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let (left, right) = line.split_once(" - ")?;
    let mount_point = left.split(' ').nth(4)?;
    let fstype = right.split(' ').next()?;
    Some(MountEntry {
        mount_point: unescape_mountinfo(mount_point),
        fstype: fstype.to_string(),
    })
}

/// Undoes mountinfo's octal escaping (`\040` for a space) of space, tab, newline and backslash.
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let octal = field
                .get(i + 1..i + 4)
                .and_then(|digits| u8::from_str_radix(digits, 8).ok());
            if let Some(value) = octal {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount point `path` (absolute and canonical) resolves to in `mountinfo`: the longest mount
/// point that is a whole-component prefix of it, the later entry winning a tie.
pub fn mount_point_of(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<String> = None;
    for entry in mountinfo.lines().filter_map(parse_mountinfo_line) {
        if !path.starts_with(Path::new(&entry.mount_point)) {
            continue;
        }
        let longer = best
            .as_ref()
            .is_none_or(|current| entry.mount_point.len() >= current.len());
        if longer {
            best = Some(entry.mount_point);
        }
    }
    best
}

/// Where one path of the filesystem check lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsReading {
    pub path: PathBuf,
    /// `st_dev`, or why it could not be read.
    pub dev: Result<u64, String>,
    /// The mount point it resolves to, when `/proc/self/mountinfo` was readable.
    pub mount_point: Option<String>,
}

impl fmt::Display for FsReading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.dev {
            Ok(dev) => write!(
                f,
                "`{}` on device {dev}, mount `{}`",
                self.path.display(),
                self.mount_point.as_deref().unwrap_or("unknown")
            ),
            Err(err) => write!(f, "`{}` unreadable ({err})", self.path.display()),
        }
    }
}

/// Whether the work root shares a filesystem with the cases' outside-the-workdir targets.
///
/// `hardlink-escape` links `/etc/passwd` into the workdir and `rename-across-boundary` renames a
/// workdir file into `/var/tmp`. When the workdir is on another filesystem, link(2) and rename(2)
/// fail with `EXDEV` before any containment is consulted, so those cases report `INCONCLUSIVE` —
/// a false negative about the boundary, not a measurement of it. A run whose work root crosses
/// filesystems is therefore not citable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRootFilesystem {
    /// The work root first, then `/etc` and `/var/tmp`.
    pub readings: Vec<FsReading>,
}

/// The paths the work root has to share a filesystem with.
pub const CROSS_FILESYSTEM_TARGETS: [&str; 2] = ["/etc", "/var/tmp"];

impl WorkRootFilesystem {
    /// Reads `work_root` (canonical) and [`CROSS_FILESYSTEM_TARGETS`] on this host.
    pub fn probe(work_root: &Path) -> WorkRootFilesystem {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok();
        let read = |path: &Path| {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            FsReading {
                path: path.to_path_buf(),
                dev: device_of(&canonical),
                mount_point: mountinfo
                    .as_deref()
                    .and_then(|info| mount_point_of(info, &canonical)),
            }
        };
        let mut readings = vec![read(work_root)];
        readings.extend(CROSS_FILESYSTEM_TARGETS.iter().map(|p| read(Path::new(p))));
        WorkRootFilesystem { readings }
    }

    /// True when every path was read and all share one device and one mount point. A path that
    /// could not be read counts as crossing: the run cannot show it did not.
    pub fn is_same(&self) -> bool {
        let Some(first) = self.readings.first() else {
            return false;
        };
        let Ok(dev) = &first.dev else {
            return false;
        };
        self.readings.iter().all(|reading| {
            reading.dev.as_ref() == Ok(dev) && reading.mount_point == first.mount_point
        })
    }
}

impl fmt::Display for WorkRootFilesystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let joined: Vec<String> = self.readings.iter().map(ToString::to_string).collect();
        write!(
            f,
            "{} — {}",
            if self.is_same() {
                "same filesystem"
            } else {
                "CROSSES FILESYSTEMS"
            },
            joined.join("; ")
        )
    }
}

#[cfg(unix)]
fn device_of(path: &Path) -> Result<u64, String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|meta| meta.dev())
        .map_err(|err| err.to_string())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Result<u64, String> {
    Err("no st_dev on this platform".to_string())
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

/// Reads what [`classify`] decides on. Read-only, and quick: two files, one environment
/// variable, the mount table and at most one `systemd-detect-virt` call.
///
/// Deliberately a fixed, short list rather than an exhaustive one: the point is that a record
/// states plainly which checks ran and what each said, so a reader can judge the evidence. A long
/// list of half-reliable heuristics would make a clean result harder to trust, not easier.
pub fn read_environment() -> EnvironmentReadings {
    let link = |path: &str| {
        std::fs::read_link(path)
            .map(|target| target.display().to_string())
            .map_err(|err| err.to_string())
    };
    let dmi = |name: &str| {
        std::fs::read_to_string(format!("/sys/class/dmi/id/{name}"))
            .ok()
            .map(|s| s.trim().to_string())
    };
    EnvironmentReadings {
        dockerenv: Path::new("/.dockerenv").exists(),
        containerenv: Path::new("/run/.containerenv").exists(),
        pid1_cgroup: std::fs::read_to_string("/proc/1/cgroup").map_err(|err| err.to_string()),
        self_cgroup_ns: link("/proc/self/ns/cgroup"),
        pid1_cgroup_ns: link("/proc/1/ns/cgroup"),
        serviceaccount: Path::new("/var/run/secrets/kubernetes.io/serviceaccount").exists(),
        kubernetes_service_host: std::env::var("KUBERNETES_SERVICE_HOST").ok(),
        root_fstype: std::fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .and_then(|info| root_fstype(&info)),
        // Exits 1 and prints `none` on a host with no hypervisor, so stdout is read whatever the
        // status; only a failure to run at all means it is not installed.
        detect_virt: Command::new("systemd-detect-virt")
            .arg("--vm")
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .map(|text| {
                if text.is_empty() {
                    "none".to_string()
                } else {
                    text
                }
            }),
        dmi: dmi("sys_vendor").zip(dmi("product_name")),
    }
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
        environment: classify(&read_environment()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What this kind of laptop reads: a systemd host, a non-root run, no hypervisor.
    fn bare() -> EnvironmentReadings {
        EnvironmentReadings {
            dockerenv: false,
            containerenv: false,
            pid1_cgroup: Ok("0::/init.scope\n".to_string()),
            self_cgroup_ns: Ok(INITIAL_CGROUP_NS.to_string()),
            pid1_cgroup_ns: Err("Permission denied (os error 13)".to_string()),
            serviceaccount: false,
            kubernetes_service_host: None,
            root_fstype: Some("btrfs".to_string()),
            detect_virt: Some("none".to_string()),
            dmi: Some(("LENOVO".to_string(), "20N2CTO1WW".to_string())),
        }
    }

    fn fired(detection: &EnvironmentDetection) -> Vec<&str> {
        detection
            .signals
            .iter()
            .filter(|s| s.fires.is_some())
            .map(|s| s.name.as_str())
            .collect()
    }

    #[test]
    fn a_bare_host_classifies_as_bare_and_records_every_check() {
        let detection = classify(&bare());
        assert_eq!(detection.environment, Environment::BareHost);
        assert!(detection.is_bare_host());
        assert_eq!(detection.firing(), "none");
        assert_eq!(detection.signals.len(), 9);
        assert!(detection
            .signals
            .iter()
            .all(|s| !s.result.starts_with("DETECTED")));
        // Root reads PID 1's namespace and finds it shared.
        let root = EnvironmentReadings {
            pid1_cgroup_ns: Ok(INITIAL_CGROUP_NS.to_string()),
            ..bare()
        };
        assert!(classify(&root).is_bare_host());
        // No systemd-detect-virt and no DMI: nothing fired, so still bare.
        let sparse = EnvironmentReadings {
            detect_virt: None,
            dmi: None,
            pid1_cgroup: Err("Permission denied".to_string()),
            ..bare()
        };
        assert!(classify(&sparse).is_bare_host());
    }

    #[test]
    fn pod_signals_classify_as_a_pod() {
        let env_var = EnvironmentReadings {
            kubernetes_service_host: Some("10.96.0.1".to_string()),
            ..bare()
        };
        let detection = classify(&env_var);
        assert_eq!(detection.environment, Environment::Pod);
        assert_eq!(fired(&detection), ["KUBERNETES_SERVICE_HOST set"]);
        assert!(detection.environment.is_refused_by_default());

        let mount = EnvironmentReadings {
            serviceaccount: true,
            ..bare()
        };
        assert_eq!(classify(&mount).environment, Environment::Pod);

        let kubepods = EnvironmentReadings {
            pid1_cgroup: Ok("0::/kubepods/burstable/pod1234/abcd".to_string()),
            ..bare()
        };
        assert_eq!(classify(&kubepods).environment, Environment::Pod);
    }

    #[test]
    fn container_signals_classify_as_a_container() {
        let cases = [
            EnvironmentReadings {
                dockerenv: true,
                ..bare()
            },
            EnvironmentReadings {
                containerenv: true,
                ..bare()
            },
            EnvironmentReadings {
                pid1_cgroup: Ok("0::/system.slice/docker-abc.scope".to_string()),
                ..bare()
            },
            EnvironmentReadings {
                root_fstype: Some("overlay".to_string()),
                ..bare()
            },
            // `unshare -C` on a bare host: a new cgroup namespace, PID 1's unreadable.
            EnvironmentReadings {
                self_cgroup_ns: Ok("cgroup:[4026532999]".to_string()),
                ..bare()
            },
            // Root can read PID 1's and sees it differs.
            EnvironmentReadings {
                self_cgroup_ns: Ok("cgroup:[4026532999]".to_string()),
                pid1_cgroup_ns: Ok(INITIAL_CGROUP_NS.to_string()),
                ..bare()
            },
        ];
        for readings in &cases {
            let detection = classify(readings);
            assert_eq!(
                detection.environment,
                Environment::Container,
                "{readings:?}"
            );
            assert_eq!(fired(&detection).len(), 1, "{readings:?}");
            assert!(detection.to_string().starts_with("CONTAINER DETECTED ("));
        }
    }

    #[test]
    fn vm_signals_classify_as_a_vm_and_are_not_refused() {
        let virt = EnvironmentReadings {
            detect_virt: Some("microsoft".to_string()),
            ..bare()
        };
        let detection = classify(&virt);
        assert_eq!(detection.environment, Environment::VirtualMachine);
        assert!(!detection.environment.is_refused_by_default());
        assert!(!detection.is_bare_host());

        // A GitHub Actions runner's DMI, with no systemd-detect-virt.
        let azure = EnvironmentReadings {
            detect_virt: None,
            dmi: Some((
                "Microsoft Corporation".to_string(),
                "Virtual Machine".to_string(),
            )),
            ..bare()
        };
        assert_eq!(classify(&azure).environment, Environment::VirtualMachine);

        let ec2 = |product: &str| EnvironmentReadings {
            detect_virt: None,
            dmi: Some(("Amazon EC2".to_string(), product.to_string())),
            ..bare()
        };
        assert_eq!(
            classify(&ec2("m5.large")).environment,
            Environment::VirtualMachine
        );
        assert_eq!(
            classify(&ec2("c5.metal")).environment,
            Environment::BareHost
        );
    }

    /// The most contained environment wins: a pod inside a VM is a pod.
    #[test]
    fn the_most_contained_signal_decides() {
        let both = EnvironmentReadings {
            detect_virt: Some("kvm".to_string()),
            dockerenv: true,
            kubernetes_service_host: Some("10.0.0.1".to_string()),
            ..bare()
        };
        let detection = classify(&both);
        assert_eq!(detection.environment, Environment::Pod);
        assert_eq!(fired(&detection).len(), 3);
    }

    const MOUNTINFO: &str = "\
22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p5 rw
30 22 0:29 / /space rw,relatime shared:2 - ext4 /dev/nvme0n1p7 rw
31 22 0:30 / /tmp rw shared:3 - tmpfs tmpfs rw
32 22 0:31 / /mnt/a\\040b rw shared:4 - ext4 /dev/sdb1 rw
";

    #[test]
    fn mount_points_resolve_by_whole_component_prefix() {
        assert_eq!(
            mount_point_of(MOUNTINFO, Path::new("/etc")).as_deref(),
            Some("/")
        );
        assert_eq!(
            mount_point_of(MOUNTINFO, Path::new("/space/_murmur/x")).as_deref(),
            Some("/space")
        );
        // `/spaceship` is not under `/space`.
        assert_eq!(
            mount_point_of(MOUNTINFO, Path::new("/spaceship")).as_deref(),
            Some("/")
        );
        assert_eq!(
            mount_point_of(MOUNTINFO, Path::new("/mnt/a b/c")).as_deref(),
            Some("/mnt/a b")
        );
        assert_eq!(root_fstype(MOUNTINFO).as_deref(), Some("btrfs"));
        let overlaid = format!("{MOUNTINFO}40 22 0:40 / / rw - overlay overlay rw\n");
        assert_eq!(root_fstype(&overlaid).as_deref(), Some("overlay"));
    }

    fn reading(path: &str, dev: u64, mount: &str) -> FsReading {
        FsReading {
            path: PathBuf::from(path),
            dev: Ok(dev),
            mount_point: Some(mount.to_string()),
        }
    }

    #[test]
    fn a_work_root_on_another_filesystem_crosses() {
        let same = WorkRootFilesystem {
            readings: vec![
                reading("/home/u/work", 30, "/"),
                reading("/etc", 30, "/"),
                reading("/var/tmp", 30, "/"),
            ],
        };
        assert!(same.is_same());
        assert!(same.to_string().starts_with("same filesystem"));

        let space = WorkRootFilesystem {
            readings: vec![
                reading("/space/work", 42, "/space"),
                reading("/etc", 30, "/"),
                reading("/var/tmp", 30, "/"),
            ],
        };
        assert!(!space.is_same());
        assert!(space.to_string().starts_with("CROSSES FILESYSTEMS"));

        // Same device, different mount (a bind mount): link(2) still fails with EXDEV.
        let bind = WorkRootFilesystem {
            readings: vec![
                reading("/srv/work", 30, "/srv"),
                reading("/etc", 30, "/"),
                reading("/var/tmp", 30, "/"),
            ],
        };
        assert!(!bind.is_same());

        let unreadable = WorkRootFilesystem {
            readings: vec![
                reading("/home/u/work", 30, "/"),
                reading("/etc", 30, "/"),
                FsReading {
                    path: PathBuf::from("/var/tmp"),
                    dev: Err("No such file or directory".to_string()),
                    mount_point: None,
                },
            ],
        };
        assert!(!unreadable.is_same());
    }
}
