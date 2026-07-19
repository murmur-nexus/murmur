# Murmur Docs

This documentation site is powered by MkDocs + Material for MkDocs.

## Install tooling (pinned)

From repo root:

```bash
python3 -m venv .venv-docs
source .venv-docs/bin/activate
pip install -r docs/requirements.txt
```

## Build docs (strict)

```bash
cd docs
mkdocs build --strict
```

## Serve locally

```bash
cd docs
mkdocs serve
```

## Optional: suppress upstream MkDocs 2.0 warning banner

```bash
NO_MKDOCS_2_WARNING=true mkdocs build --strict
```

(Use only to reduce CI noise; it does not change behavior.)
