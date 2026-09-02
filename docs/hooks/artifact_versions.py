"""Fail the build when the site prints an artifact version the registry does not publish.

`default-artifacts/artifacts.toml` is the source of truth for every published
artifact version. The docs restate those versions in two places: the
`extra.v.murmur_*` table in `mkdocs.yml`, and — once the macros plugin has
expanded `{{ v.* }}` — the rendered text of any page that names a default
artifact at a version. This hook compares both against `artifacts.toml`.

The check is conditional on `MURMUR_DEFAULT_ARTIFACTS_DIR`, the one way this
repository names a `default-artifacts` checkout. The docs CI job and
`deploy.sh` build from this repository alone, with no sibling checkout on the
runner, so an unconditional check would fail every build and every deploy. With
the variable unset the hook logs one line and compares nothing; on a machine
that has the checkout — the machine where `artifacts.toml` is edited — a stale
docs version fails `mkdocs build --strict` on the spot.

`on_page_markdown` carries `@event_priority(-100)` because the macros plugin
expands `{{ v.* }}` during that same event. At default priority the hook would
scan unexpanded `{{ ... }}` templates and see no versions at all.
"""

from __future__ import annotations

import logging
import os
import re
import tomllib
from pathlib import Path

from mkdocs.exceptions import PluginError
from mkdocs.plugins import event_priority

log = logging.getLogger("mkdocs.hooks.artifact_versions")

ENV_VAR = "MURMUR_DEFAULT_ARTIFACTS_DIR"

# A default artifact named at a concrete version, in either the `name@version`
# reference form or the `name-version.mur.zip` filename form.
_ARTIFACT_LITERAL = re.compile(
    r"(?P<name>murmur-(?:driver|hook|tool|skill)-[a-z0-9-]+)[@-](?P<version>[0-9]+\.[0-9]+\.[0-9]+)"
)

# The same assertion split across two YAML lines, as every manifest example
# writes it. This is the form a reader copies into their own `murmur.yaml`, and
# it carries no `@`, so `_ARTIFACT_LITERAL` cannot see it.
_MANIFEST_NAME = re.compile(
    r"^\s*-?\s*name:\s*[\"']?(?P<name>murmur-(?:driver|hook|tool|skill)-[a-z0-9-]+)[\"']?\s*$"
)
_MANIFEST_VERSION = re.compile(
    r"^\s*version:\s*[\"']?(?P<version>[0-9]+\.[0-9]+\.[0-9]+)[\"']?\s*$"
)


def _manifest_pins(markdown: str):
    """Yield (literal, name, version) for each `name:`/`version:` manifest pair.

    Pages illustrate shapes with invented artifact names as often as they name
    real ones, so an unrecognized name is left to the caller to skip rather than
    reported — unlike the `@version` form, which appears in command lines meant
    to be run verbatim.
    """
    lines = markdown.split("\n")
    for i, line in enumerate(lines):
        name_match = _MANIFEST_NAME.match(line)
        if not name_match:
            continue
        # `version:` normally follows `name:` directly; allow a couple of
        # intervening keys, but stop at the next entry or a blank line.
        for candidate in lines[i + 1 : i + 4]:
            if not candidate.strip() or _MANIFEST_NAME.match(candidate):
                break
            version_match = _MANIFEST_VERSION.match(candidate)
            if version_match:
                name = name_match.group("name")
                version = version_match.group("version")
                yield f"{name} version: {version}", name, version
                break

# Artifact name -> published version, or None when no checkout was named and the
# comparison is therefore disabled for this build.
_published: dict[str, str] | None = None

# Page-level disagreements, collected across pages and drained in on_post_build.
_findings: list[str] = []


def _artifact_name(key: str) -> str:
    """Map an `extra.v` key to the artifact it names: `murmur_tool_git` -> `murmur-tool-git`."""
    return key.replace("_", "-")


def _macro_key(name: str) -> str:
    return name.replace("-", "_")


def _report(headline: str, errors: list[str]) -> str:
    body = "\n".join(f"  - {error}" for error in errors)
    return f"artifact_versions: {headline}\n{body}"


def on_config(config):
    global _published, _findings
    _published = None
    _findings = []

    root = os.environ.get(ENV_VAR)
    if not root:
        log.info(
            "artifact_versions: %s is unset; not checking docs versions against "
            "artifacts.toml. Set it to a default-artifacts checkout to enable the check.",
            ENV_VAR,
        )
        return config

    manifest = Path(root) / "artifacts.toml"
    if not manifest.is_file():
        log.info(
            "artifact_versions: no artifacts.toml at %s; not checking docs versions "
            "against it.",
            manifest,
        )
        return config

    try:
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise PluginError(f"artifact_versions: cannot read {manifest}: {exc}") from exc

    _published = {
        entry["name"]: str(entry["version"])
        for entry in data.get("artifact", [])
        if "name" in entry and "version" in entry
    }

    errors: list[str] = []
    for key, claimed in ((config["extra"] or {}).get("v") or {}).items():
        if not key.startswith("murmur_"):
            continue
        name = _artifact_name(key)
        actual = _published.get(name)
        if actual is None:
            errors.append(
                f"extra.v.{key} names artifact '{name}', which has no [[artifact]] entry "
                f"in {manifest}"
            )
        elif str(claimed) != actual:
            errors.append(
                f'extra.v.{key} claims "{claimed}", but {manifest} publishes '
                f'\'{name}\' at "{actual}"'
            )

    if errors:
        raise PluginError(
            _report("mkdocs.yml asserts versions that are not published:", errors)
        )

    log.info(
        "artifact_versions: %d published artifacts read from %s", len(_published), manifest
    )
    return config


# Negative priority runs this after the macros plugin, so the scanned text holds
# resolved versions rather than `{{ v.* }}` templates.
@event_priority(-100)
def on_page_markdown(markdown, page, config, files):
    if _published is None:
        return markdown

    src_uri = page.file.src_uri
    for match in _ARTIFACT_LITERAL.finditer(markdown):
        name = match.group("name")
        version = match.group("version")
        actual = _published.get(name)
        if actual is None:
            _findings.append(
                f"{src_uri}: '{match.group(0)}' names artifact '{name}', which has no "
                f"[[artifact]] entry in artifacts.toml"
            )
        elif version != actual:
            _findings.append(
                f"{src_uri}: '{match.group(0)}' pins '{name}' at \"{version}\", but the "
                f'published version is "{actual}" — write '
                f"{{{{ v.{_macro_key(name)} }}}} instead of a literal"
            )

    for literal, name, version in _manifest_pins(markdown):
        actual = _published.get(name)
        if actual is not None and version != actual:
            _findings.append(
                f"{src_uri}: '{literal}' pins '{name}' at \"{version}\", but the "
                f'published version is "{actual}" — write '
                f"{{{{ v.{_macro_key(name)} }}}} instead of a literal"
            )
    return markdown


def on_post_build(config):
    global _findings
    findings = list(dict.fromkeys(_findings))
    _findings = []
    if findings:
        raise PluginError(
            _report("pages assert versions that are not published:", findings)
        )
