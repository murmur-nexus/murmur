//! What a test that launches a formation with `mur run --roster` needs: one launch at a time on
//! the host, and the check that no member outlives the launcher.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Held while a test launches a formation. Each launch starts several `mur run` processes, and
/// several launches at once on one host turn a readiness deadline into a measure of contention.
/// Exclusive across the threads of one test binary and, through an `flock` on a file in the
/// temp directory, across test binaries.
pub struct LaunchLock {
    _thread: MutexGuard<'static, ()>,
    _host: File,
}

pub fn launch_lock() -> LaunchLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let thread = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let host = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("murmur-cli-formation-launch.lock"))
        .unwrap();
    // SAFETY: `flock` takes the descriptor `host` owns, which stays open until the returned guard
    // is dropped; it dereferences no memory. Closing the descriptor releases the lock.
    let locked = unsafe { libc::flock(host.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(locked, 0, "flock: {}", std::io::Error::last_os_error());
    LaunchLock {
        _thread: thread,
        _host: host,
    }
}

/// Whether `pid` is a live process: present, and not a zombie.
pub fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                != Some("Z")
        }
        Err(_) => false,
    }
}

/// Every live process whose command line contains `needle`.
pub fn processes_mentioning(needle: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline.contains(needle) && alive(pid) {
            found.push((pid, cmdline));
        }
    }
    found
}

/// Fail unless, within `limit`, no process on the host carries `project` on its command line and
/// every pid in `pids` is dead. Every member runs with `--workdir <project>`, so a unique project
/// path finds every member, reported or not.
pub fn assert_no_member_remains(project: &Path, pids: &[u32], limit: Duration) {
    let needle = project.to_string_lossy().into_owned();
    let deadline = Instant::now() + limit;
    loop {
        let holders = processes_mentioning(&needle);
        let alive: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
        if holders.is_empty() && alive.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "member processes remain after {limit:?}: by command line {holders:?}, by \
                 reported pid {alive:?}"
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}
