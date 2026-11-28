# Building mur from source

Most people install the prebuilt binaries: [Install mur](https://docs.murmur.nexus/getting-started/install/).
Build from source on a distribution older than the prebuilt binaries support, or on a platform with no
published binary.

## What every source build needs

A stable Rust toolchain, installed with [rustup](https://rustup.rs).

## Building from source on Linux: system packages

These packages are needed only to build from source on Linux; the prebuilt binaries do not need them.

| Distribution | Command |
|---|---|
| Debian, Ubuntu | `sudo apt-get install build-essential libseccomp-dev` |
| Fedora, RHEL, Rocky Linux, AlmaLinux | `sudo dnf install gcc libseccomp-devel` |
| openSUSE | `sudo zypper install gcc libseccomp-devel` |
| Arch Linux | `sudo pacman -S --needed base-devel libseccomp` |
| Alpine | `sudo apk add build-base libseccomp-dev` |

The C compiler builds the C parts of some dependencies and is the linker; the libseccomp development
package provides `libseccomp.so`, which `mur` and `mur-roost` link against.

## Build

| Source | Command | Binaries land in |
|---|---|---|
| crates.io | `cargo install murmur-cli` and `cargo install mur-roost` | `~/.cargo/bin` |
| A checkout of this repository | `cargo build --release -p murmur-cli -p mur-roost` | `target/release/` |

On Ubuntu 23.10 and later, each of these install paths needs an AppArmor profile:
[Other install locations](https://docs.murmur.nexus/getting-started/install/#other-install-locations)
names it and gives the steps.

## When a Linux build stops with a libseccomp message

Early in a Linux build, before `mur` itself compiles, the build checks that the C linker can find
`libseccomp.so`; when it cannot, the build stops with the package to install for each distribution.
Install the package from the distribution's row above and build again. When `libseccomp.so` is
installed in a directory the linker does not search by default, set `LIBSECCOMP_LIB_PATH` to that
directory.
