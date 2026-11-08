# Install mur

`mur` is one binary, with `mur-roost` beside it. On Linux hosts where AppArmor restricts
unprivileged user namespaces (Ubuntu 23.10 and later), `mur` also needs the `mur-sealed` AppArmor
profile. Without it, every capsule with a `capabilities.shell.allow` list refuses to launch with
[`E-CAP-005`](../reference/diagnostics.md#e-cap-005), and a `sealed` capsule refuses with
[`E-CAP-003`](../reference/diagnostics.md#e-cap-003). Hosts without AppArmor (macOS, Fedora, Arch)
need no profile.

??? definition "What is an AppArmor profile?"
    An **AppArmor profile** is a set of permissions the Linux kernel gives to one program, chosen
    by the path the program runs from. The `mur-sealed` profile lets `mur` create the unprivileged
    user namespaces its capsule sandbox is built from, and gives that permission to no other
    program.

---

## Install with the installer

The installer is the supported install on macOS (Apple Silicon and Intel) and Linux x86_64:

```bash
curl -fsSL https://install.murmur.rs | sh
```

It downloads the release's binaries, checks them against the release's `checksums.txt`, runs each
one once, and installs them:

| What | Where |
|---|---|
| `mur` and `mur-roost` | `/usr/local/bin` when it is writable, otherwise `~/.local/bin` (`MUR_INSTALL_DIR` overrides both) |
| The `mur-sealed` profile | `/etc/apparmor.d/mur-sealed`, loaded, on an AppArmor host when the release carries the profile as an asset and the installer runs as root |

When the release does not carry the profile, the installer warns and loads nothing. Install the
profile with [Step 3 and Step 4](#step-3-install-the-profile) below, then confirm with
[Step 5](#step-5-confirm-with-mur-doctor).

---

## Paths the profile names

AppArmor gives a profile to a program by the path it runs from. The shipped profiles name these
paths, and no others:

| Path | Profile | Installed by |
|---|---|---|
| `/usr/local/bin/mur`, `/usr/bin/mur`, `/opt/mur/bin/mur` | `mur-sealed` | `/etc/apparmor.d/mur-sealed` |
| `~/.local/bin/mur`, `~/.cargo/bin/mur` (under `/home/<user>` or `/root`) | `mur-sealed-home` | `/etc/apparmor.d/mur-sealed` |
| `<checkout>/target/debug/mur`, `<checkout>/target/release/mur` | `mur-sealed-dev` | `sudo scripts/install-dev-apparmor.sh`, run from that checkout |

AppArmor matches the path the binary resolves to, after every symlink. A symlink such as
`/usr/local/bin/mur -> mur-<version>` resolves to `/usr/local/bin/mur-<version>`, which no profile
names, so `mur` runs without the profile. Install the binary as a real file at one of the paths above.

Widening the profile to name more paths is not the fix: a profile that names a pattern gives the
right to create user namespaces to every binary that matches it.

---

## Install by hand

These steps install `mur` as a real file at `/usr/local/bin/mur` and load the profile for it. Run
them on Linux; on macOS, Step 1 and Step 2 are the whole install.

### Step 1 — download the release files { #step-1-download-the-release-files }

Set `VERSION` to the release you want, without the leading `v`, and `PLATFORM` to your host's
platform (`linux-x86_64`, `darwin-aarch64` or `darwin-x86_64`). Download both binaries and the
release's checksums, and check them:

```bash
VERSION=0.10.0
PLATFORM=linux-x86_64
BASE=https://github.com/murmur-nexus/murmur/releases/download/v${VERSION}

curl -fsSLO ${BASE}/mur-${VERSION}-${PLATFORM}
curl -fsSLO ${BASE}/mur-roost-${VERSION}-${PLATFORM}
curl -fsSLO ${BASE}/checksums.txt
sha256sum --ignore-missing -c checksums.txt
```

Both binaries must report `OK`. On macOS, use `shasum -a 256 --ignore-missing -c checksums.txt`.

### Step 2 — install the binaries as real files { #step-2-install-the-binaries }

```bash
sudo install -m 755 mur-${VERSION}-${PLATFORM} /usr/local/bin/mur
sudo install -m 755 mur-roost-${VERSION}-${PLATFORM} /usr/local/bin/mur-roost
```

`install` copies the file, so `/usr/local/bin/mur` is the binary itself rather than a link to it.

### Step 3 — install the profile { #step-3-install-the-profile }

Fetch the profile from the same release tag, and install it:

```bash
curl -fsSL https://raw.githubusercontent.com/murmur-nexus/murmur/v${VERSION}/packaging/apparmor/mur-sealed -o mur-sealed
sudo install -m 644 mur-sealed /etc/apparmor.d/mur-sealed
```

### Step 4 — load the profile { #step-4-load-the-profile }

```bash
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
```

This loads both profiles in the file, `mur-sealed` and `mur-sealed-home`.

### Step 5 — confirm with `mur doctor` { #step-5-confirm-with-mur-doctor }

Run `mur doctor` in a project directory. Its `AppArmor / user namespaces` block reads:

```text
AppArmor / user namespaces
  this binary:  /usr/local/bin/mur
  profile attached: mur-sealed
  userns grant: profile_confining
  network namespace: created
  /etc/apparmor.d/mur-sealed: matches the profile this build ships
```

`mur doctor` prints more lines than these. If `profile attached:` reads `none`, `mur doctor`
names which case applies and the command that fixes it:

| Case | Command `mur doctor` names |
|---|---|
| No profile loaded | `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`, or the download and load line when the file is not installed |
| Binary path not profiled, started through a symlink | `mv -f <file> <link>`, with `sudo` outside your home directory |
| Binary path not profiled, no symlink | `sudo rm -f /usr/local/bin/mur && sudo install -m 755 <file> /usr/local/bin/mur` |
| Profile loaded and not attached | `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed` |

---

## Other install locations

| Install | Binary path | Profile |
|---|---|---|
| Per user, without root for the binary | `~/.local/bin/mur` (`install -m 755 mur-${VERSION}-${PLATFORM} ~/.local/bin/mur`) | `mur-sealed-home`. The profile still needs root: run Step 3 and Step 4 |
| `cargo install murmur-cli` | `~/.cargo/bin/mur` | `mur-sealed-home`. Run Step 3 and Step 4 with `VERSION` set to the installed crate's version |
| A checkout build | `<checkout>/target/debug/mur` or `<checkout>/target/release/mur` | `mur-sealed-dev`: run `sudo scripts/install-dev-apparmor.sh` from the checkout. A build whose target directory is not `<checkout>/target` gets no profile |

On a host without AppArmor, install the binaries and skip the profile steps: `mur doctor` reports
`userns grant: apparmor_absent`.

## Summary

| Step | How it works |
|---|---|
| Installer | The script at `https://install.murmur.rs` installs `mur`, `mur-roost` and, when the release carries it, the profile |
| Binary location | A real file at a path the profile names; a symlink resolves to a path it does not name |
| Profile | `/etc/apparmor.d/mur-sealed` from the release tag, loaded with `sudo apparmor_parser -r` |
| Checkout builds | `sudo scripts/install-dev-apparmor.sh` loads `mur-sealed-dev` for `<checkout>/target` |
| Confirmation | `mur doctor` reads `profile attached: mur-sealed` and `userns grant: profile_confining` |
