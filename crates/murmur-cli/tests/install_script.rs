//! What `scripts/install.sh` puts on disk, driven against a release directory on this machine.
//!
//! The installer is the only path that puts `mur` and `mur-roost` on an operator's `PATH`, and the
//! two must arrive together, from one release tag, or not at all. Everything here runs offline: a
//! `curl` shim earlier on `PATH` than the real one serves the fixture release out of a temporary
//! directory and logs every URL the script asks for, which is also how "both binaries came from the
//! same release" is asserted rather than assumed.
//!
//! The fixture assets are real programs. The installer runs each staged binary once before it is
//! renamed onto PATH and refuses the whole install if either cannot exec, so an asset of arbitrary
//! bytes no longer reaches the happy path. The payloads are shell scripts rather than compiled
//! binaries: what the check asks is whether this host's kernel and loader accept the file and it
//! exits 0, and a script answers that question the same way a linked ELF does — while letting a
//! test hand the installer a file the loader refuses, which is the case that shipped.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use murmur_artifact::{current_platform, sha256_hex};
use tempfile::TempDir;

/// The version the fixture release is published under. Pinning it keeps
/// `resolve_latest_version()` — the one part of the script that needs GitHub — out of the run.
const VERSION: &str = "9.9.9";

/// A release that predates `mur-roost` being published at all.
const OLD_VERSION: &str = "9.9.8";

const REPO: &str = "murmur-nexus/murmur";

/// A `curl` that serves `FAKE_RELEASE_DIR` and appends every requested URL to `FAKE_CURL_LOG`.
///
/// It accepts the flags `scripts/install.sh` passes (`-fsSL --proto '=https' --tlsv1.2 -o <dest>`)
/// and exits 22, curl's HTTP-error status, for an asset the release directory does not hold.
const FAKE_CURL: &str = r#"#!/bin/sh
url=""
dest=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) dest="$2"; shift 2 ;;
        --proto) shift 2 ;;
        -*) shift ;;
        *) url="$1"; shift ;;
    esac
done
printf '%s\n' "$url" >> "$FAKE_CURL_LOG"
asset="${url##*/}"
[ -f "$FAKE_RELEASE_DIR/$asset" ] || exit 22
cp "$FAKE_RELEASE_DIR/$asset" "$dest"
"#;

/// The AppArmor leg of the installer is not under test here, and on a host where this stub is
/// rejected it warns and returns — which keeps the run from writing to `/etc/apparmor.d` when the
/// suite happens to be running as root.
const FAKE_APPARMOR_PARSER: &str = "#!/bin/sh\nexit 1\n";

/// A release asset that runs: it answers `--version` and exits 0, which is all the installer's
/// exec check asks of a staged binary.
fn runnable_asset(name: &str, version: &str) -> Vec<u8> {
    format!("#!/bin/sh\nprintf '%s\\n' \"{name} {version}\"\n").into_bytes()
}

/// ELF magic and then nothing that is a program. The kernel refuses the exec with `ENOEXEC` and
/// the shell reports `Exec format error` — a truncated download that somehow checksummed, or an
/// asset for the wrong architecture, reaches the installer looking exactly like this.
const NOT_A_PROGRAM: &[u8] = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00not a program\x00";

/// A binary this host's loader refuses, standing in for one built above the glibc floor: it writes
/// the line the dynamic loader writes and exits 127, exactly as `mur` built against glibc 2.39
/// does on Debian 11.
const REFUSED_BY_LOADER: &str = r#"#!/bin/sh
echo "$0: /lib/x86_64-linux-gnu/libc.so.6: version \`GLIBC_2.39' not found" >&2
exit 127
"#;

/// The loader's own words, which the installer must pass through unedited: an operator who can see
/// this line knows which library and which version, and no message the installer could write for
/// them would say it.
const LOADER_LINE: &str = "/lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found";

/// The checkout this test file was compiled from, which is where `scripts/install.sh` lives.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap()
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// One installer run: a release directory, the `curl` that serves it, the install directory it
/// writes into, and the log of every URL it asked for.
struct Installer {
    release: TempDir,
    bin: TempDir,
    install_dir: TempDir,
    home: TempDir,
    log: PathBuf,
    platform: &'static str,
}

impl Installer {
    /// A release carrying `mur` and, when `with_roost`, `mur-roost` — with `checksums.txt` listing
    /// exactly the assets the directory holds.
    fn new(version: &str, with_roost: bool) -> Self {
        let release = TempDir::new().unwrap();
        let bin = TempDir::new().unwrap();
        let platform = current_platform();

        let mut assets = vec![(
            format!("mur-{version}-{platform}"),
            runnable_asset("mur", version),
        )];
        if with_roost {
            assets.push((
                format!("mur-roost-{version}-{platform}"),
                runnable_asset("mur-roost", version),
            ));
        }
        let mut checksums = String::new();
        for (name, bytes) in &assets {
            fs::write(release.path().join(name), bytes).unwrap();
            checksums.push_str(&format!("{}  {name}\n", sha256_hex(bytes)));
        }
        fs::write(release.path().join("checksums.txt"), checksums).unwrap();

        write_executable(&bin.path().join("curl"), FAKE_CURL);
        write_executable(&bin.path().join("apparmor_parser"), FAKE_APPARMOR_PARSER);

        let home = TempDir::new().unwrap();
        Self {
            log: home.path().join("urls.log"),
            release,
            bin,
            install_dir: TempDir::new().unwrap(),
            home,
            platform,
        }
    }

    fn asset(&self, name: &str) -> PathBuf {
        self.release.path().join(name)
    }

    fn installed(&self, name: &str) -> PathBuf {
        self.install_dir.path().join(name)
    }

    /// Replaces a release asset's payload *and* its `checksums.txt` line, so the download and
    /// checksum legs still pass and what the payload does when executed is the only thing the run
    /// turns on.
    fn replace_asset(&self, name: &str, bytes: &[u8]) {
        fs::write(self.asset(name), bytes).unwrap();
        let sums = self.release.path().join("checksums.txt");
        let updated: String = fs::read_to_string(&sums)
            .unwrap()
            .lines()
            .map(|line| {
                if line.split_whitespace().nth(1) == Some(name) {
                    format!("{}  {name}\n", sha256_hex(bytes))
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        fs::write(sums, updated).unwrap();
    }

    fn run(&self, version: &str) -> Output {
        let inherited = std::env::var("PATH").unwrap_or_default();
        Command::new("sh")
            .arg(repo_root().join("scripts").join("install.sh"))
            .env("PATH", format!("{}:{inherited}", self.bin.path().display()))
            .env("HOME", self.home.path())
            .env("MUR_VERSION", version)
            .env("MUR_INSTALL_DIR", self.install_dir.path())
            .env("MUR_REPO", REPO)
            .env("FAKE_RELEASE_DIR", self.release.path())
            .env("FAKE_CURL_LOG", &self.log)
            .output()
            .unwrap()
    }

    fn requested_urls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Everything left in the install directory, staging files included.
    fn install_dir_entries(&self) -> Vec<String> {
        let mut entries: Vec<String> = fs::read_dir(self.install_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }
}

/// `linux-aarch64` has a platform tag and no published binary, and the installer dies on it by
/// name; `unknown` is a host the release publishes nothing for. Neither installs anything, here or
/// on a real machine.
fn skip_unpublished_platform(test_name: &str) -> bool {
    let platform = current_platform();
    if matches!(platform, "linux-aarch64" | "unknown") {
        eprintln!("[SKIP] {test_name}: no release binary is published for {platform}");
        return true;
    }
    false
}

/// The documented install, on a release carrying both binaries: `mur` and `mur-roost` land beside
/// each other, executable, byte-identical to the assets, and every request goes to one release tag.
#[test]
fn the_installer_puts_both_binaries_in_the_install_directory() {
    if skip_unpublished_platform("the_installer_puts_both_binaries_in_the_install_directory") {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let output = installer.run(VERSION);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "{stdout}\n{stderr}");

    let platform = installer.platform;
    for (asset, installed) in [
        (format!("mur-{VERSION}-{platform}"), "mur"),
        (format!("mur-roost-{VERSION}-{platform}"), "mur-roost"),
    ] {
        let target = installer.installed(installed);
        assert_eq!(
            fs::read(&target).unwrap(),
            fs::read(installer.asset(&asset)).unwrap(),
            "{installed} must be the release asset's bytes"
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&target), 0o755, "{installed} must be executable");
    }

    assert!(
        stdout.contains("installed mur-roost"),
        "the installer must report the daemon it installed, got:\n{stdout}"
    );
    for asset in [
        format!("mur-{VERSION}-{platform}"),
        format!("mur-roost-{VERSION}-{platform}"),
    ] {
        assert!(
            stdout.contains(&format!("{asset} runs on this host")),
            "the installer must report that it ran {asset}, got:\n{stdout}"
        );
    }

    // One release tag, one checksums.txt, both binaries under it.
    let base = format!("https://github.com/{REPO}/releases/download/v{VERSION}/");
    let urls = installer.requested_urls();
    for url in &urls {
        assert!(url.starts_with(&base), "{url} is not under {base}");
    }
    for asset in [
        format!("mur-{VERSION}-{platform}"),
        format!("mur-roost-{VERSION}-{platform}"),
        "checksums.txt".to_string(),
    ] {
        assert!(
            urls.contains(&format!("{base}{asset}")),
            "{asset} was never requested, got {urls:?}"
        );
    }
}

/// A `mur-roost` asset whose bytes do not match the release's `checksums.txt`: the install stops
/// before anything is moved into place, so neither binary is installed.
#[test]
fn a_corrupt_roost_asset_installs_neither_binary() {
    if skip_unpublished_platform("a_corrupt_roost_asset_installs_neither_binary") {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let roost_asset = format!("mur-roost-{VERSION}-{}", installer.platform);
    fs::write(installer.asset(&roost_asset), b"tampered").unwrap();

    let output = installer.run(VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(&roost_asset), "{stderr}");
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert_eq!(
        installer.install_dir_entries(),
        Vec::<String>::new(),
        "a failed install must leave nothing behind, not even a staging file"
    );
}

/// The same when the release lists the asset and cannot deliver it.
#[test]
fn a_roost_asset_that_cannot_be_downloaded_installs_neither_binary() {
    if skip_unpublished_platform("a_roost_asset_that_cannot_be_downloaded_installs_neither_binary")
    {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let roost_asset = format!("mur-roost-{VERSION}-{}", installer.platform);
    fs::remove_file(installer.asset(&roost_asset)).unwrap();

    let output = installer.run(VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(&roost_asset), "{stderr}");
    assert_eq!(
        installer.install_dir_entries(),
        Vec::<String>::new(),
        "a failed install must leave nothing behind, not even a staging file"
    );
}

/// A release published before `mur-roost` was: `mur` installs, the daemon is reported missing with
/// the consequence stated, and the install still succeeds.
#[test]
fn a_release_without_a_roost_asset_still_installs_mur() {
    if skip_unpublished_platform("a_release_without_a_roost_asset_still_installs_mur") {
        return;
    }
    let installer = Installer::new(OLD_VERSION, false);
    let output = installer.run(OLD_VERSION);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(installer.installed("mur").exists(), "{stdout}");
    assert!(
        !installer.installed("mur-roost").exists(),
        "the release carries no daemon, so none is installed"
    );
    assert!(
        stderr.contains(&format!(
            "carries no mur-roost-{OLD_VERSION}-{}",
            installer.platform
        )),
        "{stderr}"
    );
    assert!(stderr.contains("capabilities.spawn.allow"), "{stderr}");
    assert!(stderr.contains("E-RUN-019"), "{stderr}");
}

/// An asset that is not a program: correctly listed, correctly checksummed, and refused by the
/// kernel. Nothing is installed and nothing is left staged.
#[test]
fn a_mur_asset_that_cannot_exec_installs_nothing() {
    if skip_unpublished_platform("a_mur_asset_that_cannot_exec_installs_nothing") {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let asset = format!("mur-{VERSION}-{}", installer.platform);
    installer.replace_asset(&asset, NOT_A_PROGRAM);

    let output = installer.run(VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(&asset), "the asset must be named: {stderr}");
    assert!(
        stderr.contains("Exec format error"),
        "the kernel's own refusal must reach the operator: {stderr}"
    );
    assert!(
        stderr.contains("glibc 2.31"),
        "the supported floor must be stated: {stderr}"
    );
    assert_eq!(
        installer.install_dir_entries(),
        Vec::<String>::new(),
        "a binary that cannot exec must leave nothing behind, not even a staging file"
    );
}

/// A `mur` the loader refuses, which is what a build above the glibc floor is on a target host.
/// The loader's line reaches the operator unedited, unwrapped and unsummarised.
#[test]
fn a_mur_asset_the_loader_refuses_installs_nothing_and_quotes_the_loader() {
    if skip_unpublished_platform(
        "a_mur_asset_the_loader_refuses_installs_nothing_and_quotes_the_loader",
    ) {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let asset = format!("mur-{VERSION}-{}", installer.platform);
    installer.replace_asset(&asset, REFUSED_BY_LOADER.as_bytes());

    let output = installer.run(VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(&asset), "the asset must be named: {stderr}");
    assert!(
        stderr.contains(LOADER_LINE),
        "the loader's own line must appear verbatim: {stderr}"
    );
    assert!(
        stderr.contains("glibc 2.31"),
        "the supported floor must be stated: {stderr}"
    );
    assert_eq!(
        installer.install_dir_entries(),
        Vec::<String>::new(),
        "a binary the loader refuses must leave nothing behind, not even a staging file"
    );
}

/// Refusing a bad install must never be worse than not having run the installer: a working `mur`
/// already on PATH is byte-identical afterwards and still answers `--version`.
#[test]
fn a_failed_exec_check_leaves_a_working_previous_install_untouched() {
    if skip_unpublished_platform("a_failed_exec_check_leaves_a_working_previous_install_untouched")
    {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let asset = format!("mur-{VERSION}-{}", installer.platform);
    installer.replace_asset(&asset, REFUSED_BY_LOADER.as_bytes());

    let previous = String::from_utf8(runnable_asset("mur", "0.0.1-previous")).unwrap();
    let target = installer.installed("mur");
    write_executable(&target, &previous);

    let output = installer.run(VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        previous,
        "the previous install must be byte-identical"
    );
    let ran = Command::new(&target).arg("--version").output().unwrap();
    assert!(ran.status.success(), "the previous install must still run");
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains("mur 0.0.1-previous"),
        "the previous install must still answer --version"
    );
    assert_eq!(
        installer.install_dir_entries(),
        vec!["mur".to_string()],
        "the refused install must leave no staging file beside the previous binary"
    );
}

/// A `mur-roost` that cannot exec refuses the whole install, the same all-or-nothing rule the
/// checksum leg holds to: a daemon that will not start fails a delegation launch with something far
/// less obvious than a loader error.
#[test]
fn a_roost_asset_that_cannot_exec_installs_neither_binary() {
    if skip_unpublished_platform("a_roost_asset_that_cannot_exec_installs_neither_binary") {
        return;
    }
    let installer = Installer::new(VERSION, true);
    let roost_asset = format!("mur-roost-{VERSION}-{}", installer.platform);
    installer.replace_asset(&roost_asset, REFUSED_BY_LOADER.as_bytes());

    let output = installer.run(VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains(&roost_asset),
        "the daemon asset must be named: {stderr}"
    );
    assert!(
        stderr.contains(LOADER_LINE),
        "the loader's own line must appear verbatim: {stderr}"
    );
    assert_eq!(
        installer.install_dir_entries(),
        Vec::<String>::new(),
        "a daemon that cannot exec must take the whole install down with it"
    );
}
