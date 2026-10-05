//! Loopback ports claimed for the lifetime of a test process, and kept from every other test
//! process on the host.
//!
//! For tests that need the number itself: to write into a manifest or onto a command line before
//! the process that binds it starts, to restart something on the same port, or to point a client
//! at a port nothing listens on. An OS-assigned `:0` port serves none of these, because the number
//! is only known once something has bound it, and once released the kernel can hand it to any
//! other binder on the host.
//!
//! Candidates come from below the Linux ephemeral range (32768–60999), so no `:0` bind anywhere
//! can be assigned one. Each claim holds an advisory lock on a file named after the port in a
//! directory every test process shares, and a candidate whose lock another process holds is
//! skipped. cargo-nextest runs every test in its own process, so a claim set kept in memory would
//! only keep apart the tests that share a binary. The lock goes with the process: the kernel
//! releases it on exit however the test ended, and a later run can claim the port again.
//!
//! This file uses only `std` and no `crate::` path, because the `murmur-cli` and `mur-roost`
//! integration tests compile it too, through `#[path]`. One copy keeps every crate's tests on the
//! same range and the same lock directory.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    net::TcpListener,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

const FIRST: u16 = 20_000;
const LAST: u16 = 30_000;

/// The directory the per-port lock files live in.
fn lock_dir() -> PathBuf {
    std::env::temp_dir().join("murmur-test-ports")
}

/// A port in `20000..=30000` that was bindable when claimed and that no other test process on
/// this host can claim until this one exits.
///
/// Panics when every candidate is taken.
pub fn claim() -> u16 {
    // The cursor, and the lock files whose locks are the claims. Neither is ever dropped, so every
    // lock lasts until the process exits.
    static CLAIMS: OnceLock<Mutex<(u16, Vec<File>)>> = OnceLock::new();
    let claims = CLAIMS.get_or_init(|| {
        // Seeded from the pid so concurrent processes start in different places rather than all
        // contending for the first free candidate. The locks, not the seed, keep them apart.
        let span = u32::from(LAST - FIRST + 1);
        let seed = (std::process::id() % span) as u16;
        Mutex::new((FIRST + seed, Vec::new()))
    });

    let dir = lock_dir();
    if let Err(error) = fs::create_dir_all(&dir) {
        panic!(
            "cannot create the port lock directory {}: {error}",
            dir.display()
        );
    }

    let mut guard = claims.lock().unwrap();
    let mut last_error = None;
    for _ in FIRST..=LAST {
        let candidate = guard.0;
        guard.0 = if candidate >= LAST {
            FIRST
        } else {
            candidate + 1
        };

        let path = dir.join(format!("{candidate}.lock"));
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                last_error = Some(format!("{}: {error}", path.display()));
                continue;
            }
        };
        // `flock` locks belong to the open file description, so this also refuses a port this
        // process claimed earlier: that claim's `File` is a separate open of the same path.
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => {
                last_error = Some(format!("{}: {error}", path.display()));
                continue;
            }
        }
        // Held by something outside the test suite. Dropping `file` releases the lock.
        if TcpListener::bind(("127.0.0.1", candidate)).is_err() {
            continue;
        }
        guard.1.push(file);
        return candidate;
    }
    match last_error {
        Some(error) => panic!("no claimable port in {FIRST}..={LAST}; last error: {error}"),
        None => panic!("no claimable port in {FIRST}..={LAST}"),
    }
}
