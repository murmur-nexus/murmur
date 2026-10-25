//! The host's TLS trust store, found at staging for a capsule whose commands reach the network.
//!
//! On `scoped` a command's filesystem reach is a Landlock ruleset, and a path with no rule is
//! denied. An allowlisted `curl`, `git` or `python3` that may open a TLS connection still has to
//! read the certificates that verify the server, so without a rule for them it fails certificate
//! verification over an otherwise-working connection. Where a host keeps them differs by
//! distribution, so the runtime checks a fixed table of well-known locations against this host and
//! grants what exists (see [`crate::sandbox::resolve_trust_store_grants`]): readable and listable,
//! never executable, never writable.
//!
//! Nothing is derived unless the capsule allowlists a binary *and* declares a network allowlist:
//! a command that cannot reach the network has no certificate to verify. No subprocess is run and
//! `SSL_CERT_FILE` / `SSL_CERT_DIR` are not read: they are `mur`'s own environment, which the
//! capsule's commands do not receive, and they can name any file. A trust store outside the table
//! is reachable through `capabilities.shell.interpreter_runtime`.

use std::path::{Path, PathBuf};

/// One host path staging granted as part of the trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStorePath {
    /// Canonical: symlinks resolved, so the rule names the inode the host really keeps.
    pub path: PathBuf,
    /// Whether `path` is a directory, which decides whether its grant is listable.
    pub directory: bool,
}

/// One well-known trust-store location: a CA bundle file or a hashed certificate directory.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrustStoreCandidate<'a> {
    /// Absolute, as a distribution spells it; possibly a symlink.
    pub(crate) path: &'a str,
    /// Whether the location is a directory; a candidate that turns out to be the other kind is
    /// not the trust store and contributes nothing.
    pub(crate) directory: bool,
}

/// Every location the runtime checks, in order: the union of the Linux lists the Go standard
/// library (`crypto/x509/root_linux.go`) and the `openssl-probe` crate search. To add one, name a
/// path a distribution ships its CA bundle or hashed certificate directory at; never a directory
/// that also holds keys or configuration, because a directory grant covers everything beneath it.
#[cfg(target_os = "linux")]
pub(crate) const TRUST_STORE_CANDIDATES: &[TrustStoreCandidate<'static>] = &[
    // Debian, Ubuntu, Arch, Gentoo.
    TrustStoreCandidate {
        path: "/etc/ssl/certs/ca-certificates.crt",
        directory: false,
    },
    // Fedora, RHEL.
    TrustStoreCandidate {
        path: "/etc/pki/tls/certs/ca-bundle.crt",
        directory: false,
    },
    // RHEL 7+, Fedora, CentOS.
    TrustStoreCandidate {
        path: "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        directory: false,
    },
    // openSUSE.
    TrustStoreCandidate {
        path: "/etc/ssl/ca-bundle.pem",
        directory: false,
    },
    // OpenELEC.
    TrustStoreCandidate {
        path: "/etc/pki/tls/cacert.pem",
        directory: false,
    },
    // Alpine.
    TrustStoreCandidate {
        path: "/etc/ssl/cert.pem",
        directory: false,
    },
    // Debian's OpenSSL default `OPENSSLDIR`, a symlink to the Debian bundle.
    TrustStoreCandidate {
        path: "/usr/lib/ssl/cert.pem",
        directory: false,
    },
    // Debian, Ubuntu, Alpine, Arch: the hashed `CApath`.
    TrustStoreCandidate {
        path: "/etc/ssl/certs",
        directory: true,
    },
    // Fedora, RHEL.
    TrustStoreCandidate {
        path: "/etc/pki/tls/certs",
        directory: true,
    },
    // RHEL 7+, Fedora.
    TrustStoreCandidate {
        path: "/etc/pki/ca-trust/extracted/pem",
        directory: true,
    },
    // Debian's OpenSSL default `CApath`, a symlink to `/etc/ssl/certs`.
    TrustStoreCandidate {
        path: "/usr/lib/ssl/certs",
        directory: true,
    },
    // openSUSE.
    TrustStoreCandidate {
        path: "/var/lib/ca-certificates/pem",
        directory: true,
    },
];

/// The trust-store paths this host has, for a capsule allowlisting `shell_allow` and reaching
/// `network_allow`: empty unless both are non-empty.
///
/// `capsule_writable` are the trees the capsule's commands can write
/// ([`crate::interpreter_runtime::capsule_writable_roots`]); nothing under one is derived. Sorted
/// by path and deduplicated, so every report of the set agrees.
///
/// Reads the filesystem only, runs nothing, and fails nothing: a location that is missing,
/// dangling or unreadable contributes nothing.
///
/// Off Linux this returns nothing: commands' file reads are not path-scoped there, so there is no
/// rule to feed.
pub fn derive_trust_store(
    shell_allow: &[String],
    network_allow: &[String],
    capsule_writable: &[&Path],
) -> Vec<TrustStorePath> {
    #[cfg(target_os = "linux")]
    {
        derive_in(
            TRUST_STORE_CANDIDATES,
            shell_allow,
            network_allow,
            capsule_writable,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (shell_allow, network_allow, capsule_writable);
        Vec::new()
    }
}

/// [`derive_trust_store`] with the candidate table injected.
///
///   - A candidate is kept when it canonicalizes to its own kind: a directory for a directory
///     candidate, a regular file for a file candidate.
///   - Each symlink directly inside a kept directory that resolves to a regular file outside every
///     kept directory makes that file's parent a listable grant, unless the parent is `/`, an
///     ancestor of (or equal to) a kept path, or under a capsule-writable root. This is where
///     Debian's `/etc/ssl/certs/*.pem` reach `/usr/share/ca-certificates/mozilla`, and why a link
///     to `/etc/ssl/openssl.cnf` cannot widen the grant to `/etc/ssl` and its `private/`.
///   - A kept file inside a granted directory is dropped as redundant.
///   - Nothing that is `/` or under a capsule-writable root is returned.
#[cfg(target_os = "linux")]
pub(crate) fn derive_in(
    candidates: &[TrustStoreCandidate<'_>],
    shell_allow: &[String],
    network_allow: &[String],
    capsule_writable: &[&Path],
) -> Vec<TrustStorePath> {
    if shell_allow.is_empty() || network_allow.is_empty() {
        return Vec::new();
    }
    let roots: Vec<PathBuf> = capsule_writable
        .iter()
        .map(|root| crate::protected_paths::canonicalize_existing_prefix(root))
        .collect();
    let grantable =
        |path: &Path| path != Path::new("/") && !crate::reachability::under_any(path, &roots);

    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let Ok(canonical) = std::fs::canonicalize(candidate.path) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            continue;
        };
        if !grantable(&canonical) {
            continue;
        }
        if candidate.directory && metadata.is_dir() {
            dirs.push(canonical);
        } else if !candidate.directory && metadata.is_file() {
            files.push(canonical);
        }
    }

    let inside_kept_dir = |path: &Path| dirs.iter().any(|dir| path.starts_with(dir));
    let encloses_kept_path = |parent: &Path| {
        dirs.iter()
            .chain(files.iter())
            .any(|kept| kept.starts_with(parent))
    };
    let mut linked_dirs: Vec<PathBuf> = Vec::new();
    for dir in &dirs {
        let Ok(listing) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                continue;
            }
            let Ok(target) = std::fs::canonicalize(&path) else {
                continue;
            };
            if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) || inside_kept_dir(&target) {
                continue;
            }
            let Some(parent) = target.parent() else {
                continue;
            };
            if grantable(parent) && !encloses_kept_path(parent) {
                linked_dirs.push(parent.to_path_buf());
            }
        }
    }

    let mut derived: Vec<TrustStorePath> = dirs
        .iter()
        .chain(linked_dirs.iter())
        .map(|path| TrustStorePath {
            path: path.clone(),
            directory: true,
        })
        .collect();
    derived.extend(
        files
            .into_iter()
            .filter(|file| {
                !dirs
                    .iter()
                    .chain(linked_dirs.iter())
                    .any(|dir| file.starts_with(dir))
            })
            .map(|path| TrustStorePath {
                path,
                directory: false,
            }),
    );
    derived.sort_by(|a, b| a.path.cmp(&b.path));
    derived.dedup_by(|a, b| a.path == b.path);
    derived
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        (temp, root)
    }

    fn allow() -> Vec<String> {
        vec!["curl".to_string()]
    }

    fn network() -> Vec<String> {
        vec!["example.com".to_string()]
    }

    /// Each path as a `(path, directory)` candidate, borrowing `paths`.
    fn candidates(paths: &[(String, bool)]) -> Vec<TrustStoreCandidate<'_>> {
        paths
            .iter()
            .map(|(path, directory)| TrustStoreCandidate {
                path,
                directory: *directory,
            })
            .collect()
    }

    fn spec(path: &Path, directory: bool) -> (String, bool) {
        (path.to_str().unwrap().to_string(), directory)
    }

    fn dir(path: &Path) -> TrustStorePath {
        TrustStorePath {
            path: path.to_path_buf(),
            directory: true,
        }
    }

    fn file(path: &Path) -> TrustStorePath {
        TrustStorePath {
            path: path.to_path_buf(),
            directory: false,
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A Debian-shaped store under `root`: `etc/ssl/certs` holding a bundle, a `.pem` link into
    /// `share/mozilla` and a hashed link to that `.pem`, beside `etc/ssl/openssl.cnf` and
    /// `etc/ssl/private/`. Returns `etc/ssl/certs`.
    fn debian_layout(root: &Path) -> PathBuf {
        let certs = root.join("etc/ssl/certs");
        std::fs::create_dir_all(&certs).unwrap();
        std::fs::create_dir_all(root.join("etc/ssl/private")).unwrap();
        write(&root.join("etc/ssl/openssl.cnf"), "[openssl_init]\n");
        write(
            &root.join("share/mozilla/X.crt"),
            "-----BEGIN CERTIFICATE-----\n",
        );
        write(
            &certs.join("ca-certificates.crt"),
            "-----BEGIN CERTIFICATE-----\n",
        );
        symlink("../../../share/mozilla/X.crt", certs.join("X.pem")).unwrap();
        symlink("X.pem", certs.join("abcd1234.0")).unwrap();
        certs
    }

    #[test]
    fn nothing_is_derived_without_both_a_shell_and_a_network_allowlist() {
        let (_temp, root) = canonical_tempdir();
        let certs = debian_layout(&root);
        let table = [spec(&certs, true)];
        let table = candidates(&table);
        assert!(derive_in(&table, &[], &network(), &[]).is_empty());
        assert!(derive_in(&table, &allow(), &[], &[]).is_empty());
        assert!(derive_in(&table, &[], &[], &[]).is_empty());
        assert!(!derive_in(&table, &allow(), &network(), &[]).is_empty());
        // The production table under the same gate.
        assert!(derive_trust_store(&[], &network(), &[]).is_empty());
        assert!(derive_trust_store(&allow(), &[], &[]).is_empty());
    }

    #[test]
    fn missing_and_wrong_kind_candidates_contribute_nothing() {
        let (_temp, root) = canonical_tempdir();
        let a_dir = root.join("a-dir");
        let a_file = root.join("a-file.pem");
        std::fs::create_dir_all(&a_dir).unwrap();
        write(&a_file, "x");
        symlink(root.join("nowhere"), root.join("dangling")).unwrap();
        let table = [
            spec(&root.join("missing"), true),
            spec(&root.join("missing.pem"), false),
            spec(&root.join("dangling"), false),
            spec(&a_dir, false),
            spec(&a_file, true),
        ];
        assert!(derive_in(&candidates(&table), &allow(), &network(), &[]).is_empty());
    }

    #[test]
    fn a_candidate_directory_is_granted_listable_at_its_canonical_path() {
        let (_temp, root) = canonical_tempdir();
        let real = root.join("etc/pki/tls/certs");
        std::fs::create_dir_all(&real).unwrap();
        let alias = root.join("usr-lib-ssl-certs");
        symlink(&real, &alias).unwrap();
        let table = [spec(&alias, true), spec(&real, true)];
        assert_eq!(
            derive_in(&candidates(&table), &allow(), &network(), &[]),
            vec![dir(&real)]
        );
    }

    #[test]
    fn certificate_links_grant_the_directory_they_point_into() {
        let (_temp, root) = canonical_tempdir();
        let certs = debian_layout(&root);
        let table = [spec(&certs, true)];
        assert_eq!(
            derive_in(&candidates(&table), &allow(), &network(), &[]),
            vec![dir(&certs), dir(&root.join("share/mozilla"))]
        );
    }

    #[test]
    fn a_link_to_a_sibling_of_the_store_does_not_widen_the_grant_to_its_parent() {
        let (_temp, root) = canonical_tempdir();
        let certs = debian_layout(&root);
        symlink("../openssl.cnf", certs.join("evil")).unwrap();
        symlink("../private", certs.join("keys")).unwrap();
        let bundle = root.join("etc/ssl/cert.pem");
        write(&bundle, "-----BEGIN CERTIFICATE-----\n");
        symlink("../cert.pem", certs.join("bundle-link")).unwrap();
        let table = [spec(&bundle, false), spec(&certs, true)];
        let derived = derive_in(&candidates(&table), &allow(), &network(), &[]);
        assert_eq!(
            derived,
            vec![file(&bundle), dir(&certs), dir(&root.join("share/mozilla"))]
        );
        for path in derived.iter().map(|entry| &entry.path) {
            assert_ne!(path, &root.join("etc/ssl"));
            assert!(!path.starts_with(root.join("etc/ssl/private")));
            assert!(!certs.starts_with(path) || path == &certs, "{path:?}");
        }
    }

    #[test]
    fn links_into_writable_roots_dangling_links_and_directory_links_are_dropped() {
        let (_temp, root) = canonical_tempdir();
        let certs = root.join("etc/ssl/certs");
        std::fs::create_dir_all(&certs).unwrap();
        let work = root.join("work");
        write(&work.join("planted.crt"), "x");
        std::fs::create_dir_all(root.join("share/subdir")).unwrap();
        symlink(work.join("planted.crt"), certs.join("planted.pem")).unwrap();
        symlink(root.join("gone.crt"), certs.join("dangling.pem")).unwrap();
        symlink(root.join("share/subdir"), certs.join("subdir")).unwrap();
        let fifo = root.join("share/fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        symlink(&fifo, certs.join("fifo.pem")).unwrap();
        let table = [spec(&certs, true)];
        assert_eq!(
            derive_in(&candidates(&table), &allow(), &network(), &[&work]),
            vec![dir(&certs)]
        );
    }

    #[test]
    fn a_candidate_under_a_writable_root_is_dropped() {
        let (_temp, root) = canonical_tempdir();
        let work = root.join("work");
        let certs = work.join("certs");
        std::fs::create_dir_all(&certs).unwrap();
        write(&work.join("bundle.pem"), "x");
        let table = [spec(&certs, true), spec(&work.join("bundle.pem"), false)];
        assert!(derive_in(&candidates(&table), &allow(), &network(), &[&work]).is_empty());
    }

    #[test]
    fn a_file_candidate_inside_a_granted_directory_is_listed_once() {
        let (_temp, root) = canonical_tempdir();
        let certs = debian_layout(&root);
        let inside = certs.join("ca-certificates.crt");
        let alias = root.join("usr-lib-ssl-cert.pem");
        symlink(&inside, &alias).unwrap();
        let outside = root.join("etc/pki/tls/cacert.pem");
        write(&outside, "x");
        let table = [
            spec(&inside, false),
            spec(&alias, false),
            spec(&outside, false),
            spec(&certs, true),
        ];
        assert_eq!(
            derive_in(&candidates(&table), &allow(), &network(), &[]),
            vec![
                file(&outside),
                dir(&certs),
                dir(&root.join("share/mozilla"))
            ]
        );
    }

    #[test]
    fn the_result_is_sorted_and_deduplicated() {
        let (_temp, root) = canonical_tempdir();
        let b = root.join("b/certs");
        let a = root.join("a/certs");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        write(&root.join("store/1.crt"), "x");
        write(&root.join("store/2.crt"), "x");
        symlink(root.join("store/1.crt"), a.join("1.pem")).unwrap();
        symlink(root.join("store/2.crt"), b.join("2.pem")).unwrap();
        let table = [spec(&b, true), spec(&a, true), spec(&b, true)];
        assert_eq!(
            derive_in(&candidates(&table), &allow(), &network(), &[]),
            vec![dir(&a), dir(&b), dir(&root.join("store"))]
        );
    }

    #[test]
    fn the_candidate_table_is_absolute_and_names_no_key_or_config_directory() {
        assert_eq!(TRUST_STORE_CANDIDATES.len(), 12);
        for candidate in TRUST_STORE_CANDIDATES {
            let path = Path::new(candidate.path);
            assert!(path.is_absolute(), "{path:?}");
            assert!(
                !path.components().any(|c| c.as_os_str() == "private"),
                "{path:?}"
            );
            for broad in [
                "/",
                "/etc",
                "/etc/ssl",
                "/etc/pki",
                "/etc/pki/tls",
                "/usr/lib/ssl",
            ] {
                assert_ne!(path, Path::new(broad));
            }
        }
    }
}
