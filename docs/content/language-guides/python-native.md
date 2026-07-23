# Quickstart: Building a Murmur Tool in Python (Native Binary Path)

Murmur is WASM-first, but it also supports a **native compatibility path** for tools.

Use this path when:

- you want to ship quickly from existing Python code
- a dependency stack is difficult to compile to WASM
- you prefer a process-based tool runtime

> Native tools trade off isolation compared to WASM tools. In Murmur docs/specs this is an explicit compatibility mode.

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
[`requires_files`](../reference/manifest-schema.md).

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
  - name: py-echo-native
    version: "0.1.0"
    runtime: native
```

---

## Step 7 — Run and test

```bash
mur run
```

When the capsule invokes `py-echo-native`, the runtime executes the packaged binary and forwards the normalized result back to the caller.

The binary does not inherit the host's environment as-is: `HOME` is replaced with a session-scoped synthetic directory, credential-shaped variables (`GITHUB_TOKEN`, `*_API_KEY`, `AWS_*`, etc.) are stripped, and only a safe baseline (`PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TMPDIR`, `TEMP`, `TMP`, `CARGO_HOME`, `RUSTUP_HOME`, `TERM`) passes through — the same sanitization [shell tool subprocesses](../how-to/agent-shell-access.md#step-4-manage-the-subprocess-environment) get, applied regardless of whether the capsule declares `capabilities.shell.allow`.

---

## Common errors

**Tool launches but fails with missing shared libraries**  
Bundle dependencies into the executable (or run in an environment matching your target).

**`runtime` mismatch in capsule manifest**  
If the artifact is published as native, declare it with `runtime: native` in the capsule manifest.

**Binary works locally but fails on another platform**  
Build/publish per target platform (for example `linux/amd64`, `linux/arm64`, `darwin/arm64`) and resolve the correct variant at runtime.

---

## Which Python path should you choose?

- Choose **[Python Tool Quickstart (WASM)](python.md)** for stronger isolation and component-model alignment.
- Choose **Python native** when ecosystem compatibility or packaging speed is more important for your use case.
