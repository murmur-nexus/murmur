//! Stops a Linux source build that would fail to link libseccomp before anything compiles, with
//! the package to install, instead of a `cannot find -lseccomp` link error at the end.
//!
//! The probe asks `cc`, rustc's default linker, for `libseccomp.so` and `libseccomp.a`. It steps
//! aside whenever the answer could differ from the real link: see `libseccomp::skip_reason`.

// Cargo reads a build script's stdout as directives and shows its stderr when it fails; the
// crate's print lints guard a capsule's stdout, which a build script never writes.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::Command;

#[path = "build/libseccomp.rs"]
mod libseccomp;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/libseccomp.rs");
    println!("cargo:rerun-if-env-changed=LIBSECCOMP_LIB_PATH");
    println!("cargo:rerun-if-env-changed=LIBRARY_PATH");
    println!("cargo:rerun-if-env-changed=RUSTC_LINKER");

    let env = libseccomp::BuildEnv {
        target_os: var("CARGO_CFG_TARGET_OS").unwrap_or_default(),
        host: var("HOST").unwrap_or_default(),
        target: var("TARGET").unwrap_or_default(),
        rustc_linker: var("RUSTC_LINKER"),
        encoded_rustflags: var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default(),
        libseccomp_lib_path: var("LIBSECCOMP_LIB_PATH"),
    };
    if libseccomp::skip_reason(&env).is_some() {
        return;
    }

    let mut compiler_answers = Vec::new();
    for file in libseccomp::LIBRARY_FILES {
        match print_file_name(file) {
            Some(stdout) => compiler_answers.extend(libseccomp::compiler_candidate(&stdout)),
            None => return,
        }
    }
    let dirs = libseccomp::extra_dirs(
        var("LIBRARY_PATH").as_deref(),
        &var("CARGO_CFG_TARGET_ARCH").unwrap_or_default(),
        &var("CARGO_CFG_TARGET_ENV").unwrap_or_default(),
    );
    if !libseccomp::library_found(&compiler_answers, &dirs, |path| path.is_file()) {
        eprintln!("{}", libseccomp::missing_message());
        std::process::exit(1);
    }
}

/// An environment variable's value; `None` when unset or not valid UTF-8.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// What `cc -print-file-name=<file>` printed. `None` when `cc` cannot be run or fails: the probe
/// then has no answer to judge and the build goes on, so the link reports whatever is wrong.
fn print_file_name(file: &str) -> Option<String> {
    let output = Command::new("cc")
        .arg(format!("-print-file-name={file}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
