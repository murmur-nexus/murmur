//! Whether the C linker can find libseccomp, decided before `capsule-runtime` compiles, and the
//! refusal that names the package to install when it cannot. Shared by `build.rs`, which runs the
//! probe, and by the crate's unit tests.
//!
//! `libseccomp-sys` emits `cargo:rustc-link-lib=dylib=seccomp` without looking for the library,
//! so a host without the development package fails only at the final link of `mur`, minutes in,
//! as `cannot find -lseccomp` inside the linker's command line. The runtime package ships
//! `libseccomp.so.2` alone; the unversioned `libseccomp.so` the linker resolves `-lseccomp` to
//! comes from the development package.

use std::path::{Path, PathBuf};

/// The build-script environment the skip decision reads, as cargo sets it.
pub struct BuildEnv {
    /// `CARGO_CFG_TARGET_OS`.
    pub target_os: String,
    /// `HOST`.
    pub host: String,
    /// `TARGET`.
    pub target: String,
    /// `RUSTC_LINKER`: set only when the operator configured a linker for the target.
    pub rustc_linker: Option<String>,
    /// `CARGO_ENCODED_RUSTFLAGS`: the rustflags joined by `\x1f`.
    pub encoded_rustflags: String,
    /// `LIBSECCOMP_LIB_PATH`: the directory `libseccomp-sys` adds to the link search.
    pub libseccomp_lib_path: Option<String>,
}

/// Why the probe does not run. Each is a build whose link the probe cannot predict by asking the
/// host's `cc`.
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The target is not Linux, so nothing links libseccomp.
    NotLinux,
    /// A cross build links with the target's toolchain, not the host's `cc`.
    CrossTarget,
    /// The operator chose the linker.
    LinkerConfigured,
    /// A `-L` rustflag adds link search directories `cc` knows nothing about.
    LinkSearchFlag,
    /// `LIBSECCOMP_LIB_PATH` names the library's directory.
    LibPathSet,
}

/// Why the probe should not run for `env`, or `None` when it should.
pub fn skip_reason(env: &BuildEnv) -> Option<SkipReason> {
    if env.target_os != "linux" {
        return Some(SkipReason::NotLinux);
    }
    if env.host != env.target {
        return Some(SkipReason::CrossTarget);
    }
    if env.rustc_linker.is_some() {
        return Some(SkipReason::LinkerConfigured);
    }
    if env
        .encoded_rustflags
        .split('\x1f')
        .any(|flag| flag.starts_with("-L"))
    {
        return Some(SkipReason::LinkSearchFlag);
    }
    if env.libseccomp_lib_path.is_some() {
        return Some(SkipReason::LibPathSet);
    }
    None
}

/// The path `cc -print-file-name=<name>` printed, when it names one. The compiler prints the full
/// path of a file it finds in its library search and the bare name back when it finds none, so
/// only an absolute answer is a candidate; the caller still checks the file exists.
pub fn compiler_candidate(stdout: &str) -> Option<PathBuf> {
    let answer = Path::new(stdout.trim());
    answer.is_absolute().then(|| answer.to_path_buf())
}

/// The directories looked in besides the compiler's answer: each `LIBRARY_PATH` entry, then the
/// local-install and lib64 directories some distributions' linkers search but not every `cc`
/// reports. `target_arch` and `target_env` are `CARGO_CFG_TARGET_ARCH` and `CARGO_CFG_TARGET_ENV`;
/// the multiarch directory `/usr/local/lib/<arch>-linux-gnu` is listed for a gnu target only.
pub fn extra_dirs(library_path: Option<&str>, target_arch: &str, target_env: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = library_path
        .map(|value| {
            std::env::split_paths(value)
                .filter(|dir| !dir.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    dirs.push(PathBuf::from("/usr/local/lib"));
    dirs.push(PathBuf::from("/usr/local/lib64"));
    if target_env == "gnu" {
        dirs.push(PathBuf::from(format!(
            "/usr/local/lib/{target_arch}-linux-gnu"
        )));
    }
    dirs.push(PathBuf::from("/usr/lib64"));
    dirs.push(PathBuf::from("/lib64"));
    dirs
}

/// The library files that satisfy the link: the shared library `-lseccomp` resolves to by default,
/// and the archive `LIBSECCOMP_LINK_TYPE=static` asks for.
pub const LIBRARY_FILES: [&str; 2] = ["libseccomp.so", "libseccomp.a"];

/// Whether the linker will find libseccomp: a compiler answer in `compiler_answers` is an existing
/// file, or one of [`LIBRARY_FILES`] exists in one of `dirs`. `exists` is the filesystem check.
pub fn library_found(
    compiler_answers: &[PathBuf],
    dirs: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> bool {
    compiler_answers.iter().any(|path| exists(path))
        || dirs
            .iter()
            .flat_map(|dir| LIBRARY_FILES.iter().map(move |file| dir.join(file)))
            .any(|path| exists(&path))
}

/// The refusal printed when the linker would not find libseccomp. One install command per
/// distribution: the rows match the table in `BUILD.md`, minus the C toolchain, which a build
/// that reached this script already has.
pub fn missing_message() -> String {
    "\
error: building mur from source needs the libseccomp development files, because mur links libseccomp.
The C linker cannot find libseccomp.so. Install the package for your distribution, then build again:

  Debian, Ubuntu                          sudo apt-get install libseccomp-dev
  Fedora, RHEL, Rocky Linux, AlmaLinux    sudo dnf install libseccomp-devel
  openSUSE                                sudo zypper install libseccomp-devel
  Arch Linux                              sudo pacman -S libseccomp
  Alpine                                  sudo apk add libseccomp-dev

If libseccomp.so is installed in a directory the linker does not search by default, set
LIBSECCOMP_LIB_PATH to that directory.

Building from source: https://github.com/murmur-nexus/murmur/blob/main/BUILD.md
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A native Linux build with nothing directing the linker: the probe runs.
    fn native_linux() -> BuildEnv {
        BuildEnv {
            target_os: "linux".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
            target: "x86_64-unknown-linux-gnu".to_string(),
            rustc_linker: None,
            encoded_rustflags: String::new(),
            libseccomp_lib_path: None,
        }
    }

    #[test]
    fn native_linux_build_with_nothing_set_probes() {
        assert_eq!(skip_reason(&native_linux()), None);
    }

    #[test]
    fn rustflags_without_link_search_still_probe() {
        let env = BuildEnv {
            encoded_rustflags: "-C\x1ftarget-cpu=native\x1f--cfg\x1ffoo".to_string(),
            ..native_linux()
        };
        assert_eq!(skip_reason(&env), None);
    }

    #[test]
    fn non_linux_target_skips() {
        for os in ["macos", "windows", "freebsd"] {
            let env = BuildEnv {
                target_os: os.to_string(),
                ..native_linux()
            };
            assert_eq!(skip_reason(&env), Some(SkipReason::NotLinux), "{os}");
        }
    }

    #[test]
    fn cross_target_skips() {
        let env = BuildEnv {
            target: "aarch64-unknown-linux-gnu".to_string(),
            ..native_linux()
        };
        assert_eq!(skip_reason(&env), Some(SkipReason::CrossTarget));
    }

    #[test]
    fn configured_linker_skips() {
        let env = BuildEnv {
            rustc_linker: Some("/usr/bin/clang".to_string()),
            ..native_linux()
        };
        assert_eq!(skip_reason(&env), Some(SkipReason::LinkerConfigured));
    }

    #[test]
    fn separate_link_search_flag_skips() {
        let env = BuildEnv {
            encoded_rustflags: "-C\x1fopt-level=2\x1f-L\x1fnative=/opt/seccomp/lib".to_string(),
            ..native_linux()
        };
        assert_eq!(skip_reason(&env), Some(SkipReason::LinkSearchFlag));
    }

    #[test]
    fn joined_link_search_flag_skips() {
        for flag in ["-Lnative=/usr/lib/x86_64-linux-gnu", "-L/opt/seccomp/lib"] {
            let env = BuildEnv {
                encoded_rustflags: flag.to_string(),
                ..native_linux()
            };
            assert_eq!(
                skip_reason(&env),
                Some(SkipReason::LinkSearchFlag),
                "{flag}"
            );
        }
    }

    #[test]
    fn libseccomp_lib_path_skips() {
        let env = BuildEnv {
            libseccomp_lib_path: Some("/opt/seccomp/lib".to_string()),
            ..native_linux()
        };
        assert_eq!(skip_reason(&env), Some(SkipReason::LibPathSet));
    }

    #[test]
    fn absolute_compiler_answer_is_a_candidate() {
        assert_eq!(
            compiler_candidate(
                "/usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/libseccomp.so\n"
            ),
            Some(PathBuf::from(
                "/usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/libseccomp.so"
            ))
        );
    }

    #[test]
    fn bare_name_answer_is_not_found() {
        assert_eq!(compiler_candidate("libseccomp.so\n"), None);
    }

    #[test]
    fn empty_answer_is_not_found() {
        assert_eq!(compiler_candidate(""), None);
        assert_eq!(compiler_candidate("  \n"), None);
    }

    #[test]
    fn relative_answer_is_not_found() {
        assert_eq!(compiler_candidate("lib/libseccomp.so"), None);
    }

    #[test]
    fn extra_dirs_put_library_path_first_and_name_multiarch_for_gnu() {
        assert_eq!(
            extra_dirs(Some("/opt/a/lib::/opt/b/lib"), "x86_64", "gnu"),
            [
                "/opt/a/lib",
                "/opt/b/lib",
                "/usr/local/lib",
                "/usr/local/lib64",
                "/usr/local/lib/x86_64-linux-gnu",
                "/usr/lib64",
                "/lib64",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn extra_dirs_leave_out_multiarch_for_non_gnu() {
        assert_eq!(
            extra_dirs(None, "x86_64", "musl"),
            ["/usr/local/lib", "/usr/local/lib64", "/usr/lib64", "/lib64"].map(PathBuf::from)
        );
    }

    #[test]
    fn existing_compiler_answer_is_found() {
        let answer = PathBuf::from("/usr/lib/x86_64-linux-gnu/libseccomp.so");
        let found = library_found(std::slice::from_ref(&answer), &[], |path| path == answer);
        assert!(found);
    }

    #[test]
    fn missing_compiler_answer_is_not_found() {
        let answer = PathBuf::from("/usr/lib/x86_64-linux-gnu/libseccomp.so");
        assert!(!library_found(&[answer], &[], |_| false));
    }

    #[test]
    fn library_in_an_extra_dir_is_found() {
        let dirs = extra_dirs(None, "x86_64", "gnu");
        for file in LIBRARY_FILES {
            let wanted = Path::new("/usr/local/lib64").join(file);
            assert!(library_found(&[], &dirs, |path| path == wanted), "{file}");
        }
    }

    #[test]
    fn message_names_each_package_the_override_and_build_md() {
        let message = missing_message();
        for wanted in [
            "libseccomp development files",
            "cannot find libseccomp.so",
            "sudo apt-get install libseccomp-dev",
            "sudo dnf install libseccomp-devel",
            "sudo zypper install libseccomp-devel",
            "sudo pacman -S libseccomp",
            "sudo apk add libseccomp-dev",
            "LIBSECCOMP_LIB_PATH",
            "https://github.com/murmur-nexus/murmur/blob/main/BUILD.md",
        ] {
            assert!(
                message.contains(wanted),
                "missing {wanted:?} in:\n{message}"
            );
        }
    }
}
