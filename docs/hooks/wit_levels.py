"""Generate the WIT level tables on reference/wit-levels.md at build time.

The page carries a marker comment; this hook replaces it with two tables built
from the committed level table (crates/capsule-runtime/wit/levels.txt) and the
repository's release tags, so the page cannot drift from either:

- a summary: each level, the mur releases that serve it, and what changed from
  the level before;
- a matrix: one row per package, one column per level.

A release serves a level when the `package murmur:*` declarations under
crates/capsule-runtime/wit at its tag equal that level's set. They are read
from the object store the way scripts/lib/wit-packages.sh reads a revision
(`git ls-tree` + `git show`, `%` escaping dropped). A tag whose set equals no
level is listed as serving none.

Any failure to read the file or git raises, so `mkdocs build --strict` fails
instead of publishing an empty table.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

PAGE = "reference/wit-levels.md"
MARKER = "<!-- wit-levels:tables -->"
WIT_DIR = "crates/capsule-runtime/wit"
LEVELS_FILE = f"{WIT_DIR}/levels.txt"

# The declaration scripts/lib/wit-packages.sh matches, anchored at line start.
_PACKAGE = re.compile(r"^package (murmur:%?[a-z-]+)@([0-9]+\.[0-9]+\.[0-9]+)", re.M)
_VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def _repo_root(config) -> Path:
    return Path(config["config_file_path"]).resolve().parent.parent


def _git(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"wit_levels hook: `git {' '.join(args)}` failed: {result.stderr.strip()}"
        )
    return result.stdout


def _read_levels(root: Path) -> dict[int, dict[str, str]]:
    path = root / LEVELS_FILE
    text = path.read_text(encoding="utf-8")
    levels: dict[int, dict[str, str]] = {}
    for number, raw in enumerate(text.splitlines(), start=1):
        fields = raw.split("#", 1)[0].split()
        if not fields:
            continue
        if len(fields) != 2 or "@" not in fields[1] or not fields[0].isdigit():
            raise RuntimeError(f"wit_levels hook: {LEVELS_FILE} line {number}: {raw!r}")
        package, version = fields[1].split("@", 1)
        if not package.startswith("murmur:") or not _VERSION.match(version):
            raise RuntimeError(f"wit_levels hook: {LEVELS_FILE} line {number}: {raw!r}")
        levels.setdefault(int(fields[0]), {})[package] = version
    if not levels or sorted(levels) != list(range(1, len(levels) + 1)):
        raise RuntimeError(f"wit_levels hook: {LEVELS_FILE} does not number its levels from 1")
    return levels


def _version_key(tag: str) -> tuple:
    parts = re.findall(r"\d+", tag)
    return tuple(int(part) for part in parts)


def _tag_packages(root: Path, tag: str) -> dict[str, str]:
    files = [
        name
        for name in _git(root, "ls-tree", "-r", "--name-only", tag, "--", WIT_DIR).splitlines()
        if name.endswith(".wit")
    ]
    packages: dict[str, str] = {}
    for name in files:
        for package, version in _PACKAGE.findall(_git(root, "show", f"{tag}:{name}")):
            packages[package.replace("murmur:%", "murmur:")] = version
    return packages


def _releases(root: Path, levels: dict[int, dict[str, str]]) -> tuple[dict[int, list[str]], list[str]]:
    tags = sorted(_git(root, "tag", "-l", "v*").split(), key=_version_key)
    if not tags:
        raise RuntimeError(
            "wit_levels hook: the repository has no v* tags; check out with full history"
        )
    serving: dict[int, list[str]] = {level: [] for level in levels}
    unlevelled: list[str] = []
    for tag in tags:
        packages = _tag_packages(root, tag)
        match = next((n for n, s in levels.items() if s == packages), None)
        if match is None:
            unlevelled.append(tag)
        else:
            serving[match].append(tag)
    return serving, unlevelled


def _changes(before: dict[str, str], after: dict[str, str]) -> str:
    changes = []
    for package in sorted(set(before) | set(after)):
        old, new = before.get(package), after.get(package)
        if old == new:
            continue
        if old is None:
            changes.append(f"`{package}` added at `{new}`")
        elif new is None:
            changes.append(f"`{package}` removed")
        else:
            changes.append(f"`{package}` `{old}` → `{new}`")
    return "<br>".join(changes)


def _tables(root: Path) -> str:
    levels = _read_levels(root)
    serving, unlevelled = _releases(root, levels)
    numbers = sorted(levels)

    lines = ["| Level | mur releases | Changed from the previous level |", "|---:|---|---|"]
    for number in numbers:
        releases = ", ".join(f"`{tag}`" for tag in serving[number]) or "not yet released"
        changed = "first level" if number == 1 else _changes(levels[number - 1], levels[number])
        lines.append(f"| {number} | {releases} | {changed} |")
    if unlevelled:
        tags = ", ".join(f"`{tag}`" for tag in unlevelled)
        lines += ["", f"Releases serving no level: {tags}."]

    packages = sorted(set().union(*levels.values()))
    lines += [
        "",
        "| Package | " + " | ".join(f"Level {n}" for n in numbers) + " |",
        "|---|" + "---|" * len(numbers),
    ]
    for package in packages:
        cells = [f"`{levels[n][package]}`" if package in levels[n] else "—" for n in numbers]
        lines.append(f"| `{package}` | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def on_page_markdown(markdown, page, config, files):
    if page.file.src_uri != PAGE:
        return markdown
    if MARKER not in markdown:
        raise RuntimeError(f"wit_levels hook: {PAGE} has no {MARKER} marker")
    return markdown.replace(MARKER, _tables(_repo_root(config)))
