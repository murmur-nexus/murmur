# How to pin a program download

A tool that drives a heavy host program, such as a headless browser, works only where that program
is installed and on `PATH`. A tool can instead pin a download of the program per platform in its
own `murmur.yaml`: [`mur install`](../reference/cli.md#mur-install) fetches it once into
`~/.murmur/programs/`, checks its SHA-256, and a capsule that grants the program by name runs that
copy, with no `PATH` entry and no host path in its manifest. A host that already has a program of
that name on `PATH` keeps using its own. This guide pins `chrome-headless-shell` for a browser
tool.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [programs](../reference/manifest.md#artifact-programs) | What each host program a tool runs needs, in the tool's own manifest |
| [programs.&lt;program&gt;.download](../reference/manifest.md#artifact-programs-download) | The pinned download of a program, per platform |
| [artifacts](../reference/manifest.md#field-artifacts) | Which tools the capsule pins |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which programs the capsule's commands may run, by name |

---

## Step 1 — declare the download in the tool's manifest

In the tool's own `murmur.yaml`, add a `programs:` block naming the program as the capsule will
grant it, with one `download:` entry per platform the tool supports. `url` is the upstream file,
`sha256` the SHA-256 of its bytes, and `run` the file to run inside the zip. The build number and
checksums below are placeholders; take yours from the upstream release you pin.

```yaml
name: browser-tool
version: "0.1.0"
runtime: tool
requires_files:
  - tool.wasm
programs:
  chrome-headless-shell:
    download:
      linux-x86_64:
        url: https://storage.googleapis.com/chrome-for-testing-public/<build>/linux64/chrome-headless-shell-linux64.zip
        sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
        run: chrome-headless-shell-linux64/chrome-headless-shell
      darwin-aarch64:
        url: https://storage.googleapis.com/chrome-for-testing-public/<build>/mac-arm64/chrome-headless-shell-mac-arm64.zip
        sha256: fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
        run: chrome-headless-shell-mac-arm64/chrome-headless-shell
```

Compute a checksum from a downloaded copy with `sha256sum <file>` (`shasum -a 256 <file>` on
macOS). A download that is the program itself, not a zip, leaves out `run`.

Build the tool:

```bash
mur build
```

`mur build` refuses a malformed entry with [`E-BLD-008`](../reference/diagnostics.md#e-bld-008),
naming the field, such as `programs.chrome-headless-shell.download.linux-x86_64.sha256`.

## Step 2 — grant the program in the capsule's manifest

In the capsule's `murmur.yaml`, pin the tool under `artifacts:` and name the program, spelled
exactly as the tool's `programs:` key, in `capabilities.shell.allow`:

```yaml
artifacts:
  - name: local/browser-tool
    version: "0.1.0"
    runtime: tool
capabilities:
  containment: scoped
  shell:
    allow:
      - chrome-headless-shell
```

The capsule names no folder and no path for the program.

## Step 3 — install

Install the tool's `.mur.zip`, then the capsule's dependencies:

```bash
mur install ./browser-tool-0.1.0.mur.zip
mur install
```

--8<-- "includes/mur-pull-info.md"

`mur install` pins the tool in `murmur.lock` with an `operator` origin, then fetches the download
for this host's platform:

```text
Installed local/browser-tool@0.1.0 from ./browser-tool-0.1.0.mur.zip
  fetched chrome-headless-shell for linux-x86_64 (local/browser-tool): 98 MB, sha256 verified
```

A later `mur install` verifies the stored copy and fetches nothing. A download that does not match
its `sha256` is refused with [`E-REG-014`](../reference/diagnostics.md#e-reg-014) and nothing is
stored. A tool a capsule pulls at runtime gets no download.

## Step 4 — check which copy runs

```bash
mur doctor
```

The `Pinned programs:` block says whether the store's copy is present and which copy a launch
runs:

```text
Pinned programs:
  ✓  local/browser-tool (chrome-headless-shell): pinned copy present; runs /home/you/.murmur/programs/0123…cdef/files/chrome-headless-shell-linux64/chrome-headless-shell (pinned)
```

A copy that is missing or fails its checksum adds [`W-RUN-012`](../reference/diagnostics.md#w-run-012)
with a `Fix:` line; `mur install` repairs both. `mur run --explain-scope` lists the same choice
under `pinned programs:`, and every session records it in
`session_start.effective_grants.pinned_programs`.

## Step 5 — keep a distribution package instead

A host that installs the program from its distribution keeps it: a program of that name on `PATH`
always runs instead of the store's copy, and gets no extra grant. With
`/usr/bin/chrome-headless-shell` installed, `mur doctor` says so:

```text
Pinned programs:
  ✓  local/browser-tool (chrome-headless-shell): pinned copy present; runs /usr/bin/chrome-headless-shell (host)
```

A host program that needs libraries or folders of its own gets them through the tool's
[`needs`](../reference/manifest.md#artifact-programs) for the same program.

## Summary

| Feature / setting | How it works |
|---|---|
| `programs.<program>.download.<platform>` | The tool pins `url`, `sha256` and, for a zip, `run`; mur never re-hosts the file |
| `mur install` | Fetches each pinned download for this host once into `~/.murmur/programs/<sha256>/` and checks it |
| `capabilities.shell.allow` | Naming the program grants the store's copy; no `PATH` entry or host path is written |
| A program of that name on `PATH` | Runs instead of the store's copy |
| `mur doctor` | Shows each pinned program as present, missing or failing its checksum, and which copy runs |
