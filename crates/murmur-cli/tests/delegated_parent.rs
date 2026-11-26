//! A launch inside a delegated parent cgroup takes that parent instead of asking systemd for a
//! scope, driven through the real binary.
//!
//! The construction gives an unprivileged user a delegated parent with no session bus: a transient
//! `systemd-run --user --scope -p Delegate=yes` scope, entered through a user, mount and cgroup
//! namespace (`unshare -Urm -C`) so the scope is the root of the visible hierarchy, with a tmpfs
//! over `/sys/fs` and a fresh cgroup2 mount at `/sys/fs/cgroup`. The wrapper shell moves itself
//! into `/sys/fs/cgroup/launcher` and (unless a test leaves them off) enables `memory pids cpu`
//! for the root's children, and the session bus is hidden from everything inside. Each test runs
//! one shell script there, which starts `mur` and writes what it saw to files in an output folder.
//! A host where the construction cannot be built prints `[SKIP-HOST]` and the case returns.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const DRIVER: &str = "local/fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";

/// The controllers the construction enables for the parent's children by default.
const ENABLE_CPU_MEMORY_PIDS: &str = "+memory +pids +cpu";

/// The default and `io`: what a systemd scope delegates on this host.
const ENABLE_WITH_IO: &str = "+memory +pids +cpu +io";

/// The setup the construction's `sh -c` runs before it `exec`s the test's script: a cgroup2 mount
/// of the namespace's own root, the wrapper moved into `launcher`, then `$ENABLE` written to the
/// root's `cgroup.subtree_control` when set.
const SETUP: &str = "mount -t tmpfs tmpfs /sys/fs && mkdir /sys/fs/cgroup && \
     mount -t cgroup2 cgroup2 /sys/fs/cgroup && mkdir /sys/fs/cgroup/launcher && \
     echo $$ > /sys/fs/cgroup/launcher/cgroup.procs && \
     { [ -z \"$ENABLE\" ] || echo \"$ENABLE\" > /sys/fs/cgroup/cgroup.subtree_control; } && \
     exec \"$0\" \"$@\"";

fn mur_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// `sh -c script` inside the construction, with `enable` written to the parent's
/// `cgroup.subtree_control` (nothing when empty), `HOME`, `$MUR` and `vars` set, and the session
/// bus hidden from everything inside: `systemd-run` itself still needs it to reach the user
/// manager, so it is removed only past that point.
fn in_parent(
    enable: &str,
    home: &Path,
    cwd: &Path,
    script: &str,
    vars: &[(&str, &str)],
) -> Command {
    let mut command = Command::new("systemd-run");
    command
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
            "env",
            "-u",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR=/nonexistent-for-this-test",
            "unshare",
            "-Urm",
            "-C",
            "sh",
            "-c",
            SETUP,
            "sh",
            "-c",
            script,
        ])
        .env("ENABLE", enable)
        .env("HOME", home)
        .env("MUR", mur_path())
        .current_dir(cwd);
    for (name, value) in vars {
        command.env(name, value);
    }
    command
}

/// `None` when the construction works here — inside it this process's cgroup is `/launcher`, its
/// parent enables the controllers and no session bus is reachable — else why it does not.
fn construction_unavailable() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("cgroups are Linux-only".to_string());
    }
    let script = "sed -n 's/^0:://p' /proc/self/cgroup; cat /sys/fs/cgroup/cgroup.subtree_control";
    let output = in_parent(
        ENABLE_CPU_MEMORY_PIDS,
        Path::new("/"),
        Path::new("/"),
        script,
        &[],
    )
    .output();
    match output {
        Err(error) => Some(format!("systemd-run or unshare could not be run: {error}")),
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            let mut lines = text.lines();
            let cgroup = lines.next().unwrap_or_default();
            let enabled: BTreeSet<&str> = lines.next().unwrap_or_default().split(' ').collect();
            if output.status.success()
                && cgroup == "/launcher"
                && ["memory", "pids", "cpu"]
                    .iter()
                    .all(|controller| enabled.contains(controller))
            {
                None
            } else {
                Some(format!(
                    "the delegated parent construction did not come up ({}): {}{}",
                    output.status,
                    text.trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        }
    }
}

fn skip(test: &str) -> bool {
    if let Some(why) = construction_unavailable() {
        eprintln!("[SKIP-HOST] {test}: {why}");
        return true;
    }
    capsule_runtime::skip_without_egress_namespace(test)
}

/// A process-transport capsule whose fake harness holds its turn open for `hold_secs` seconds,
/// with `resources` (YAML lines under `capabilities.resources:`, may be empty). Its fixture
/// process driver runs in a guest worker and its harness in the session cgroup, so a launch needs
/// a cgroup scope.
struct Capsule {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    manifest: PathBuf,
}

impl Capsule {
    fn new(hold_secs: u32, resources: &str) -> Self {
        let home = common::scratch_off_tmp();
        let artifacts = TempDir::new().unwrap();
        let project = common::scratch_off_tmp();
        let driver = common::create_driver_artifact_with_auth(
            artifacts.path(),
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("process-driver/tool/process-driver.wasm"),
            "",
        );
        common::publish_local(&home, &driver).success();
        let harness = common::write_fake_harness(
            &project.path().join("harness.sh"),
            &format!(
                "export FIXTURE_HARNESS_PROFILE=held\nexport FIXTURE_HARNESS_HOLD_SECS={hold_secs}\n"
            ),
        );
        let resources = if resources.is_empty() {
            String::new()
        } else {
            format!("  resources:\n{resources}")
        };
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: delegated-parent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  containment: \
                 scoped\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n{resources}\
                 inference:\n  transport: process\n  driver:\n    artifact: {DRIVER}\n  \
                 command: {}\n",
                harness.display()
            ),
        )
        .unwrap();
        Self {
            home,
            project,
            _artifacts: artifacts,
            manifest,
        }
    }

    /// Runs `script` in the construction with `$MANIFEST` this capsule's manifest and `$OUT` a
    /// fresh output folder, which is returned once the script has finished.
    fn in_parent(&self, enable: &str, script: &str) -> Out {
        let out = TempDir::new().unwrap();
        let output = in_parent(
            enable,
            self.home.path(),
            self.project.path(),
            script,
            &[
                ("MANIFEST", self.manifest.to_str().unwrap()),
                ("OUT", out.path().to_str().unwrap()),
            ],
        )
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "the construction's script failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Out(out)
    }

    /// Whether any session wrote a trace under this capsule's `HOME`.
    fn any_trace(&self) -> bool {
        fn walk(path: &Path) -> bool {
            fs::read_dir(path).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    let path = entry.path();
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        walk(&path)
                    } else {
                        path.file_name().is_some_and(|name| name == "trace.jsonl")
                    }
                })
            })
        }
        walk(&self.home.path().join(".murmur"))
    }
}

/// The output folder of one script.
struct Out(TempDir);

impl Out {
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.path().join(name)).unwrap_or_default()
    }

    /// What launch `name` printed: `<name>.out`, `<name>.err` and the exit code in
    /// `<name>.code`.
    fn launch(&self, name: &str) -> Launch {
        Launch {
            code: self.read(&format!("{name}.code")).trim().parse().ok(),
            pid: self
                .read(&format!("{name}.pid"))
                .trim()
                .parse()
                .unwrap_or(0),
            stdout: self.read(&format!("{name}.out")),
            stderr: self.read(&format!("{name}.err")),
        }
    }
}

struct Launch {
    code: Option<i32>,
    pid: u32,
    stdout: String,
    stderr: String,
}

impl Launch {
    fn all(&self) -> String {
        format!(
            "--- pid {} exit {:?} ---\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.pid, self.code, self.stdout, self.stderr
        )
    }
}

/// Shell functions every script uses: `launch NAME` starts `mur run` of `$MANIFEST` in the
/// background with its pid in `$OUT/NAME.pid`; `finish NAME` waits for it and writes its exit
/// code; `snapshots` appends, every 50 ms until `$OUT/stop` exists, every `murmur-*` cgroup under
/// the root, its children and `launcher` with its `populated` value to `$OUT/snap.txt`.
const FUNCTIONS: &str = r#"
launch() { "$MUR" run --manifest "$MANIFEST" --task "do the thing" > "$OUT/$1.out" 2> "$OUT/$1.err" & echo $! > "$OUT/$1.pid"; }
finish() { wait "$(cat "$OUT/$1.pid")"; echo $? > "$OUT/$1.code"; }
snapshots() {
  while [ ! -e "$OUT/stop" ]; do
    for d in /sys/fs/cgroup/murmur-* /sys/fs/cgroup/murmur-*/* /sys/fs/cgroup/launcher/murmur-*; do
      [ -d "$d" ] && echo "$d $(grep populated "$d/cgroup.events")"
    done
    echo ---
    sleep 0.05
  done >> "$OUT/snap.txt"
}
leftovers() { find /sys/fs/cgroup -name 'murmur-*' > "$OUT/leftover.txt"; }
"#;

/// Twelve launches at once, with snapshots of the cgroup tree while they run.
const TWELVE: &str = r#"
snapshots & SNAP=$!
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do launch $i; done
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do finish $i; done
touch "$OUT/stop"; wait $SNAP
leftovers
"#;

fn script(body: &str) -> String {
    format!("{FUNCTIONS}{body}")
}

/// The `murmur-<pid>-p` directories directly under the root that `snap.txt` saw, by pid.
fn adopted_pids(snapshots: &str) -> BTreeSet<u32> {
    snapshots
        .lines()
        .filter_map(|line| line.strip_prefix("/sys/fs/cgroup/murmur-"))
        .filter_map(|rest| rest.split(['/', ' ']).next())
        .filter_map(|name| name.strip_suffix("-p"))
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// Twelve concurrent launches with no session bus, inside a parent that enables the controllers,
/// each take the parent: every launch succeeds in a `murmur-<pid>-p` of its own, with its
/// supervisor leaf and its session cgroup inside it, nothing is created in the wrapper's
/// `launcher`, and nothing is left once they have exited.
#[test]
fn twelve_launches_adopt_the_parent_with_no_session_bus() {
    let test = "twelve_launches_adopt_the_parent_with_no_session_bus";
    if skip(test) {
        return;
    }
    let capsule = Capsule::new(1, "");
    let out = capsule.in_parent(ENABLE_CPU_MEMORY_PIDS, &script(TWELVE));

    let launches: Vec<Launch> = (1..=12).map(|i| out.launch(&i.to_string())).collect();
    for launch in &launches {
        assert_eq!(launch.code, Some(0), "{}", launch.all());
    }
    let pids: BTreeSet<u32> = launches.iter().map(|launch| launch.pid).collect();
    assert_eq!(pids.len(), 12, "{pids:?}");

    let snapshots = out.read("snap.txt");
    assert_eq!(
        adopted_pids(&snapshots),
        pids,
        "each launch, and nothing else, has a murmur-<pid>-p of its own:\n{snapshots}"
    );
    let laid_out = pids.iter().any(|pid| {
        let own = format!("/sys/fs/cgroup/murmur-{pid}-p/");
        snapshots
            .lines()
            .any(|line| line.starts_with(&format!("{own}murmur-supervisor ")))
            && snapshots.lines().any(|line| {
                line.starts_with(&format!("{own}murmur-ses_")) && line.ends_with("populated 1")
            })
    });
    assert!(
        laid_out,
        "no launch showed murmur-<pid>-p/murmur-supervisor beside a populated session cgroup:\n\
         {snapshots}"
    );
    assert!(
        !snapshots.contains("/sys/fs/cgroup/launcher/murmur-"),
        "a launch built its cgroups in the wrapper's launcher:\n{snapshots}"
    );
    assert_eq!(out.read("leftover.txt"), "", "left behind");
}

/// With the controllers left disabled for the parent's children every launch declines the parent,
/// finds no session bus, and is refused with `E-RUN-012` naming the decline; none starts a
/// session or leaves a cgroup.
#[test]
fn twelve_launches_refuse_when_the_parent_enables_no_controllers() {
    let test = "twelve_launches_refuse_when_the_parent_enables_no_controllers";
    if skip(test) {
        return;
    }
    let capsule = Capsule::new(1, "");
    let out = capsule.in_parent("", &script(TWELVE));

    for i in 1..=12 {
        let launch = out.launch(&i.to_string());
        assert_ne!(launch.code, Some(0), "{}", launch.all());
        assert!(
            launch.stderr.contains("error[E-RUN-012]"),
            "{}",
            launch.all()
        );
        assert!(
            launch.stderr.contains(
                "no delegated parent was adopted: parent does not enable memory,pids,cpu"
            ),
            "{}",
            launch.all()
        );
        assert!(!launch.stdout.contains("session:"), "{}", launch.all());
    }
    assert!(
        !capsule.any_trace(),
        "a refused launch wrote a session trace"
    );
    let leftovers = out.read("leftover.txt");
    assert!(
        !leftovers.lines().any(|line| line.ends_with("-p")),
        "{leftovers}"
    );
}

/// A capsule that declares `cgroup_io_bytes_per_sec` needs `io` enabled on the parent: with only
/// `memory pids cpu` enabled it is refused naming the missing `io`, and the same capsule without
/// the declaration launches in a `murmur-<pid>-p`.
#[test]
fn a_declared_io_limit_needs_io_enabled_on_the_parent() {
    let test = "a_declared_io_limit_needs_io_enabled_on_the_parent";
    if skip(test) {
        return;
    }
    let declared = Capsule::new(1, "    cgroup_io_bytes_per_sec: 10485760\n");
    let out = declared.in_parent(
        ENABLE_CPU_MEMORY_PIDS,
        &script("launch io; finish io; leftovers\n"),
    );
    let refused = out.launch("io");
    assert_ne!(refused.code, Some(0), "{}", refused.all());
    assert!(
        refused.stderr.contains("error[E-RUN-012]"),
        "{}",
        refused.all()
    );
    assert!(
        refused
            .stderr
            .contains("no delegated parent was adopted: parent does not enable io"),
        "{}",
        refused.all()
    );

    let undeclared = Capsule::new(1, "");
    let out = undeclared.in_parent(
        ENABLE_CPU_MEMORY_PIDS,
        &script(
            "snapshots & SNAP=$!\nlaunch plain; finish plain\ntouch \"$OUT/stop\"; wait $SNAP\n\
             leftovers\n",
        ),
    );
    let launched = out.launch("plain");
    assert_eq!(launched.code, Some(0), "{}", launched.all());
    let snapshots = out.read("snap.txt");
    assert_eq!(
        adopted_pids(&snapshots),
        BTreeSet::from([launched.pid]),
        "{snapshots}"
    );
    assert_eq!(out.read("leftover.txt"), "", "left behind");
}

/// A launch killed with `SIGKILL` runs no exit hook: its processes die with it, its
/// `murmur-<pid>-p` stays, unpopulated, and the next launch under the same parent sweeps it.
#[test]
fn a_killed_launch_is_swept_by_the_next() {
    let test = "a_killed_launch_is_swept_by_the_next";
    if skip(test) {
        return;
    }
    let held = Capsule::new(60, "");
    let next = Capsule::new(1, "");
    let body = format!(
        r#"
launch held; P=$(cat "$OUT/held.pid"); own=/sys/fs/cgroup/murmur-$P-p
n=0; until ls -d "$own"/murmur-ses_* > /dev/null 2>&1 || [ $n -ge 1200 ]; do sleep 0.05; n=$((n+1)); done
cat "$own"/cgroup.procs "$own"/*/cgroup.procs > "$OUT/tree.txt" 2>/dev/null
kill -9 "$P"; wait "$P"
sleep 3
for p in $(cat "$OUT/tree.txt"); do
  [ "$p" = "$P" ] && continue
  s=$(cut -d' ' -f3 "/proc/$p/stat" 2>/dev/null) && [ "$s" != Z ] && echo "$p $s" >> "$OUT/alive.txt"
done
[ -d "$own" ] && echo present > "$OUT/own.txt"
cat "$own/cgroup.events" > "$OUT/own.events" 2>/dev/null
MANIFEST={next}; launch next; finish next
leftovers
"#,
        next = next.manifest.display()
    );
    let out = held.in_parent(ENABLE_CPU_MEMORY_PIDS, &script(&body));

    let killed = out.launch("held");
    let tree = out.read("tree.txt");
    assert!(
        tree.lines().any(|pid| pid.trim() == killed.pid.to_string()),
        "the held launch never reached its session cgroup: {tree:?}\n{}",
        killed.all()
    );
    assert_eq!(
        out.read("alive.txt"),
        "",
        "descendants alive 3 s after SIGKILL"
    );
    assert_eq!(out.read("own.txt").trim(), "present", "{}", killed.all());
    assert!(
        out.read("own.events")
            .lines()
            .any(|line| line == "populated 0"),
        "{}",
        out.read("own.events")
    );
    let second = out.launch("next");
    assert_eq!(second.code, Some(0), "{}", second.all());
    assert_eq!(out.read("leftover.txt"), "", "left behind");
}

/// Under an adopted parent the declared limits land on the session cgroup exactly as under a
/// systemd scope, `murmur-<pid>-p` itself carries none, and the launch and its guest workers sit
/// in `murmur-<pid>-p/murmur-supervisor`.
#[test]
fn declared_limits_land_on_the_session_cgroup_under_an_adopted_parent() {
    let test = "declared_limits_land_on_the_session_cgroup_under_an_adopted_parent";
    if skip(test) {
        return;
    }
    let capsule = Capsule::new(
        5,
        "    cgroup_memory_bytes: 268435456\n    cgroup_pids_max: 64\n",
    );
    let body = r#"
launch held; P=$(cat "$OUT/held.pid"); own=/sys/fs/cgroup/murmur-$P-p
n=0; until grep -qx 'populated 1' "$own"/murmur-ses_*/cgroup.events 2>/dev/null || [ $n -ge 1200 ]; do sleep 0.05; n=$((n+1)); done
S=$(ls -d "$own"/murmur-ses_* | head -1)
cat "$S/memory.max" > "$OUT/ses.memory.max"; cat "$S/pids.max" > "$OUT/ses.pids.max"
cat "$own/memory.max" > "$OUT/own.memory.max"; cat "$own/pids.max" > "$OUT/own.pids.max"
sed -n 's/^0:://p' "/proc/$P/cgroup" > "$OUT/held.cgroup"
for p in $(cat "$own/murmur-supervisor/cgroup.procs"); do
  echo "$p $(tr '\0' ' ' < /proc/$p/cmdline)" >> "$OUT/supervisor.txt"
done
finish held
leftovers
"#;
    let out = capsule.in_parent(ENABLE_CPU_MEMORY_PIDS, &script(body));

    let launch = out.launch("held");
    assert_eq!(launch.code, Some(0), "{}", launch.all());
    assert_eq!(out.read("ses.memory.max").trim(), "268435456");
    assert_eq!(out.read("ses.pids.max").trim(), "64");
    assert_eq!(out.read("own.memory.max").trim(), "max");
    assert_eq!(out.read("own.pids.max").trim(), "max");
    assert_eq!(
        out.read("held.cgroup").trim(),
        format!("/murmur-{}-p/murmur-supervisor", launch.pid)
    );
    let supervisor = out.read("supervisor.txt");
    let workers = supervisor
        .lines()
        .filter(|line| line.contains("__guest-worker"))
        .count();
    assert!(
        supervisor
            .lines()
            .any(|line| line.starts_with(&format!("{} ", launch.pid))),
        "{supervisor}"
    );
    assert!(
        workers >= 1,
        "no guest worker in the supervisor leaf:\n{supervisor}"
    );
    assert_eq!(out.read("leftover.txt"), "", "left behind");
}

/// `mur run --explain-scope` prints the same bytes on an adopted parent with no session bus as on
/// a systemd scope with one, `cgroup scope: delegated` included, and leaves nothing behind. The
/// parent enables `io` as the systemd scope outside does, so the `io.max` probe answers alike.
#[test]
fn explain_scope_is_identical_on_an_adopted_parent() {
    let test = "explain_scope_is_identical_on_an_adopted_parent";
    if skip(test) {
        return;
    }
    if !common::cgroup_delegation_available() {
        eprintln!("[SKIP-HOST] {test}: outside the construction this host cannot delegate a scope");
        return;
    }
    let capsule = Capsule::new(1, "");
    let out = capsule.in_parent(
        ENABLE_WITH_IO,
        &script(
            "\"$MUR\" run --manifest \"$MANIFEST\" --explain-scope > \"$OUT/explain.out\" \
             2> \"$OUT/explain.err\"; echo $? > \"$OUT/explain.code\"\nleftovers\n",
        ),
    );
    let inside = out.launch("explain");
    assert_eq!(inside.code, Some(0), "{}", inside.all());

    let outside = Command::new(mur_path())
        .args([
            "run",
            "--manifest",
            capsule.manifest.to_str().unwrap(),
            "--explain-scope",
        ])
        .env("HOME", capsule.home.path())
        .current_dir(capsule.project.path())
        .output()
        .unwrap();
    assert!(
        outside.status.success(),
        "{}",
        String::from_utf8_lossy(&outside.stderr)
    );
    let outside = String::from_utf8_lossy(&outside.stdout).into_owned();
    assert!(
        inside.stdout.contains("\n  cgroup scope: delegated\n"),
        "{}",
        inside.all()
    );
    assert_eq!(inside.stdout, outside);
    assert_eq!(out.read("leftover.txt"), "", "left behind");
}
