"""Set every default-artifact version the pages render from the published index.

At build time this hook reads `artifacts-index.json` on the latest
murmur-nexus/default-artifacts release and sets `extra.v.murmur_<name>` (each
`-` in the bare name written `_`, e.g. `murmur_driver_anthropic`) to the newest
version that index lists for the artifact. Pages render a version as
`{{ v.murmur_driver_anthropic }}`; `docs/mkdocs.yml` holds none. The docs do
not judge fit: `mur` does, when it installs.

It runs before mkdocs-macros reads `extra` (priority 100; hooks otherwise run
after plugins), because macros copies `extra` shallowly in its own `on_config`.

Any failure raises, so `mkdocs build --strict` fails instead of publishing an
undefined or stale version: an index that cannot be fetched or is not a
schema 2 index of the `murmur` scope, and an `extra.v` key in `mkdocs.yml` this
hook would set.
"""

from __future__ import annotations

import json
import urllib.request

from mkdocs.exceptions import PluginError
from mkdocs.plugins import event_priority

INDEX_URL = (
    "https://github.com/murmur-nexus/default-artifacts/releases/latest/download/"
    "artifacts-index.json"
)
TIMEOUT_SECS = 30
SCOPE = "murmur"
SCHEMA_VERSION = 2


def _fail(reason: str) -> PluginError:
    return PluginError(
        f"artifact_versions hook: could not read the artifact index at {INDEX_URL}: "
        f"{reason}. The docs build needs network access to read artifact versions."
    )


def _read_index() -> dict:
    try:
        with urllib.request.urlopen(INDEX_URL, timeout=TIMEOUT_SECS) as response:
            index = json.loads(response.read().decode("utf-8"))
    except (OSError, ValueError) as error:
        raise _fail(str(error)) from error
    if not isinstance(index, dict):
        raise _fail("the document is not a JSON object")
    if index.get("schema_version") != SCHEMA_VERSION:
        raise _fail(
            f"schema_version is {index.get('schema_version')!r}, not {SCHEMA_VERSION}"
        )
    if index.get("scope") != SCOPE:
        raise _fail(f"scope is {index.get('scope')!r}, not {SCOPE!r}")
    return index


def _versions(index: dict) -> dict[str, str]:
    versions: dict[str, str] = {}
    try:
        for artifact in index["artifacts"]:
            key = f"{SCOPE}_{artifact['name'].replace('-', '_')}"
            versions[key] = artifact["versions"][0]["version"]
    except (KeyError, IndexError, TypeError, AttributeError) as error:
        raise _fail(f"an artifact entry lacks a name or a version: {error!r}") from error
    return versions


@event_priority(100)
def on_config(config):
    extra_v = config["extra"].setdefault("v", {})
    versions = _versions(_read_index())
    for key in sorted(versions):
        if key in extra_v:
            raise PluginError(
                f"artifact_versions hook: docs/mkdocs.yml sets extra.v.{key}; artifact "
                f"versions come from the published index at {INDEX_URL}, not mkdocs.yml. "
                "Remove the key."
            )
    extra_v.update(versions)
    return config
