# Quickstart: Building a Murmur Tool in Python (Native Binary Path)

Murmur is WASM-first, but it also supports a **native compatibility path** for tools.

Use this path when:

- you want to ship quickly from existing Python code
- a dependency stack is difficult to compile to WASM
- you prefer a process-based tool runtime

> A native tool runs in the capsule's own subprocess sandbox, with the capsule-wide grants. A WASM tool can be narrowed below the capsule's grants on its own artifact entry; a native tool cannot. See [Native tools](../reference/containment.md#native-tools).

---

## How it works

For native tools, the runtime launches a binary and normalizes its output to Murmur's tool-result envelope.

Typical flow:

```text
tool.py → package as executable → mur build → .mur.zip → mur publish → capsule invokes tool
```

---

## Prerequisites

- Python 3.10+
- `mur` CLI installed
- A packaging approach for your platform (example below uses `pyinstaller`)

---

## Step 1 — Create the tool project

```text
my-native-tool/
  murmur.yaml
  src/
    tool.py
  bin/
    run            # final executable copied here
```

---

## Step 2 — Implement tool logic

Create `src/tool.py`:

> Tip: if your runtime version expects additional envelope fields, generate a canonical native stub first with `murmur-scaffold` and copy its output shape.

```python
#!/usr/bin/env python3
import json
import sys


def main() -> int:
    raw = sys.stdin.read().strip() or "{}"
    payload = json.loads(raw)

    data = payload.get("data")
    summary = f"received: {data}" if data is not None else "received empty payload"

    result = {
        "status": "passed",
        "summary": summary,
        "data": json.dumps({"echo": data}),
        "truncated": False,
        "metadata": {},
    }

    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

---

## Step 3 — Build an executable

Example with PyInstaller:

```bash
pip install pyinstaller
pyinstaller --onefile src/tool.py --name run
mkdir -p bin
cp dist/run bin/run
chmod +x bin/run
```

At this point, `bin/run` is the executable Murmur packages.

---

## Step 4 — Write the tool manifest

Create `murmur.yaml`:

```yaml
name: py-echo-native
version: "0.1.0"
runtime: native
requires_files:
  - bin/run
```

`requires_files:` is what `mur build` packages alongside the manifest. Without it the executable
built in Step 3 is not put in the artifact — see
[`requires_files`](../reference/manifest.md).

---

## Step 5 — Build and publish

From `my-native-tool/`:

```bash
mur build .
mur publish py-echo-native-0.1.0.mur.zip
```

---

## Step 6 — Declare in a capsule

In capsule `murmur.yaml`:

```yaml
name: my-capsule
version: "0.1.0"
artifacts:
  - name: local/py-echo-native
    version: "0.1.0"
    runtime: native
```

---

## Step 7 — Run and test

```bash
mur run
```

When the capsule invokes `py-echo-native`, the runtime executes the packaged binary and forwards the normalized result back to the caller.

The binary does not inherit the host's environment as-is: `HOME` is replaced with a session-scoped synthetic directory, credential-shaped variables (`GITHUB_TOKEN`, `*_API_KEY`, `AWS_*`, etc.) are stripped, and only a safe baseline (`PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `CARGO_HOME`, `RUSTUP_HOME`, `TERM`) passes through — the same sanitization [shell tool subprocesses](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment) get, applied regardless of whether the capsule declares `capabilities.shell.allow`.

On Linux the binary also runs in the same sandbox as a shell command, with the capsule's grants:

| The tool… | Needs |
|---|---|
| Reads or writes files | The files inside the work folder, in the session's temporary folder at `$TMPDIR`, or in its home folder at `$HOME`. On the Full tier nothing outside these three is writable |
| Runs another program | That program in `capabilities.shell.allow` |
| Connects to a host | That host in `capabilities.network.allow` |

A PyInstaller `--onefile` build unpacks itself into `$TMPDIR` before it runs your code. The runtime
sets `TMPDIR`, `TMP` and `TEMP` to the session's private temporary folder, `tmp/` in its records
directory, on every tier: as `/tmp` where the host maps `/tmp` onto that folder, and as the folder's
absolute path where it does not. See
[The session's temporary folder](../reference/containment.md#temp-folder).

---

## Common errors

**Tool launches but fails with missing shared libraries**  
Bundle dependencies into the executable (or run in an environment matching your target).

**Tool fails with `Permission denied` on Linux**  
The sandbox refused a file outside the workdir or a program outside `capabilities.shell.allow`. See the table in Step 7.

**`runtime` mismatch in capsule manifest**  
If the artifact is published as native, declare it with `runtime: native` in the capsule manifest.

**Binary works locally but fails on another platform**  
Build/publish per target platform (for example `linux/amd64`, `linux/arm64`, `darwin/arm64`) and resolve the correct variant at runtime.

---

## Which Python path should you choose?

- Choose **[Python Tool Quickstart (WASM)](python.md)** for stronger isolation and component-model alignment.
- Choose **Python native** when ecosystem compatibility or packaging speed is more important for your use case.
