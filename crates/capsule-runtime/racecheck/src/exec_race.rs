//! Race probe: `execve(2)`'s `filename` argument, against the seccomp-notify supervisor's
//! read-then-`notify_id_valid`-then-`CONTINUE` pattern.
//!
//! This is the race `decide_exec_allowed`'s own doc comment named as open while that function
//! existed. It has since been deleted along with the rest of the exec supervisor — exec is a
//! Landlock `Execute` right now, decided in-kernel on the resolved path — so this probe mirrors a
//! retired design; see this crate's `lib.rs` header. The probe's job
//! is to make it measurable rather than merely admitted.
//!
//! Layout of a run:
//!
//! - A scratch directory holds one symlink (`target-link`) and one marker path. The symlink
//!   normally points at an allowlisted binary (`true`, which ignores its arguments); the
//!   "disallowed" retarget points at `touch`, which — given the same `argv` — creates the marker
//!   file. That asymmetry is the whole trick: one fixed `argv` is inert for the allowed binary and
//!   self-evidencing for the disallowed one, so the probe needs no compiled helper.
//! - The child installs a notify filter on `execve`, then runs two threads. Thread A repeatedly
//!   `fork`s and `execve`s **the symlink path** (`execve` replaces the caller, so each attempt
//!   needs its own process). Thread B spins, atomically retargeting that symlink between the
//!   allowed and disallowed binaries via `symlink` + `rename`.
//! - The parent supervises exactly as production does: read the pathname string out of the
//!   notifying task's memory, `canonicalize` it, allow iff the canonical path is the allowlisted
//!   binary, answer allows with `new_continue`.
//!
//! **What counts as a win.** The marker file exists after an attempt. The supervisor's decision is
//! keyed on the canonical path it resolved at check time; if that was the allowed binary and the
//! marker nonetheless appears, the kernel re-resolved the same pathname *after* the response and
//! executed the disallowed binary. A supervisor that had instead read the disallowed target would
//! have answered `EACCES` and no binary would have run at all, so a marker cannot be produced by
//! any path other than a won race.
//!
//! **What counts as a loss.** `RACE_WON: 0/N` is a valid, reportable outcome.

#[cfg(not(target_os = "linux"))]
fn main() {
    racecheck::non_linux_stub("exec_race");
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux::run() {
        eprintln!("exec_race: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use racecheck::linux::{
        close, exit_code, fork, install_notify_filter, read_cstr_from_child,
        receive_fd_over_socket, socketpair, supervise, waitpid, Decision,
    };
    use racecheck::{iterations_from_args, progress_every};
    use std::ffi::CString;
    use std::os::fd::RawFd;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Deliberately lower than `connect_race`'s default: every attempt here costs a `fork` plus a
    /// full `execve` of a real binary — roughly three orders of magnitude more expensive than a
    /// loopback `connect` — so 20k attempts already run for minutes. Raise it with `--iterations`
    /// once a run's throughput on the target host is known.
    const DEFAULT_ITERATIONS: u64 = 20_000;

    const ALLOWED_CANDIDATES: &[&str] = &["/bin/true", "/usr/bin/true"];
    const DISALLOWED_CANDIDATES: &[&str] = &["/bin/touch", "/usr/bin/touch"];

    struct Paths {
        dir: PathBuf,
        link: PathBuf,
        swap: PathBuf,
        marker: PathBuf,
        allowed: PathBuf,
        disallowed: PathBuf,
    }

    pub fn run() -> Result<(), String> {
        let iterations = iterations_from_args(DEFAULT_ITERATIONS)?;
        let paths = prepare()?;

        println!(
            "exec_race: link={} allowed={} disallowed={} iterations={iterations}",
            paths.link.display(),
            paths.allowed.display(),
            paths.disallowed.display()
        );

        let (parent_sock, child_sock) = socketpair().map_err(|e| format!("socketpair: {e}"))?;

        // SAFETY: this process is still single-threaded here, so the child inherits no held locks
        // and may allocate and spawn threads freely.
        let pid = unsafe { fork() }.map_err(|e| format!("fork: {e}"))?;
        if pid == 0 {
            close(parent_sock);
            let code = match child(child_sock, &paths, iterations) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("exec_race[child]: {error}");
                    1
                }
            };
            std::process::exit(code);
        }
        close(child_sock);

        let result = parent(parent_sock, pid, &paths);
        let _ = std::fs::remove_dir_all(&paths.dir);
        result
    }

    /// Resolves the two binaries, creates the scratch directory, and points the symlink at the
    /// allowed binary to start with.
    fn prepare() -> Result<Paths, String> {
        let allowed = first_existing(ALLOWED_CANDIDATES).ok_or_else(|| {
            format!("none of {ALLOWED_CANDIDATES:?} exist; this probe needs a `true` binary")
        })?;
        let disallowed = first_existing(DISALLOWED_CANDIDATES).ok_or_else(|| {
            format!("none of {DISALLOWED_CANDIDATES:?} exist; this probe needs a `touch` binary")
        })?;
        if allowed == disallowed {
            return Err("`true` and `touch` canonicalize to the same path".to_string());
        }

        let dir = std::env::temp_dir().join(format!("murmur-exec-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let paths = Paths {
            link: dir.join("target-link"),
            swap: dir.join("target-link.swap"),
            marker: dir.join("marker"),
            allowed,
            disallowed,
            dir,
        };
        retarget(&paths.swap, &paths.link, &paths.allowed)
            .map_err(|e| format!("initial symlink: {e}"))?;
        Ok(paths)
    }

    fn first_existing(candidates: &[&str]) -> Option<PathBuf> {
        candidates
            .iter()
            .find_map(|candidate| std::fs::canonicalize(candidate).ok())
    }

    /// Atomically repoints `link` at `target`: build the new symlink under a scratch name, then
    /// `rename` it over the old one. `rename(2)` over an existing symlink is atomic, so `link`
    /// always resolves to one of the two binaries and never transiently disappears — an attempt
    /// can therefore never fail with `ENOENT` and be mistaken for a denial.
    fn retarget(swap: &Path, link: &Path, target: &Path) -> std::io::Result<()> {
        let _ = std::fs::remove_file(swap);
        std::os::unix::fs::symlink(target, swap)?;
        std::fs::rename(swap, link)
    }

    // ---- child: the racing process ------------------------------------------------------------

    fn child(child_sock: RawFd, paths: &Paths, iterations: u64) -> Result<(), String> {
        install_notify_filter(&["execve"], child_sock)
            .map_err(|e| format!("install notify filter: {e}"))?;
        close(child_sock);

        let stop = Arc::new(AtomicBool::new(false));
        let flipper = {
            let stop = Arc::clone(&stop);
            let (swap, link, allowed, disallowed) = (
                paths.swap.clone(),
                paths.link.clone(),
                paths.allowed.clone(),
                paths.disallowed.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = retarget(&swap, &link, &disallowed);
                    let _ = retarget(&swap, &link, &allowed);
                }
            })
        };

        // Every buffer `execve` will touch is built here, before the fork loop. The forked
        // grandchild runs in a process whose parent was multi-threaded, so it must do nothing but
        // async-signal-safe work — no allocation, no locks — between `fork` and `execve`/`_exit`.
        // Pre-building the `CString`s and the pointer arrays is what makes that true.
        let link_c = cstring(&paths.link)?;
        let marker_c = cstring(&paths.marker)?;
        let argv0 = CString::new("probe").expect("no interior NUL");
        // `true` ignores argv entirely; `touch` creates the file named by argv[1]. One argv, two
        // very different outcomes — that is the evidence channel.
        let argv: [*const libc::c_char; 3] =
            [argv0.as_ptr(), marker_c.as_ptr(), std::ptr::null()];
        let envp: [*const libc::c_char; 1] = [std::ptr::null()];

        let mut wins = 0u64;
        let mut ran_allowed = 0u64;
        let mut denied = 0u64;
        let mut other = 0u64;
        let progress = progress_every(iterations);

        for i in 0..iterations {
            let _ = std::fs::remove_file(&paths.marker);

            // SAFETY: the grandchild below executes only `execve` and `_exit`, both
            // async-signal-safe, using pointers into allocations made before this loop and kept
            // alive by locals that outlive it. That is the contract `fork`'s docs require when the
            // forking process has other threads running (thread B does).
            let pid = match unsafe { fork() } {
                Ok(pid) => pid,
                Err(_) => {
                    other += 1;
                    continue;
                }
            };
            if pid == 0 {
                // SAFETY: as above — no allocation, no locks, no destructors.
                unsafe {
                    libc::execve(link_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
                    libc::_exit(127);
                }
            }

            match waitpid(pid).ok().and_then(exit_code) {
                Some(0) => {
                    if paths.marker.exists() {
                        wins += 1;
                    } else {
                        ran_allowed += 1;
                    }
                }
                // 127 is this probe's own post-`execve` `_exit`: the syscall never ran, which for
                // this filter means the supervisor denied it.
                Some(127) => denied += 1,
                _ => other += 1,
            }

            if (i + 1) % progress == 0 {
                println!(
                    "exec_race: {}/{iterations} attempts, {wins} win(s) so far",
                    i + 1
                );
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = flipper.join();
        // Leave the link pointing somewhere harmless for anyone inspecting the scratch dir.
        let _ = retarget(&paths.swap, &paths.link, &paths.allowed);
        let _ = std::fs::remove_file(&paths.marker);

        println!("RACE_WON: {wins}/{iterations}");
        println!(
            "exec_race: detail: disallowed-binary-ran={wins} allowed-binary-ran={ran_allowed} \
             denied-EACCES={denied} other={other}"
        );
        Ok(())
    }

    fn cstring(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("path contains an interior NUL: {}", path.display()))
    }

    // ---- parent: the supervisor ---------------------------------------------------------------

    fn parent(parent_sock: RawFd, pid: libc::pid_t, paths: &Paths) -> Result<(), String> {
        let notify_fd =
            receive_fd_over_socket(parent_sock).map_err(|e| format!("receive notify fd: {e}"))?;
        close(parent_sock);

        // The audited pattern, verbatim: read the pathname out of the notifying task, resolve it
        // to a canonical identity, then `CONTINUE`. Mirrors `classify_and_decide`'s `execve` arm
        // plus `decide_exec_allowed` against a one-entry allowlist. Only absolute pathnames are
        // handled — the probe always passes one, so the `/proc/<pid>/cwd` base-resolution branch
        // of `decide_exec_allowed` is not exercised and is not part of this race.
        let allowed = paths.allowed.clone();
        let stats = supervise(notify_fd, |req| {
            match read_cstr_from_child(req.pid, req.data.args[0]) {
                Ok(pathname) => match std::fs::canonicalize(&pathname) {
                    Ok(canonical) if canonical == allowed => Decision::Allow,
                    _ => Decision::Deny,
                },
                Err(_) => Decision::Deny,
            }
        });

        let status = waitpid(pid).map_err(|e| format!("waitpid: {e}"))?;

        println!(
            "exec_race: supervisor: allowed={} denied={} stale-id={}",
            stats.allowed, stats.denied, stats.stale
        );
        if stats.allowed == 0 && stats.denied == 0 {
            println!(
                "exec_race: WARNING: the supervisor saw no notifications at all. The run proves \
                 nothing — check that /proc/<pid>/mem is readable (CAP_SYS_PTRACE, or a \
                 ptrace_scope that permits a grandparent read) before recording a result."
            );
        }
        match exit_code(status) {
            Some(0) => Ok(()),
            Some(code) => Err(format!("child exited with status {code}")),
            None => Err("child was killed by a signal".to_string()),
        }
    }
}
