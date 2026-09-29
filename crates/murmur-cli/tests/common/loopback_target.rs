//! A `mur deploy run` target on loopback: no VM, container, root or network.
//!
//! The target is a temp dir `T` holding `T/root` (the target's `HOME`), `T/usr/local/bin` and
//! `T/tmp`. Deploy reaches it through `ssh` and `scp` shims that [`LoopbackTarget::path_env`]
//! puts first on the deploying process's `PATH`:
//!
//! - `ssh` drops every option and the `user@host` word, appends the command it was given to
//!   `T/commands.log` as one NUL-terminated record, rewrites `/root/`, `/usr/local/bin` and `/tmp/`
//!   into `T` in one pass (a `T` under `/tmp` is never rewritten twice), and runs the result with
//!   `sh -c` in an environment of its own, as a real `ssh` would: `HOME=T/root` and a `PATH` whose
//!   first entry holds no-op `sysctl`, `iptables` and `ufw`, so the host's firewall is never
//!   touched. Nothing else of the deploying process's environment reaches the target.
//! - Before a command that starts `mur run` it writes a listing of `T/root/.murmur/compiled` to
//!   `T/compiled-before-start.txt` and appends `start <ns>` to `T/timings.log`; around a
//!   `mur precompile` command it appends `precompile <start ns> <end ns>`. Times are
//!   `CLOCK_REALTIME` nanoseconds (`date +%s%N`), the only sub-millisecond clock a shell has, so
//!   compare them with [`now_ns`].
//! - `scp` copies the local source, never rewritten, to the rewritten remote path, honouring
//!   `-r`, and appends `scp [-r] <remote path>` to `T/scp.log`.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tempfile::TempDir;

/// The listing the shim snapshots and [`LoopbackTarget::compiled_listing`] returns: `absent`, or
/// the directory's mode then one `name inode size mtime mode` line per entry, sorted.
const LIST_COMPILED: &str = r#"#!/bin/sh
dir="$1/root/.murmur/compiled"
if [ -d "$dir" ]; then
  stat -c 'dir %a' "$dir"
  find "$dir" -mindepth 1 -maxdepth 1 -printf '%f %i %s %T@ %m\n' | LC_ALL=C sort
else
  echo absent
fi
"#;

const SSH: &str = r#"#!/bin/sh
T=@T@
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-i|-p|-l|-F) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
shift
cmd="$*"
printf '%s\0' "$cmd" >> "$T/commands.log"
remote=$(printf '%s\n' "$cmd" | sed \
  -e 's#/root/#@@MUR_ROOT@@#g' -e 's#/usr/local/bin#@@MUR_BIN@@#g' -e 's#/tmp/#@@MUR_TMP@@#g' \
  -e "s#@@MUR_ROOT@@#$T/root/#g" -e "s#@@MUR_BIN@@#$T/usr/local/bin#g" \
  -e "s#@@MUR_TMP@@#$T/tmp/#g")
run() {
  env -i HOME="$T/root" PATH="$T/noop-bin:@PATH@" LANG=C.UTF-8 sh -c "$remote"
}
case "$cmd" in
  *"bin/mur run "*)
    "$T/list-compiled" "$T" > "$T/compiled-before-start.txt"
    echo "start $(date +%s%N)" >> "$T/timings.log"
    run
    ;;
  *"bin/mur precompile "*)
    started=$(date +%s%N)
    run
    status=$?
    echo "precompile $started $(date +%s%N)" >> "$T/timings.log"
    exit $status
    ;;
  *)
    run
    ;;
esac
"#;

const SCP: &str = r#"#!/bin/sh
T=@T@
recursive=
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-i|-P|-F) shift 2 ;;
    -r) recursive=-r; shift ;;
    -*) shift ;;
    *) break ;;
  esac
done
source="$1"
remote="${2#*:}"
echo "scp${recursive:+ -r} $remote" >> "$T/scp.log"
local=$(printf '%s\n' "$remote" | sed \
  -e 's#^/root/#@@MUR_ROOT@@#' -e 's#^/usr/local/bin#@@MUR_BIN@@#' -e 's#^/tmp/#@@MUR_TMP@@#' \
  -e "s#@@MUR_ROOT@@#$T/root/#" -e "s#@@MUR_BIN@@#$T/usr/local/bin#" -e "s#@@MUR_TMP@@#$T/tmp/#")
exec cp $recursive "$source" "$local"
"#;

const NOOP: &str = "#!/bin/sh\nexit 0\n";

/// Nanoseconds since the epoch on the clock the shims stamp with.
pub fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

pub struct LoopbackTarget {
    dir: TempDir,
}

impl Default for LoopbackTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopbackTarget {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mur-target-")
            .tempdir()
            .unwrap();
        let t = dir.path();
        // The rewrite is a sed replacement between `#` delimiters, inside double quotes.
        assert!(
            t.to_str()
                .unwrap()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)),
            "{} needs quoting the shims do not do",
            t.display()
        );
        for sub in ["root", "usr/local/bin", "tmp", "shim", "noop-bin"] {
            fs::create_dir_all(t.join(sub)).unwrap();
        }
        let quoted = format!("'{}'", t.display());
        let host_path = std::env::var("PATH").unwrap_or_default();
        write_executable(
            &t.join("shim/ssh"),
            &SSH.replace("@T@", &quoted).replace("@PATH@", &host_path),
        );
        write_executable(&t.join("shim/scp"), &SCP.replace("@T@", &quoted));
        write_executable(&t.join("list-compiled"), LIST_COMPILED);
        for tool in ["sysctl", "iptables", "ufw"] {
            write_executable(&t.join("noop-bin").join(tool), NOOP);
        }
        Self { dir }
    }

    /// `T`.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `T/root`, the target's `HOME`.
    pub fn root(&self) -> PathBuf {
        self.path().join("root")
    }

    /// The `PATH` for a deploying process: the shims first, then this process's own.
    pub fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.path().join("shim").display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// Every command the `ssh` shim was given, verbatim, in the order it was given them.
    pub fn commands(&self) -> Vec<String> {
        let log = fs::read(self.path().join("commands.log")).unwrap_or_default();
        log.split(|&b| b == 0)
            .filter(|record| !record.is_empty())
            .map(|record| String::from_utf8_lossy(record).into_owned())
            .collect()
    }

    /// The listing of `T/root/.murmur/compiled` taken just before the last `mur run` started.
    pub fn compiled_before_start(&self) -> String {
        fs::read_to_string(self.path().join("compiled-before-start.txt"))
            .expect("the shim ran no `mur run` command")
    }

    /// The listing of `T/root/.murmur/compiled` now, in the shim's format.
    pub fn compiled_listing(&self) -> String {
        let output = std::process::Command::new(self.path().join("list-compiled"))
            .arg(self.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    /// The `start` timestamps, in order.
    pub fn start_times(&self) -> Vec<u128> {
        self.timings("start")
            .into_iter()
            .map(|fields| fields[0])
            .collect()
    }

    /// The `(start, end)` of every `mur precompile` command, in order.
    pub fn precompile_times(&self) -> Vec<(u128, u128)> {
        self.timings("precompile")
            .into_iter()
            .map(|fields| (fields[0], fields[1]))
            .collect()
    }

    fn timings(&self, kind: &str) -> Vec<Vec<u128>> {
        fs::read_to_string(self.path().join("timings.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))
            .map(|rest| rest.split(' ').map(|n| n.parse().unwrap()).collect())
            .collect()
    }

    /// Runs `command` through the `ssh` shim, as deploy would, and returns its output.
    pub fn ssh(&self, command: &str) -> std::process::Output {
        std::process::Command::new(self.path().join("shim/ssh"))
            .args(["-o", "LogLevel=ERROR", "root@127.0.0.1", command])
            .output()
            .unwrap()
    }

    /// Kills every process whose command line names `T`, and waits for them to go.
    pub fn stop_capsules(&self) {
        let survivors = self.kill_all();
        assert!(
            survivors.is_empty(),
            "processes under {} survived SIGKILL: {survivors:?}",
            self.path().display()
        );
    }

    /// [`Self::stop_capsules`] without the assertion: the pids still alive after ten seconds.
    fn kill_all(&self) -> Vec<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pids = self.processes();
            if pids.is_empty() || Instant::now() > deadline {
                return pids;
            }
            for pid in &pids {
                // SAFETY: `kill` takes no pointers; a pid that has exited meanwhile is ESRCH.
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Live, non-zombie processes whose command line names `T`, other than this one.
    fn processes(&self) -> Vec<i32> {
        let needle = self.path().to_str().unwrap().as_bytes().to_vec();
        let own = std::process::id() as i32;
        fs::read_dir("/proc")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&pid| pid != own)
            .filter(|pid| {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
                !matches!(state.and_then(|s| s.chars().next()), None | Some('Z'))
            })
            .filter(|pid| {
                fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|cmdline| cmdline.windows(needle.len()).any(|w| w == needle))
            })
            .collect()
    }
}

impl Drop for LoopbackTarget {
    fn drop(&mut self) {
        // No assertion: a panic here while a failing test unwinds would abort the whole binary.
        self.kill_all();
    }
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
