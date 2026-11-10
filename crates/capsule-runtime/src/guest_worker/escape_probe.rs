//! The host side of the escape-conformance probe guest, compiled only with the test-only
//! `escape-probe` Cargo feature, which no release build enables (`lib.rs` refuses to compile it
//! without debug assertions, and `scripts/check-no-escape-probe.sh` checks the released binaries).
//!
//! The gate's `wasm-guest` route measures a guest worker as if a guest had escaped Wasmtime and
//! were making raw host syscalls from the worker process. A guest cannot make one, so the probe
//! guest (`escape-conformance/probe-guest`) asks: it returns its input under the result metadata
//! key [`REQUEST_KEY`], and the worker, on seeing it, runs the named case's probe body here, in
//! the worker process, under whatever enforcement the worker was spawned with. Each body mirrors
//! the Python body of the same case in `escape-conformance/src/cases.rs`, and writes the same
//! `VERDICT=`/`DETAIL=` file into the work folder the gate grades on.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bindings::host::murmur::tool::run::ToolResult;
use crate::runtime::ToolCallSpec;

/// The result metadata key the probe guest puts its request under.
pub(crate) const REQUEST_KEY: &str = "murmur-escape-probe";

/// The verdict file, relative to the work folder: `escape-conformance/src/probe.rs`'s
/// `PROBE_FILE`.
const PROBE_FILE: &str = "__ec.probe";

/// Answers the probe guest's request in `result`, if it made one: runs the case it names, writes
/// the verdict file into the call's work folder, and replaces the result's summary with the
/// verdict. A result without the key is left as it is.
pub(crate) fn answer(spec: &ToolCallSpec, result: &mut ToolResult) {
    let Some(request) = result
        .metadata
        .iter()
        .find(|(key, _)| key == REQUEST_KEY)
        .map(|(_, value)| value.clone())
    else {
        return;
    };
    let workdir = spec
        .wasi
        .preopens
        .iter()
        .find(|preopen| preopen.guest_path == ".")
        .map(|preopen| preopen.host_path.clone());
    let request_value = serde_json::from_str::<serde_json::Value>(&request).unwrap_or_default();
    let case = request_value
        .get("case")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let (verdict, detail) = match (&case, &workdir) {
        (None, _) => (
            "INCONCLUSIVE",
            format!("the escape-probe request names no case: {request}"),
        ),
        (Some(_), None) => (
            "INCONCLUSIVE",
            "the call preopens no work folder at `.`".to_string(),
        ),
        (Some(case), Some(workdir)) if case == "secrets" => secrets(&request_value, workdir),
        (Some(case), Some(workdir)) => run_case(case, workdir),
    };
    let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(workdir) = &workdir {
        let _ = fs::write(
            workdir.join(PROBE_FILE),
            format!("VERDICT={verdict}\nDETAIL={detail}\n"),
        );
    }
    result.summary = Some(format!("EC_VERDICT={verdict} EC_DETAIL={detail}"));
    result.metadata.retain(|(key, _)| key != REQUEST_KEY);
}

/// Not a registry case: what a guest worker can read of the secrets and records the supervisor
/// keeps. Reads its own `/proc/self/environ` and `/proc/self/cmdline`, each path in the request's
/// `read` list, the entries of each directory in its `list` list, and every file in the work
/// folder, and reports all of it as one JSON object, `SUCCESS` whatever it found: the caller
/// greps it for the key and checks which reads were refused.
fn secrets(request: &serde_json::Value, workdir: &Path) -> (&'static str, String) {
    const LIMIT: usize = 64 * 1024;
    let text = |bytes: Vec<u8>| {
        let shown = &bytes[..bytes.len().min(LIMIT)];
        String::from_utf8_lossy(shown).replace('\0', " ")
    };
    let read = |path: &Path| match fs::read(path) {
        Ok(bytes) => format!("ok:{}", text(bytes)),
        Err(error) => format!("err:{}", en(&error)),
    };
    let paths = |key: &str| -> Vec<String> {
        request
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|path| path.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut reads = serde_json::Map::new();
    for path in paths("read") {
        reads.insert(path.clone(), read(Path::new(&path)).into());
    }
    let mut lists = serde_json::Map::new();
    for path in paths("list") {
        let entries = match fs::read_dir(&path) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                format!("ok:{}", names.join(","))
            }
            Err(error) => format!("err:{}", en(&error)),
        };
        lists.insert(path, entries.into());
    }
    let mut files = serde_json::Map::new();
    let mut dirs = vec![workdir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok).take(256) {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(path),
                Ok(kind) if kind.is_file() && entry.file_name() != PROBE_FILE => {
                    files.insert(path.display().to_string(), read(&path).into());
                }
                _ => {}
            }
        }
    }
    let report = serde_json::json!({
        "environ": read(Path::new("/proc/self/environ")),
        "cmdline": read(Path::new("/proc/self/cmdline")),
        "reads": reads,
        "lists": lists,
        "workdir_files": files,
    });
    ("SUCCESS", report.to_string())
}

/// One case's body: `(verdict, detail)`. A case with no body here is `INCONCLUSIVE`, never a pass.
fn run_case(case: &str, workdir: &Path) -> (&'static str, String) {
    match case {
        "preflight" => preflight(),
        "read-etc-shadow" => read_etc_shadow(),
        "read-record-seal-key" => read_record_seal_key(workdir),
        "write-outside-workdir" => write_outside_workdir(),
        "stat-outside-workdir" => stat_outside_workdir(),
        "symlink-escape" => symlink_escape(workdir),
        "hardlink-escape" => hardlink_escape(workdir),
        "rename-across-boundary" => rename_across_boundary(workdir),
        "proc-pid-root-reopen" => proc_pid_root_reopen(),
        "proc-self-fd-reopen" => proc_self_fd_reopen(),
        "inherited-fd-after-exec" => inherited_fd(),
        "mknod-block-device-in-workdir" => mknod_block_device(workdir),
        "exec-renamed-disallowed-binary" => exec_renamed_binary(workdir),
        "connect-unlisted-tcp-host" => connect_unlisted_tcp_host(),
        "udp-exfiltration" => udp_exfiltration(),
        "dns-exfiltration" => dns_exfiltration(),
        "abstract-unix-socket-connect" => unix_socket_connect(true),
        "pathname-unix-socket-connect" => unix_socket_connect(false),
        "syscall-io-uring-setup" => grade_syscall(
            "io_uring_setup",
            libc::SYS_io_uring_setup,
            &[1, 0],
            "(a permitted call returns EFAULT for the NULL params pointer)",
        ),
        "syscall-userfaultfd" => grade_syscall(
            "userfaultfd",
            libc::SYS_userfaultfd,
            &[0],
            &format!(
                "(vm.unprivileged_userfaultfd={})",
                read_small("/proc/sys/vm/unprivileged_userfaultfd")
            ),
        ),
        "syscall-bpf" => grade_syscall("bpf", libc::SYS_bpf, &[0, 0, 0], &euid_note()),
        "syscall-open-by-handle-at" => grade_syscall(
            "open_by_handle_at",
            libc::SYS_open_by_handle_at,
            &[-100, 0, 0],
            &euid_note(),
        ),
        "syscall-perf-event-open" => grade_syscall(
            "perf_event_open",
            libc::SYS_perf_event_open,
            &[0, 0, -1, -1, 0],
            &euid_note(),
        ),
        "syscall-keyctl" => grade_syscall("keyctl", libc::SYS_keyctl, &[0, -3, 0], &euid_note()),
        other => (
            "INCONCLUSIVE",
            format!("the guest worker has no probe body for case {other:?}"),
        ),
    }
}

/// Symbolic errno, so a detail says `EACCES(13)` rather than `13`.
fn en(error: &io::Error) -> String {
    let Some(code) = error.raw_os_error() else {
        return error.to_string();
    };
    let name = match code {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EBADF => "EBADF",
        libc::EACCES => "EACCES",
        libc::EFAULT => "EFAULT",
        libc::EEXIST => "EEXIST",
        libc::EXDEV => "EXDEV",
        libc::ENOTDIR => "ENOTDIR",
        libc::EINVAL => "EINVAL",
        libc::EROFS => "EROFS",
        libc::ENOSYS => "ENOSYS",
        libc::ELOOP => "ELOOP",
        libc::EAFNOSUPPORT => "EAFNOSUPPORT",
        libc::ENETUNREACH => "ENETUNREACH",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::EHOSTUNREACH => "EHOSTUNREACH",
        _ => "E?",
    };
    format!("{name}({code})")
}

fn probe_open(path: &Path, write: bool, flags: i32) -> (Option<File>, String) {
    match OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => (Some(file), format!("open({})=ok", path.display())),
        Err(error) => (None, format!("open({})={}", path.display(), en(&error))),
    }
}

fn read_head(file: &mut File, limit: usize) -> usize {
    let mut buffer = vec![0u8; limit];
    file.read(&mut buffer).unwrap_or(0)
}

fn read_small(path: &str) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text.trim().to_string(),
        Err(error) => format!("unreadable {}", en(&error)),
    }
}

fn euid_note() -> String {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    format!("(euid={euid} -- a non-root call is EPERM from a missing capability alone)")
}

/// Attribution control for the `/proc` cases: whether `/proc` itself is reachable.
fn proc_reachable() -> String {
    match File::open("/proc/self/status") {
        Ok(_) => "proc_itself=reachable".to_string(),
        Err(error) => format!("proc_itself=denied open(/proc/self/status)={}", en(&error)),
    }
}

fn preflight() -> (&'static str, String) {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| "unread".to_string())
    };
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    (
        "SUCCESS",
        format!(
            "the guest worker's host side ran a probe as uid {euid}, pid {} in its PID \
             namespace (Seccomp: {}, NoNewPrivs: {}, NSpid: {}) -- this route can run a probe",
            std::process::id(),
            field("Seccomp:"),
            field("NoNewPrivs:"),
            field("NSpid:"),
        ),
    )
}

fn read_etc_shadow() -> (&'static str, String) {
    let (shadow, note) = probe_open(Path::new("/etc/shadow"), false, 0);
    let (passwd, passwd_note) = probe_open(Path::new("/etc/passwd"), false, 0);
    let control = if passwd.is_some() {
        "control /etc/passwd (world-readable, no grant): READABLE -- no filesystem mediation"
    } else {
        "control /etc/passwd (world-readable, no grant): refused -- filesystem mediation is active"
    };
    match shadow {
        Some(mut file) => (
            "ALLOWED",
            format!(
                "read {} bytes of /etc/shadow; {note}; {control}",
                read_head(&mut file, 32)
            ),
        ),
        None => ("REFUSED", format!("{note} ; {passwd_note} ; {control}")),
    }
}

fn read_record_seal_key(workdir: &Path) -> (&'static str, String) {
    let target = match fs::read_to_string(workdir.join("seal-key-path.txt")) {
        Ok(text) => PathBuf::from(text.trim()),
        Err(error) => {
            return (
                "INCONCLUSIVE",
                format!("the harness wrote no key path: {}", en(&error)),
            )
        }
    };
    let (key, note) = probe_open(&target, false, 0);
    let stat_note = match fs::metadata(&target) {
        Ok(_) => "stat=ok".to_string(),
        Err(error) => format!("stat={}", en(&error)),
    };
    match key {
        Some(mut file) => (
            "ALLOWED",
            format!(
                "read {} bytes of the record-seal key; {note}; {stat_note}",
                read_head(&mut file, 8)
            ),
        ),
        None => ("REFUSED", format!("{note} ; {stat_note}")),
    }
}

fn write_outside_workdir() -> (&'static str, String) {
    let target = format!("/var/tmp/murmur-escape-conformance-{}", std::process::id());
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&target)
    {
        Ok(mut file) => {
            let _ = file.write_all(b"escaped\n");
            (
                "ALLOWED",
                format!("wrote outside the workdir: {target} (remove it by hand)"),
            )
        }
        Err(error) => (
            "REFUSED",
            format!("create {target} refused with {}", en(&error)),
        ),
    }
}

fn stat_outside_workdir() -> (&'static str, String) {
    match fs::metadata("/etc/shadow") {
        Ok(meta) => (
            "SUCCESS",
            format!(
                "stat(/etc/shadow) succeeded: size={} -- metadata visible without any read \
                 grant, which is the documented behaviour of the scoped class, not an escape",
                meta.len()
            ),
        ),
        Err(error) => (
            "REFUSED",
            format!(
                "stat(/etc/shadow) refused with {} -- Landlock does not mediate metadata-only \
                 syscalls, so this is the path's absence",
                en(&error)
            ),
        ),
    }
}

fn symlink_escape(workdir: &Path) -> (&'static str, String) {
    let link = workdir.join("ec-symlink");
    let created = match std::os::unix::fs::symlink("/etc/passwd", &link) {
        Ok(()) => "symlink created",
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => "symlink already present",
        Err(error) => {
            return (
                "INCONCLUSIVE",
                format!(
                    "could not create the symlink at all: {} -- the read through it was never \
                     attempted",
                    en(&error)
                ),
            )
        }
    };
    match probe_open(&link, false, 0) {
        (Some(mut file), _) => (
            "ALLOWED",
            format!(
                "{created}; read {} bytes of /etc/passwd through it",
                read_head(&mut file, 32)
            ),
        ),
        (None, note) => (
            "REFUSED",
            format!("{created}; read through it refused: {note}"),
        ),
    }
}

fn hardlink_escape(workdir: &Path) -> (&'static str, String) {
    let link = workdir.join("ec-hardlink");
    let protected = read_small("/proc/sys/fs/protected_hardlinks");
    match fs::hard_link("/etc/passwd", &link) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            return (
                "INCONCLUSIVE",
                format!(
                    "EXDEV: the workdir and /etc are on different filesystems, so this host \
                     cannot attempt the hard link at all (protected_hardlinks={protected})"
                ),
            )
        }
        Err(error) => {
            return (
                "REFUSED",
                format!(
                    "link(/etc/passwd) refused with {} (protected_hardlinks={protected} -- EPERM \
                     may be that sysctl rather than Landlock)",
                    en(&error)
                ),
            )
        }
    }
    match probe_open(&link, false, 0) {
        (Some(mut file), _) => (
            "ALLOWED",
            format!(
                "hard link created and read: {} bytes of /etc/passwd aliased into the workdir",
                read_head(&mut file, 32)
            ),
        ),
        (None, note) => (
            "REFUSED",
            format!("hard link created but unreadable: {note}"),
        ),
    }
}

fn rename_across_boundary(workdir: &Path) -> (&'static str, String) {
    let source = workdir.join("ec-rename-src");
    let target = format!(
        "/var/tmp/murmur-escape-conformance-rename-{}",
        std::process::id()
    );
    if let Err(error) = fs::write(&source, b"escaped\n") {
        return (
            "INCONCLUSIVE",
            format!("could not write {}: {}", source.display(), en(&error)),
        );
    }
    match fs::rename(&source, &target) {
        Ok(()) => (
            "ALLOWED",
            format!("renamed a workdir file to {target}, outside the workdir (remove it by hand)"),
        ),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => (
            "INCONCLUSIVE",
            "EXDEV: the workdir and /var/tmp are on different filesystems, so rename(2) across \
             this boundary is impossible here for reasons unrelated to containment"
                .to_string(),
        ),
        Err(error) => (
            "REFUSED",
            format!(
                "rename {} -> {target} refused with {}",
                source.display(),
                en(&error)
            ),
        ),
    }
}

fn proc_pid_root_reopen() -> (&'static str, String) {
    let control = proc_reachable();
    // `/proc/self`, not `/proc/<getpid()>`: a worker forked into the session's PID namespace
    // reads its namespace-local pid, which names another process in a `/proc` that is not the
    // namespace's own.
    let path = Path::new("/proc/self/root/etc/passwd");
    match probe_open(path, false, 0) {
        (Some(mut file), _) => (
            "ALLOWED",
            format!(
                "read {} bytes of /etc/passwd through {} ; {control}",
                read_head(&mut file, 32),
                path.display()
            ),
        ),
        (None, note) => ("REFUSED", format!("{note} ; {control}")),
    }
}

fn proc_self_fd_reopen() -> (&'static str, String) {
    use std::os::fd::AsRawFd;
    let control = proc_reachable();
    let (base, base_note) = probe_open(Path::new("/dev/urandom"), false, 0);
    let Some(base) = base else {
        return (
            "INCONCLUSIVE",
            format!(
                "could not open the read-only base descriptor, so no upgrade was attempted: \
                 {base_note} ; {control}"
            ),
        );
    };
    let path = PathBuf::from(format!("/proc/self/fd/{}", base.as_raw_fd()));
    match probe_open(&path, true, 0) {
        (Some(_), _) => (
            "ALLOWED",
            format!(
                "re-opened the read-only /dev/urandom descriptor as O_RDWR through {} ; \
                 {control}",
                path.display()
            ),
        ),
        (None, note) => {
            let same_mode = match probe_open(&path, false, 0) {
                (Some(_), _) => "succeeded".to_string(),
                (None, note) => note,
            };
            (
                "REFUSED",
                format!(
                    "the O_RDWR upgrade through {} was refused ({note}); the same-mode re-open \
                     {same_mode} ; {control}",
                    path.display()
                ),
            )
        }
    }
}

fn inherited_fd() -> (&'static str, String) {
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::fs::MetadataExt;
    // The file the case's `Prepare::LeakFdIntoMur` opens on fd 7 in the shell that launches `mur`.
    // A worker opens descriptors of its own after it starts, so fd 7 being open says nothing:
    // the question is whether any open descriptor is that file.
    let leaked = Path::new("/etc/hostname");
    let leaked_id = fs::metadata(leaked)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()));
    let mut open = Vec::new();
    let mut matches = Vec::new();
    for fd in 3..4096 {
        // SAFETY: `fcntl(F_GETFD)` takes an integer and reads nothing through a pointer.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            continue;
        }
        open.push(fd);
        let link = fs::read_link(format!("/proc/self/fd/{fd}")).ok();
        // SAFETY: `fd` is open (checked above); the `File` is turned back into a raw descriptor
        // before it drops, so this borrows the descriptor without closing it.
        let file = unsafe { File::from_raw_fd(fd) };
        let id = file.metadata().ok().map(|meta| (meta.dev(), meta.ino()));
        let _ = file.into_raw_fd();
        if link.as_deref() == Some(leaked) || (leaked_id.is_some() && id == leaked_id) {
            matches.push(fd);
        }
    }
    if matches.is_empty() {
        (
            "REFUSED",
            format!(
                "no descriptor open in the guest worker is {} (open fds above stdio: {open:?}; \
                 fd 3 is its connection to mur); the descriptor the launching shell leaked into \
                 mur on fd 7 did not reach it",
                leaked.display()
            ),
        )
    } else {
        (
            "ALLOWED",
            format!(
                "fd(s) {matches:?} in the guest worker are {} -- the descriptor the launching \
                 shell leaked into mur reached the worker (all extra fds: {open:?})",
                leaked.display()
            ),
        )
    }
}

fn mknod_block_device(workdir: &Path) -> (&'static str, String) {
    let node = workdir.join("ec-pwn");
    let Ok(path) = CString::new(node.as_os_str().as_encoded_bytes()) else {
        return ("INCONCLUSIVE", "the node path holds a NUL".to_string());
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let rc = unsafe { libc::mknod(path.as_ptr(), libc::S_IFBLK | 0o600, libc::makedev(8, 0)) };
    if rc != 0 {
        let error = io::Error::last_os_error();
        let which = match error.raw_os_error() {
            Some(libc::EACCES) => "Landlock (workdir grant withholds MakeBlock)",
            Some(libc::EPERM) => "CAP_MKNOD missing",
            _ => "unexpected",
        };
        return (
            "REFUSED",
            format!(
                "mknod({}, blk 8:0) refused with {}: {which}",
                node.display(),
                en(&error)
            ),
        );
    }
    match probe_open(&node, false, 0) {
        (Some(mut file), _) => (
            "ALLOWED",
            format!(
                "created a block device node for 8:0 inside the workdir and read {} raw bytes",
                read_head(&mut file, 512)
            ),
        ),
        (None, note) => (
            "ALLOWED",
            format!(
                "the block device node was CREATED inside the workdir ({}), though opening it \
                 was refused: {note}",
                node.display()
            ),
        ),
    }
}

fn exec_renamed_binary(workdir: &Path) -> (&'static str, String) {
    let target = workdir.join("bash");
    if !target.exists() {
        return (
            "INCONCLUSIVE",
            format!(
                "the harness did not stage {} -- nothing was exercised",
                target.display()
            ),
        );
    }
    match std::process::Command::new(&target)
        .arg("--version")
        .env_clear()
        .output()
    {
        Ok(output) => (
            "ALLOWED",
            format!(
                "execve({}) SUCCEEDED (rc={:?}, {} bytes of output) -- the guest worker ran a \
                 binary out of the work folder",
                target.display(),
                output.status.code(),
                output.stdout.len() + output.stderr.len()
            ),
        ),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => (
            "INCONCLUSIVE",
            format!(
                "execve({}) failed with ENOENT -- staging problem, not a containment result",
                target.display()
            ),
        ),
        Err(error) => (
            "REFUSED",
            format!(
                "execve({}) refused with {} -- the worker holds no Execute grant on the work \
                 folder",
                target.display(),
                en(&error)
            ),
        ),
    }
}

fn connect_unlisted_tcp_host() -> (&'static str, String) {
    let address: SocketAddr = ([1, 1, 1, 1], 443).into();
    match TcpStream::connect_timeout(&address, Duration::from_secs(8)) {
        Ok(_) => (
            "ALLOWED",
            "connected to 1.1.1.1:443 with an empty network.allow".to_string(),
        ),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => (
            "INCONCLUSIVE",
            "connect(1.1.1.1:443) timed out -- no route from this host, so nothing about the \
             network boundary was exercised"
                .to_string(),
        ),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENETUNREACH | libc::EHOSTUNREACH)
            ) && !in_own_network_namespace() =>
        {
            (
                "INCONCLUSIVE",
                format!(
                    "connect(1.1.1.1:443) got {} -- the host has no route, so nothing was \
                     exercised",
                    en(&error)
                ),
            )
        }
        Err(error) => (
            "REFUSED",
            format!("connect(1.1.1.1:443) refused with {}", en(&error)),
        ),
    }
}

/// Whether this process's network namespace is not the initial one: the session's namespace has
/// no route out, so an unreachable network there is the boundary, not a host without a route.
fn in_own_network_namespace() -> bool {
    match (
        fs::read_link("/proc/self/ns/net"),
        fs::read_link("/proc/1/ns/net"),
    ) {
        (Ok(own), Ok(init)) => own != init,
        _ => true,
    }
}

fn udp_exfiltration() -> (&'static str, String) {
    // Not port 53: the runtime's own resolver answers that inside the session's namespace.
    let port = 46053;
    let payload = format!("murmur-escape-conformance-udp-{}", std::process::id());
    let receiver = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(socket) => socket,
        Err(error) => {
            return (
                "INCONCLUSIVE",
                format!(
                    "could not bind the same-namespace receiver on 0.0.0.0:{port} ({}), so a \
                     successful sendto could not be told apart from a real egress",
                    en(&error)
                ),
            )
        }
    };
    let _ = receiver.set_read_timeout(Some(Duration::from_secs(5)));
    let sender = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(socket) => socket,
        Err(error) => {
            return (
                "REFUSED",
                format!("socket(AF_INET, SOCK_DGRAM) refused with {}", en(&error)),
            )
        }
    };
    let sent = match sender.send_to(payload.as_bytes(), ("1.1.1.1", port)) {
        Ok(sent) => sent,
        Err(error) => {
            return (
                "REFUSED",
                format!(
                    "sendto(1.1.1.1:{port}) refused with {} before any datagram was written",
                    en(&error)
                ),
            )
        }
    };
    let mut buffer = [0u8; 128];
    match receiver.recv_from(&mut buffer) {
        Ok((len, from)) if &buffer[..len] == payload.as_bytes() => (
            "REFUSED",
            format!(
                "the datagram never left the host: sendto accepted {sent} bytes to \
                 1.1.1.1:{port} and the identical payload arrived at a receiver bound on \
                 0.0.0.0:{port} in this same network namespace, from {from}"
            ),
        ),
        Ok((len, from)) => (
            "INCONCLUSIVE",
            format!(
                "the receiver on 0.0.0.0:{port} saw {len} bytes from {from} that are not the \
                 payload sent"
            ),
        ),
        Err(error) => (
            "ALLOWED",
            format!(
                "sendto accepted {sent} bytes to 1.1.1.1:{port} and the receiver in this same \
                 network namespace saw nothing within 5s ({}) -- the datagram went somewhere \
                 this probe cannot account for",
                en(&error)
            ),
        ),
    }
}

fn dns_exfiltration() -> (&'static str, String) {
    let name = format!(
        "ec-exfil-{}.murmur-escape-conformance.invalid",
        std::process::id()
    );
    let Ok(node) = CString::new(name.clone()) else {
        return ("INCONCLUSIVE", "the name holds a NUL".to_string());
    };
    let mut result: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `node` is NUL-terminated and outlives the call; a null service and hints are
    // allowed; `result` is written by the call and freed below when it succeeded.
    let rc = unsafe {
        libc::getaddrinfo(
            node.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut result,
        )
    };
    if rc == 0 {
        // SAFETY: `result` came from a successful `getaddrinfo`.
        unsafe { libc::freeaddrinfo(result) };
        return ("ALLOWED", format!("resolved {name}"));
    }
    // SAFETY: `gai_strerror` returns a static NUL-terminated string for any code.
    let message = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(rc)) }
        .to_string_lossy()
        .into_owned();
    if rc == libc::EAI_NONAME || rc == libc::EAI_NODATA {
        (
            "ALLOWED",
            format!(
                "the lookup for {name} reached a resolver and came back NXDOMAIN ({message}) -- \
                 the queried label left the host"
            ),
        )
    } else {
        (
            "REFUSED",
            format!("resolution of {name} failed before leaving the host: {message} ({rc})"),
        )
    }
}

fn unix_socket_connect(abstract_name: bool) -> (&'static str, String) {
    // SAFETY: `socket` takes integers.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return (
            "REFUSED",
            format!(
                "socket(AF_UNIX) itself refused with {} -- refused at socket(), not at connect()",
                en(&io::Error::last_os_error())
            ),
        );
    }
    let targets: Vec<Vec<u8>> = if abstract_name {
        vec![b"\0murmur-escape-conformance-abstract".to_vec()]
    } else {
        vec![
            b"/var/run/docker.sock".to_vec(),
            b"/run/docker.sock".to_vec(),
        ]
    };
    let mut notes = Vec::new();
    for target in targets {
        // SAFETY: an all-zero `sockaddr_un` is a valid value of the type.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in address.sun_path.iter_mut().zip(target.iter()) {
            *slot = *byte as libc::c_char;
        }
        let len =
            std::mem::size_of::<libc::sa_family_t>() + target.len() + usize::from(!abstract_name);
        // SAFETY: `address` is a valid `sockaddr_un` that outlives the call, and `len` does not
        // exceed its size.
        let rc = unsafe {
            libc::connect(
                fd,
                std::ptr::addr_of!(address).cast(),
                len as libc::socklen_t,
            )
        };
        let shown = String::from_utf8_lossy(&target).replace('\0', "@");
        if rc == 0 {
            // SAFETY: `fd` is this function's own socket.
            unsafe { libc::close(fd) };
            return ("ALLOWED", format!("socket(AF_UNIX) connected to {shown}"));
        }
        notes.push(format!("{shown}={}", en(&io::Error::last_os_error())));
    }
    // SAFETY: `fd` is this function's own socket.
    unsafe { libc::close(fd) };
    (
        "REFUSED",
        format!(
            "socket(AF_UNIX) SUCCEEDED; every connect was refused ({}) -- connect-time, not the \
             socket()-time domain rule",
            notes.join("; ")
        ),
    )
}

/// The six dangerous-syscall cases: the call by number, never through a libc wrapper that could
/// return before entering the kernel. `EPERM` is the seccomp filter's default action; `ENOSYS`
/// answers nothing; any other outcome reached the kernel implementation.
fn grade_syscall(
    name: &str,
    number: libc::c_long,
    args: &[libc::c_long],
    extra: &str,
) -> (&'static str, String) {
    let mut padded = [0 as libc::c_long; 5];
    for (slot, value) in padded.iter_mut().zip(args) {
        *slot = *value;
    }
    // SAFETY: each probed call is given null pointers or invalid descriptors, which the kernel
    // rejects without writing through them, and its own return value is all that is read.
    let ret = unsafe {
        libc::syscall(
            number, padded[0], padded[1], padded[2], padded[3], padded[4],
        )
    };
    let error = io::Error::last_os_error();
    let note = format!("nr={number} ret={ret} errno={}", en(&error));
    if ret >= 0 {
        if number == libc::SYS_userfaultfd {
            // SAFETY: a successful `userfaultfd` returns a descriptor this function owns.
            unsafe { libc::close(ret as libc::c_int) };
        }
        return (
            "ALLOWED",
            format!("{name} reached the kernel implementation (not refused) {note} {extra}"),
        );
    }
    match error.raw_os_error() {
        Some(libc::EPERM) => (
            "REFUSED",
            format!("{name} refused with EPERM (seccomp default action) {note} {extra}"),
        ),
        Some(libc::ENOSYS) => (
            "INCONCLUSIVE",
            format!(
                "{name}: ENOSYS -- kernel does not implement it, nothing proven {note} {extra}"
            ),
        ),
        _ => (
            "ALLOWED",
            format!("{name} reached the kernel implementation (not refused) {note} {extra}"),
        ),
    }
}
