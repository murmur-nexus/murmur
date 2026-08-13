# Quickstart: Building a Murmur Tool in Python

Murmur's ecosystem is currently Rust-centric, but Murmur is designed to work well with **any language**.  
If your tool can be packaged as a supported artifact runtime, it can participate in the ecosystem.

This guide walks through building, packaging, and publishing a Murmur tool artifact written in Python.

By the end, you will have a working `.mur.zip` published to your local registry and a capsule that can invoke it.

Prefer shipping a native binary instead of WASM? See [Python Tool Quickstart (Native)](python-native.md).

---

## How it works

A Murmur tool can be distributed as a WASM component. For Python, the flow is:

```text
tool.py → componentize-py → tool.wasm → mur build → .mur.zip → mur publish
```

- **componentize-py** compiles Python + WIT interface definitions into a WASM component
- **mur build** packages the WASM and manifest into a `.mur.zip` artifact
- **mur publish** registers the artifact so capsules can declare and install it

---

## Prerequisites

- Python 3.10+
- `componentize-py` installed:

```bash
pip install componentize-py
```

- `mur` CLI installed

---

## Step 1 — Set up the project folder

```text
my-tool/
  wit/
    tool.wit
  tool.py
  murmur.yaml
```

---

## Step 2 — Write the WIT interface file

Create `my-tool/wit/tool.wit`:

```wit
package murmur:tool@0.1.0;

interface run {
  enum status {
    passed,
    failed,
    error,
  }

  record tool-input {
    data: option<string>,
    log-path: option<string>,
  }

  record tool-result {
    status: status,
    summary: option<string>,
    data: option<string>,
    data-path: option<string>,
    truncated: bool,
    metadata: list<tuple<string, string>>,
  }

  run: func(input: tool-input) -> tool-result;
}

world tool {
  export run;
}
```

Notes:

- Keep only this file in the `wit/` directory for this quickstart.
- Pass the `wit/` directory to `--wit-path`.
- The `@0.1.0` on the `package` line matches the version the host's `murmur:tool`
  WIT ships (see [WIT Interfaces](../reference/wit-interfaces.md)). The host
  resolves a tool's `murmur:tool/run` export by versioned name and nothing else,
  so an unversioned `package murmur:tool;` fails to load. Always pin the version.

---

## Step 3 — Generate Python bindings

```bash
cd my-tool
componentize-py --wit-path ./wit --world tool bindings .
```

This generates `wit_world/` bindings (do not hand-edit generated files).

---

## Step 4 — Implement the tool

Create `my-tool/tool.py`:

```python
from wit_world import exports
from wit_world.exports import run


class Run(exports.Run):
    def run(self, input: run.ToolInput) -> run.ToolResult:
        path = input.data or "input.jsonl"

        try:
            with open(path) as f:
                count = sum(1 for line in f if line.strip())

            return run.ToolResult(
                status=run.Status.PASSED,
                summary=f"{count} lines",
                data=str(count),
                data_path=None,
                truncated=False,
                metadata=[],
            )
        except OSError as e:
            return run.ToolResult(
                status=run.Status.ERROR,
                summary=str(e),
                data=None,
                data_path=None,
                truncated=False,
                metadata=[],
            )
```

Key points:

- Class name must be `Run`
- Method must be `run(self, input)` and return `ToolResult`
- On failures, return `status = error` rather than crashing

---

## Step 5 — Compile to WASM

```bash
componentize-py --wit-path ./wit --world tool componentize tool -o tool.wasm
```

Expected output:

```text
Component built successfully
```

---

## Step 6 — Write the manifest

Create `my-tool/murmur.yaml`:

```yaml
name: jsonl-line-count
version: "0.1.0"
requires_files:
  - tool.wasm
```

`requires_files:` is what `mur build` packages alongside the manifest. Without it the compiled
`tool.wasm` is not put in the artifact — see [`requires_files`](../reference/manifest.md).

---

## Step 7 — Build and publish

From `my-tool/`:

```bash
mur build .
mur publish jsonl-line-count-0.1.0.mur.zip
```

Expected publish output:

```text
Published jsonl-line-count@0.1.0
```

---

## Step 8 — Declare the tool in a capsule

In capsule `murmur.yaml`:

```yaml
name: my-capsule
version: "0.1.0"
artifacts:
  - name: jsonl-line-count
    version: "0.1.0"
    runtime: wasm
```

---

## Step 9 — Run

With your capsule artifact in place, run:

```bash
mur run
```

At runtime, Murmur resolves `jsonl-line-count@0.1.0`, installs it, and dispatches it when invoked.

---

## Common errors

**`World 'tool' not found in package 'murmur:tool'`**  
`--wit-path` pointed to a file instead of the directory. Use `--wit-path ./wit`.

**`package identifier does not match previous package name`**  
Your `wit/` folder contains extra WIT files from other packages.

**`No module named ...`**  
Bindings were not generated (or imports don't match generated paths). Re-run:

```bash
componentize-py --wit-path ./wit --world tool bindings .
```
