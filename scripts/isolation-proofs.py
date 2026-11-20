#!/usr/bin/env python3
"""Prove Murmur's isolation-sensitive tests on the host, and judge the evidence.

A test that cannot isolate a capsule on the host it runs on prints a host-skip line and returns,
which nextest counts as a pass. Inside a capsule every such test lands there: a capsule command's
seccomp filter refuses `unshare`, `mount` and `seccomp`, so a Landlock or guest-worker test run
by a board's capsule stage proves nothing. This script is how those tests are proven anyway: a
board's capsule stage hands them over, a host stage runs them here, outside any sandbox, and the
judging stage reads per-test evidence in which a skip is never a pass.

    python3 -I scripts/isolation-proofs.py list [--json] [--expand]
    python3 -I scripts/isolation-proofs.py handover target/nextest/ci/junit.xml --out handover.json
    python3 -I scripts/isolation-proofs.py run --set landlock --set wasm-guest-workers --evidence ev.json
    python3 -I scripts/isolation-proofs.py judge ev.json --require-set landlock

The isolation sets are named in .config/isolation-tests.toml; the contributor page
docs/content/contributing/isolation-tests.md describes the placements, the grants the host needs,
and what running card-authored tests as the operator exposes.

Exit codes:
  list, handover  0 done, 2 refused (a malformed classification or report, or a set that expands
                  to no test)
  run             0 every test passed, 1 a test failed, 3 no test failed but at least one was
                  skipped for the host or never ran, 2 refused (and no evidence file exists)
  judge           0 pass, 1 a failure, no evidence or a missing required set or test, 2 the
                  evidence is unusable (wrong format, another commit or tree, a JUnit report whose
                  hash no longer matches, or a record that disagrees with its reports)

Standard library only, Python 3.11 or later (tomllib).
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import xml.etree.ElementTree as ElementTree
from dataclasses import dataclass, field

CLASSIFICATION_FORMAT = "murmur.isolation-classification/1"
HANDOVER_FORMAT = "murmur.isolation-handover/1"
EVIDENCE_FORMAT = "murmur.isolation-evidence/1"
CLASSIFICATION_VERSION = 1
DEFAULT_CLASSIFICATION = os.path.join(".config", "isolation-tests.toml")
DEFAULT_NEXTEST_CONFIG = os.path.join(".config", "nextest.toml")
DEFAULT_PREFLIGHT = os.path.join(".github", "scripts", "assert-containment-tier.py")
NEXTEST_PROFILE = "isolation"

# The closed vocabulary of what a set needs from the host.
NEEDS = (
    "landlock",
    "user-namespace",
    "network-namespace",
    "mount-namespace",
    "pid-namespace",
    "cgroup-delegation",
    "seccomp",
    "wasmtime",
)
# Lowest first: a selection's preflight asserts the highest class among its sets.
CLASSES = ("scoped", "sealed")
SET_KEYS_REQUIRED = ("name", "package", "target", "filter", "needs", "class", "why")
SET_KEYS_OPTIONAL = ("features",)
NAME_RE = re.compile(r"[a-z0-9-]+")
PACKAGE_RE = re.compile(r"[A-Za-z0-9_-]+")
TARGET_RE = re.compile(r"(lib)|(test|bin):([A-Za-z0-9_-]+)")
FEATURE_RE = re.compile(r"[A-Za-z0-9_/+.-]+")
EXACT_NAME_RE = re.compile(r"[A-Za-z0-9_:]+")

# A host skip: a line holding `[SKIP-HOST]` (the capsule_runtime helpers and murmur-cli's
# `common::tiers`), or one starting `skipping <test>:` (six capsule-runtime sandbox tests,
# `scoped_landlock_enforcement` among them). Leading whitespace is ignored.
HOST_SKIP_MARK = "[SKIP-HOST]"
UNPREFIXED_SKIP_RE = re.compile(r"skipping \S+:")
FAILURE_TAGS = ("failure", "error", "flakyFailure", "flakyError", "rerunFailure", "rerunError")
DETAIL_CHARS = 300

STATUSES = ("passed", "failed", "skipped_for_host", "not_run")
EXIT_PASS, EXIT_FAIL, EXIT_REFUSED, EXIT_NO_EVIDENCE = 0, 1, 2, 3
VERDICT_EXIT = {"pass": EXIT_PASS, "fail": EXIT_FAIL, "no_evidence": EXIT_NO_EVIDENCE}

# The environment cargo, and so every test, sees: these names, and any name starting with one of
# the prefixes unless it contains one of the secret words. Everything else is dropped by name.
ENV_ALLOWED = (
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
)
ENV_ALLOWED_PREFIXES = ("CARGO_", "RUSTUP_", "RUST_")
ENV_SECRET_WORDS = ("TOKEN", "SECRET", "PASSWORD", "KEY")

SCOPE_ARGV = (
    "systemd-run",
    "--user",
    "--scope",
    "--quiet",
    "--collect",
    "-p",
    "Delegate=yes",
    "-p",
    "TasksMax=infinity",
    "--",
)
# git's output must not depend on the operator's git configuration, or the judge's recomputed
# diff hash would differ from the runner's on the same tree.
GIT_DIFF_ARGV = (
    "diff",
    "HEAD",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)
GIT_ENV_DROPPED = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY")


class Refusal(Exception):
    """Why a command did nothing: printed as one message, exit 2, never a traceback."""


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def clip(text: str, limit: int = DETAIL_CHARS) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[:limit] + " …"


# ── Filters ─────────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Term:
    kind: str  # "exact" or "regex"
    value: str

    def matches(self, name: str) -> bool:
        if self.kind == "exact":
            return name == self.value
        return re.search(self.value, name) is not None

    def text(self) -> str:
        return f"test(={self.value})" if self.kind == "exact" else f"test(/{self.value}/)"


_EXACT_TERM = re.compile(r"test\(=([^()\s]*)\)")
_REGEX_TERM = re.compile(r"test\(/(.*?)/\)")
_SEPARATOR = re.compile(r"\s*\|\s*")


def parse_filter(text: object) -> list[Term]:
    """The terms of a filter in the classification's grammar, or ValueError naming what is wrong.

    One or more terms joined by `|`; each is `test(=<exact test path>)` or `test(/<regex>/)`.
    The same string goes to nextest's `-E`, and these terms decide membership here.
    """
    if not isinstance(text, str) or not text.strip():
        raise ValueError("a filter must be a non-empty string")
    terms: list[Term] = []
    position = 0
    source = text.strip()
    while True:
        exact = _EXACT_TERM.match(source, position)
        regex = None if exact else _REGEX_TERM.match(source, position)
        if exact:
            name = exact.group(1)
            if not EXACT_NAME_RE.fullmatch(name):
                raise ValueError(f"term {exact.group(0)!r} does not name a test path")
            terms.append(Term("exact", name))
            position = exact.end()
        elif regex:
            pattern = regex.group(1)
            if not pattern:
                raise ValueError(f"term {regex.group(0)!r} has an empty regex")
            try:
                re.compile(pattern)
            except re.error as error:
                raise ValueError(f"term {regex.group(0)!r} is not a valid regex: {error}") from None
            terms.append(Term("regex", pattern))
            position = regex.end()
        else:
            rest = source[position:].split("|")[0].strip() or source[position:]
            raise ValueError(
                f"term {rest!r} is not test(=<test path>) or test(/<regex>/); "
                "no other nextest filter is accepted"
            )
        if position == len(source):
            return terms
        separator = _SEPARATOR.match(source, position)
        if not separator or separator.end() == position:
            raise ValueError(f"expected ' | ' after term {terms[-1].text()!r}")
        position = separator.end()


def filter_text(terms: list[Term]) -> str:
    return " | ".join(term.text() for term in terms)


def filter_matches(terms: list[Term], name: str) -> bool:
    return any(term.matches(name) for term in terms)


# ── Targets and binary ids ──────────────────────────────────────────────────────────────────


def binary_id_for(package: str, target: str) -> str:
    """nextest's binary id for a package's target: `pkg`, `pkg::<test>` or `pkg::bin/<bin>`."""
    if target == "lib":
        return package
    kind, _, name = target.partition(":")
    return f"{package}::{name}" if kind == "test" else f"{package}::bin/{name}"


def target_for_binary_id(binary_id: str) -> tuple[str, str]:
    """The (package, target) a binary id names, or ValueError for one cargo cannot select alone."""
    package, separator, rest = binary_id.partition("::")
    if not PACKAGE_RE.fullmatch(package):
        raise ValueError(f"binary id {binary_id!r} does not start with a package name")
    if not separator:
        return package, "lib"
    if rest.startswith("bin/") and PACKAGE_RE.fullmatch(rest[4:]):
        return package, f"bin:{rest[4:]}"
    if PACKAGE_RE.fullmatch(rest):
        return package, f"test:{rest}"
    raise ValueError(f"binary id {binary_id!r} is not a lib, an integration test or a bin")


def cargo_target_args(target: str) -> list[str]:
    if target == "lib":
        return ["--lib"]
    kind, _, name = target.partition(":")
    return [f"--{kind}", name]


# ── Classification ──────────────────────────────────────────────────────────────────────────


@dataclass
class IsolationSet:
    name: str
    package: str
    target: str
    filter: str
    terms: list[Term]
    needs: list[str]
    klass: str
    why: str
    features: list[str] = field(default_factory=list)

    @property
    def binary_id(self) -> str:
        return binary_id_for(self.package, self.target)

    def holds(self, binary_id: str, name: str) -> bool:
        return binary_id == self.binary_id and filter_matches(self.terms, name)

    def as_json(self) -> dict:
        return {
            "name": self.name,
            "package": self.package,
            "target": self.target,
            "binary_id": self.binary_id,
            "filter": self.filter,
            "needs": list(self.needs),
            "class": self.klass,
            "why": self.why,
            "features": list(self.features),
        }


@dataclass
class Classification:
    path: str
    sha256: str
    sets: list[IsolationSet]

    def by_name(self, name: str) -> IsolationSet | None:
        return next((s for s in self.sets if s.name == name), None)

    def set_of(self, binary_id: str, name: str) -> str | None:
        return next((s.name for s in self.sets if s.holds(binary_id, name)), None)


def _set_problem(path: str, index: int, raw: dict, key: str, value: object, why: str) -> Refusal:
    name = raw.get("name") if isinstance(raw.get("name"), str) else None
    which = f'set "{name}"' if name else f"set #{index + 1}"
    return Refusal(f"{path}: {which}: {key} {json.dumps(value)}: {why}")


def validate_classification(data: object, path: str) -> list[IsolationSet]:
    """The sets of a parsed classification, or a Refusal naming the file, the set and the value."""
    if not isinstance(data, dict):
        raise Refusal(f"{path}: the classification is not a table")
    unknown = sorted(set(data) - {"version", "set"})
    if unknown:
        raise Refusal(f"{path}: unknown top-level key {json.dumps(unknown[0])}: only version and [[set]]")
    if data.get("version") != CLASSIFICATION_VERSION:
        raise Refusal(
            f"{path}: version {json.dumps(data.get('version'))}: expected {CLASSIFICATION_VERSION}"
        )
    raw_sets = data.get("set")
    if not isinstance(raw_sets, list) or not raw_sets:
        raise Refusal(f"{path}: no [[set]] tables: a classification names at least one set")
    sets: list[IsolationSet] = []
    seen: set[str] = set()
    for index, raw in enumerate(raw_sets):
        if not isinstance(raw, dict):
            raise Refusal(f"{path}: set #{index + 1} is not a table")
        for key in raw:
            if key not in SET_KEYS_REQUIRED + SET_KEYS_OPTIONAL:
                raise _set_problem(path, index, raw, key, raw[key], "not a set field")
        for key in SET_KEYS_REQUIRED:
            if key not in raw:
                raise _set_problem(path, index, raw, key, None, "missing; every set gives it")
        name = raw["name"]
        if not isinstance(name, str) or not NAME_RE.fullmatch(name):
            raise _set_problem(path, index, raw, "name", name, "must match [a-z0-9-]+")
        if name in seen:
            raise _set_problem(path, index, raw, "name", name, "another set already has this name")
        seen.add(name)
        package = raw["package"]
        if not isinstance(package, str) or not PACKAGE_RE.fullmatch(package):
            raise _set_problem(path, index, raw, "package", package, "not a cargo package name")
        target = raw["target"]
        if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
            raise _set_problem(path, index, raw, "target", target, "must be lib, test:<name> or bin:<name>")
        try:
            terms = parse_filter(raw["filter"])
        except ValueError as error:
            raise _set_problem(path, index, raw, "filter", raw["filter"], str(error)) from None
        needs = raw["needs"]
        if not isinstance(needs, list) or not needs:
            raise _set_problem(path, index, raw, "needs", needs, "must be a non-empty list")
        for need in needs:
            if need not in NEEDS:
                raise _set_problem(
                    path, index, raw, "needs", need, f"not one of {', '.join(NEEDS)}"
                )
        if len(set(needs)) != len(needs):
            raise _set_problem(path, index, raw, "needs", needs, "lists a need twice")
        klass = raw["class"]
        if klass not in CLASSES:
            raise _set_problem(path, index, raw, "class", klass, f"must be {' or '.join(CLASSES)}")
        why = raw["why"]
        if not isinstance(why, str) or not why.strip():
            raise _set_problem(path, index, raw, "why", why, "must be a non-empty sentence")
        features = raw.get("features", [])
        if not isinstance(features, list) or not all(
            isinstance(f, str) and FEATURE_RE.fullmatch(f) for f in features
        ):
            raise _set_problem(path, index, raw, "features", features, "must be a list of feature names")
        sets.append(
            IsolationSet(
                name=name,
                package=package,
                target=target,
                filter=raw["filter"].strip(),
                terms=terms,
                needs=list(needs),
                klass=klass,
                why=why.strip(),
                features=list(features),
            )
        )
    return sets


def load_classification(path: str) -> Classification:
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except OSError as error:
        raise Refusal(f"{path}: the classification cannot be read: {error.strerror or error}") from None
    try:
        data = tomllib.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise Refusal(f"{path}: not valid TOML: {error}") from None
    sets = validate_classification(data, path)
    return Classification(path=os.path.abspath(path), sha256=hashlib.sha256(raw).hexdigest(), sets=sets)


# ── JUnit reports ───────────────────────────────────────────────────────────────────────────


@dataclass
class Case:
    binary_id: str
    name: str
    status: str
    time_secs: float | None
    detail: str | None
    skip_line: str | None


def host_skip_line(text: str) -> str | None:
    """The first host-skip line in a test's output, or None."""
    for line in text.splitlines():
        stripped = line.strip()
        if HOST_SKIP_MARK in stripped or UNPREFIXED_SKIP_RE.match(stripped):
            return clip(stripped)
    return None


def failure_line(case: ElementTree.Element, failures: list[ElementTree.Element]) -> str:
    """The first line that says why a test failed: the panic and its message, when there is one."""
    texts = ["".join(f.itertext()) for f in failures]
    for tag in ("system-err", "system-out"):
        texts += ["".join(e.itertext()) for e in case.iter(tag)]
    for text in texts:
        lines = [line.strip() for line in text.splitlines()]
        for index, line in enumerate(lines):
            if " panicked at " in line or line.startswith("panicked at "):
                rest = next((later for later in lines[index + 1 :] if later), "")
                return clip(f"{line} {rest}".strip())
    for failure in failures:
        message = failure.get("message")
        if message:
            return clip(message)
    for text in texts:
        first = next((line.strip() for line in text.splitlines() if line.strip()), None)
        if first:
            return clip(first)
    return clip(failures[0].get("type") or failures[0].tag)


def parse_junit(path: str) -> dict[tuple[str, str], Case]:
    """Every test case of a nextest JUnit report, keyed by (binary id, test name)."""
    try:
        root = ElementTree.parse(path).getroot()
    except (OSError, ElementTree.ParseError) as error:
        raise Refusal(f"{path}: the JUnit report cannot be read: {error}") from None
    cases: dict[tuple[str, str], Case] = {}
    for element in root.iter("testcase"):
        binary_id = element.get("classname")
        name = element.get("name")
        if not binary_id or not name:
            raise Refusal(f"{path}: a testcase lacks its classname or name")
        failures = [child for child in element if child.tag in FAILURE_TAGS]
        text = "".join(element.itertext())
        skip = host_skip_line(text)
        try:
            time_secs = round(float(element.get("time", "")), 3)
        except ValueError:
            time_secs = None
        if failures:
            status, detail = "failed", failure_line(element, failures)
        elif any(child.tag == "skipped" for child in element):
            status, detail = "not_run", "nextest skipped it"
        elif skip:
            status, detail = "skipped_for_host", skip
        else:
            status, detail = "passed", None
        cases[(binary_id, name)] = Case(binary_id, name, status, time_secs, detail, skip)
    return cases


# ── Verdicts and lines ──────────────────────────────────────────────────────────────────────


def totals_of(tests: list[dict]) -> dict[str, int]:
    return {status: sum(1 for t in tests if t["status"] == status) for status in STATUSES}


def verdict_of(totals: dict[str, int]) -> str:
    """`fail` beats `no_evidence`, which beats `pass`; no test at all is no evidence."""
    if totals["failed"]:
        return "fail"
    if totals["skipped_for_host"] or totals["not_run"] or not totals["passed"]:
        return "no_evidence"
    return "pass"


def test_line(test: dict) -> str:
    where = f"{test['binary_id']} {test['name']}"
    status = test["status"]
    if status == "passed":
        return f"PASS {where}"
    if status == "failed":
        return f"FAIL {where}: {test.get('detail') or 'failed'}"
    if status == "skipped_for_host":
        return f"NO-EVIDENCE {where}: skipped for the host: {test.get('detail') or ''}".rstrip()
    return f"NO-EVIDENCE {where}: not run" + (
        f" ({test['detail']})" if test.get("detail") and test["detail"] != "not run" else ""
    )


# ── Environment and host ────────────────────────────────────────────────────────────────────


def filter_environment(environ: dict[str, str]) -> tuple[dict[str, str], list[str]]:
    """The allowlisted part of `environ`, and the sorted names of everything dropped."""
    kept: dict[str, str] = {}
    dropped: list[str] = []
    for name, value in environ.items():
        upper = name.upper()
        allowed = name in ENV_ALLOWED or (
            name.startswith(ENV_ALLOWED_PREFIXES)
            and not any(word in upper for word in ENV_SECRET_WORDS)
        )
        if allowed:
            kept[name] = value
        else:
            dropped.append(name)
    return kept, sorted(dropped)


def status_field(status_text: str, key: str) -> str | None:
    for line in status_text.splitlines():
        if line.startswith(f"{key}:"):
            return line.split(":", 1)[1].strip()
    return None


def host_refusal(uid: int, status_text: str | None) -> str | None:
    """Why this process must not run isolation tests, or None.

    As root a test proves what root can do, not what a capsule's user can. Under a seccomp
    filter or `no_new_privs` the process is already inside a sandbox, and an isolation test run
    there proves the outer sandbox, not the code.
    """
    if uid == 0:
        return (
            "running as root (uid 0): isolation tests run as root prove what root can do, not "
            "what a capsule's unprivileged user can; run as the operator's own user"
        )
    if status_text is None:
        return "/proc/self/status cannot be read, so whether this process is sandboxed is unknown"
    no_new_privs = status_field(status_text, "NoNewPrivs")
    seccomp = status_field(status_text, "Seccomp")
    if no_new_privs is None or seccomp is None:
        return "/proc/self/status shows no NoNewPrivs or Seccomp line, so whether this process is sandboxed is unknown"
    if no_new_privs != "0" or seccomp != "0":
        return (
            f"this process runs inside a sandbox (NoNewPrivs: {no_new_privs}, Seccomp: {seccomp}): "
            "an isolation test run inside a sandbox proves the outer sandbox, not the code; run "
            "the runner on the host, outside any capsule"
        )
    return None


def read_proc_status() -> str | None:
    try:
        with open("/proc/self/status", encoding="utf-8") as handle:
            return handle.read()
    except OSError:
        return None


def virtualization() -> str:
    """`systemd-detect-virt`'s answer, recorded as information: `none`, a name, or `unknown`."""
    if not shutil.which("systemd-detect-virt"):
        return "unknown"
    try:
        done = subprocess.run(
            ["systemd-detect-virt"], capture_output=True, text=True, timeout=10, check=False
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
    return done.stdout.strip() or "unknown"


# ── git ─────────────────────────────────────────────────────────────────────────────────────


def git_env() -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k not in GIT_ENV_DROPPED}
    env["LC_ALL"] = "C"
    return env


def git(repo: str, *args: str, binary: bool = False) -> bytes | str:
    try:
        done = subprocess.run(
            ["git", "-C", repo, *args], capture_output=True, env=git_env(), check=False
        )
    except OSError as error:
        raise Refusal(f"git cannot run: {error}") from None
    if done.returncode != 0:
        message = done.stderr.decode("utf-8", "replace").strip().splitlines()
        raise Refusal(f"{repo}: git {' '.join(args)} failed: {message[0] if message else done.returncode}")
    return done.stdout if binary else done.stdout.decode("utf-8", "replace").strip()


def tree_state(repo: str) -> tuple[str, dict]:
    """The repo's commit and tree: clean, or the sha256 of `git diff HEAD` (untracked files aside)."""
    if git(repo, "rev-parse", "--is-inside-work-tree") != "true":
        raise Refusal(f"{repo} is not inside a git work tree")
    commit = git(repo, "rev-parse", "HEAD")
    porcelain = git(repo, "status", "--porcelain", "--untracked-files=no")
    if not porcelain:
        return commit, {"clean": True, "diff_sha256": None}
    diff = git(repo, *GIT_DIFF_ARGV, binary=True)
    return commit, {"clean": False, "diff_sha256": hashlib.sha256(diff).hexdigest()}


def repo_root(repo: str | None) -> str:
    where = os.path.abspath(repo or os.getcwd())
    if not os.path.isdir(where):
        raise Refusal(f"{where} is not a directory")
    try:
        done = subprocess.run(
            ["git", "-C", where, "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            env=git_env(),
            check=False,
        )
    except OSError as error:
        raise Refusal(f"git cannot run: {error}") from None
    if done.returncode != 0:
        raise Refusal(f"{where} is not inside a git repository")
    return done.stdout.strip()


def default_repo_for_files() -> str:
    """Where list and handover look for the default classification: the current repository."""
    try:
        return repo_root(None)
    except Refusal:
        return os.getcwd()


# ── Evidence files ──────────────────────────────────────────────────────────────────────────


def remove_evidence(evidence: str) -> None:
    """Remove the evidence file and the JUnit copies a previous run left beside it."""
    directory, stem = os.path.split(os.path.abspath(evidence))
    stem = os.path.splitext(stem)[0]
    targets = [os.path.abspath(evidence)]
    if os.path.isdir(directory):
        prefix = f"{stem}.junit-"
        targets += [
            os.path.join(directory, entry)
            for entry in os.listdir(directory)
            if entry.startswith(prefix) and entry.endswith(".xml")
        ]
    for target in targets:
        try:
            if os.path.isdir(target) and not os.path.islink(target):
                raise Refusal(f"{target} is a directory, not an evidence file")
            os.unlink(target)
        except FileNotFoundError:
            pass
        except OSError as error:
            raise Refusal(f"{target} cannot be removed: {error.strerror or error}") from None


def write_json_atomically(path: str, record: dict) -> None:
    """Write `record` to a temp file in `path`'s directory and rename it into place."""
    directory = os.path.dirname(os.path.abspath(path))
    handle = tempfile.NamedTemporaryFile(
        "w",
        encoding="utf-8",
        dir=directory,
        prefix=f".{os.path.basename(path)}.",
        suffix=".tmp",
        delete=False,
    )
    try:
        with handle:
            json.dump(record, handle, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(handle.name, path)
    except BaseException:
        try:
            os.unlink(handle.name)
        except OSError:
            pass
        raise


# ── Selection ───────────────────────────────────────────────────────────────────────────────


@dataclass
class Group:
    """One nextest invocation: a package's target, its features and the terms that select tests."""

    package: str
    target: str
    features: list[str]
    terms: list[Term]
    # The set each term came from (None for an unclassified handover test).
    term_sets: list[str | None]

    @property
    def binary_id(self) -> str:
        return binary_id_for(self.package, self.target)


def load_handover(path: str) -> dict:
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
        data = json.loads(raw)
    except OSError as error:
        raise Refusal(f"{path}: the handover cannot be read: {error.strerror or error}") from None
    except (ValueError, UnicodeDecodeError) as error:
        raise Refusal(f"{path}: the handover is not JSON: {error}") from None
    if not isinstance(data, dict) or data.get("format") != HANDOVER_FORMAT:
        found = data.get("format") if isinstance(data, dict) else None
        raise Refusal(f"{path}: format {json.dumps(found)}: expected {HANDOVER_FORMAT}")
    tests = data.get("tests")
    if not isinstance(tests, list):
        raise Refusal(f"{path}: tests must be a list")
    for index, test in enumerate(tests):
        if (
            not isinstance(test, dict)
            or not isinstance(test.get("binary_id"), str)
            or not isinstance(test.get("name"), str)
            or not EXACT_NAME_RE.fullmatch(test["name"])
            or not (test.get("set") is None or isinstance(test.get("set"), str))
        ):
            raise Refusal(f"{path}: test #{index + 1} needs a binary_id, a test-path name and a set or null")
        try:
            target_for_binary_id(test["binary_id"])
        except ValueError as error:
            raise Refusal(f"{path}: test #{index + 1}: {error}") from None
    return {"data": data, "sha256": hashlib.sha256(raw).hexdigest()}


def select_sets(classification: Classification, names: list[str]) -> tuple[list[str], list[Group]]:
    groups: dict[tuple, Group] = {}
    chosen: list[str] = []
    for name in names:
        found = classification.by_name(name)
        if found is None:
            known = ", ".join(s.name for s in classification.sets)
            raise Refusal(f'unknown set "{name}" in {classification.path}; the known sets are: {known}')
        if name in chosen:
            continue
        chosen.append(name)
        key = (found.package, found.target, tuple(found.features))
        group = groups.setdefault(key, Group(found.package, found.target, list(found.features), [], []))
        for term in found.terms:
            group.terms.append(term)
            group.term_sets.append(name)
    if not chosen:
        raise Refusal("the selection is empty: name at least one set")
    return chosen, list(groups.values())


def select_handover(classification: Classification, handover: dict) -> tuple[list[str], list[Group]]:
    groups: dict[tuple, Group] = {}
    chosen: list[str] = []
    for test in handover["data"]["tests"]:
        package, target = target_for_binary_id(test["binary_id"])
        # Classify by the trusted classification, not by the handover's own `set`.
        set_name = classification.set_of(test["binary_id"], test["name"])
        found = classification.by_name(set_name) if set_name else None
        features = list(found.features) if found else []
        if set_name and set_name not in chosen:
            chosen.append(set_name)
        key = (package, target, tuple(features))
        group = groups.setdefault(key, Group(package, target, features, [], []))
        term = Term("exact", test["name"])
        if term not in group.terms:
            group.terms.append(term)
            group.term_sets.append(set_name)
    if not groups:
        raise Refusal("the selection is empty: the handover lists no test")
    return chosen, list(groups.values())


def tests_for_group(group: Group, cases: dict[tuple[str, str], Case] | None, why_absent: str) -> list[dict]:
    """Evidence rows for one invocation: every reported test, and a `not_run` row per term that
    matched nothing (an exact name absent from the report, or a regex no reported test matches)."""
    rows: list[dict] = []
    reported = [] if cases is None else [c for (b, _), c in cases.items() if b == group.binary_id]
    for case in reported:
        set_name = next(
            (s for term, s in zip(group.terms, group.term_sets) if term.matches(case.name)), None
        )
        rows.append(
            {
                "binary_id": case.binary_id,
                "name": case.name,
                "set": set_name,
                "status": case.status,
                "time_secs": case.time_secs,
                "detail": case.detail,
            }
        )
    for term, set_name in zip(group.terms, group.term_sets):
        if any(term.matches(case.name) for case in reported):
            continue
        rows.append(
            {
                "binary_id": group.binary_id,
                "name": term.value if term.kind == "exact" else term.text(),
                "set": set_name,
                "status": "not_run",
                "time_secs": None,
                "detail": why_absent if term.kind == "exact" else f"no test matched; {why_absent}",
            }
        )
    return rows


# ── Commands ────────────────────────────────────────────────────────────────────────────────


def cmd_list(args: argparse.Namespace) -> int:
    path = args.classification or os.path.join(default_repo_for_files(), DEFAULT_CLASSIFICATION)
    classification = load_classification(path)
    expanded: dict[str, list[dict]] = {}
    if args.expand:
        env, _ = filter_environment(dict(os.environ))
        for found in classification.sets:
            expanded[found.name] = expand_set(found, env)
    if args.json:
        sets = []
        for found in classification.sets:
            row = found.as_json()
            if args.expand:
                row["tests"] = expanded[found.name]
            sets.append(row)
        print(json.dumps({"format": CLASSIFICATION_FORMAT, "sets": sets}, indent=2))
        return 0
    print(f"{len(classification.sets)} isolation sets in {path}")
    for found in classification.sets:
        print(f"\n{found.name}")
        print(f"  package   {found.package}")
        print(f"  target    {found.target}  (binary id {found.binary_id})")
        print(f"  filter    {found.filter}")
        print(f"  needs     {', '.join(found.needs)}")
        print(f"  class     {found.klass}")
        print(f"  features  {', '.join(found.features) if found.features else '-'}")
        print(f"  why       {found.why}")
        if args.expand:
            for test in expanded[found.name]:
                print(f"  test      {test['binary_id']} {test['name']}")
    return 0


def expand_set(found: IsolationSet, env: dict[str, str]) -> list[dict]:
    argv = ["cargo", "nextest", "list", "--message-format", "json", "-p", found.package]
    argv += cargo_target_args(found.target)
    if found.features:
        argv += ["--features", ",".join(found.features)]
    argv += ["-E", found.filter]
    print(f"$ {' '.join(argv)}", file=sys.stderr, flush=True)
    try:
        done = subprocess.run(argv, stdout=subprocess.PIPE, env=env, text=True, check=False)
    except OSError as error:
        raise Refusal(f"cargo cannot run: {error}") from None
    if done.returncode != 0:
        raise Refusal(f'set "{found.name}": cargo nextest list exited {done.returncode}')
    try:
        listing = json.loads(done.stdout)
    except ValueError as error:
        raise Refusal(f'set "{found.name}": cargo nextest list printed no JSON: {error}') from None
    tests = []
    for binary_id, suite in sorted(listing.get("rust-suites", {}).items()):
        for name, case in sorted(suite.get("testcases", {}).items()):
            matched = case.get("filter-match", {}).get("status") == "matches"
            if matched and found.holds(binary_id, name):
                tests.append({"binary_id": binary_id, "name": name})
    if not tests:
        raise Refusal(f'set "{found.name}": filter {found.filter} selects no test in {found.binary_id}')
    return tests


def cmd_handover(args: argparse.Namespace) -> int:
    path = args.classification or os.path.join(default_repo_for_files(), DEFAULT_CLASSIFICATION)
    classification = load_classification(path)
    cases = parse_junit(args.junit)
    tests = []
    for case in cases.values():
        if case.skip_line is None:
            continue
        tests.append(
            {
                "binary_id": case.binary_id,
                "name": case.name,
                "line": case.skip_line,
                "set": classification.set_of(case.binary_id, case.name),
            }
        )
    unclassified = sum(1 for t in tests if t["set"] is None)
    record = {
        "format": HANDOVER_FORMAT,
        "source": {"path": os.path.abspath(args.junit), "sha256": sha256_file(args.junit)},
        "tests": tests,
        "unclassified": unclassified,
    }
    out_dir = os.path.dirname(os.path.abspath(args.out))
    os.makedirs(out_dir, exist_ok=True)
    write_json_atomically(args.out, record)
    for test in tests:
        print(f"HANDOVER {test['binary_id']} {test['name']} [{test['set'] or 'unclassified'}]: {test['line']}")
    print(
        f"{len(tests)} test{'s' if len(tests) != 1 else ''} printed a host-skip line "
        f"({unclassified} unclassified); written to {args.out}"
    )
    return 0


@dataclass
class RunContext:
    """What `run` injects, so the refusal logic is testable without a host or cargo."""

    uid: int
    proc_status: str | None
    environ: dict[str, str]
    has_command: object  # callable(name) -> bool


def default_context() -> RunContext:
    return RunContext(
        uid=os.getuid(),
        proc_status=read_proc_status(),
        environ=dict(os.environ),
        has_command=lambda name: shutil.which(name) is not None,
    )


def nextest_available(env: dict[str, str]) -> bool:
    try:
        done = subprocess.run(
            ["cargo", "nextest", "--version"], capture_output=True, env=env, check=False, timeout=60
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return done.returncode == 0


def run_logged(argv: list[str], env: dict[str, str], cwd: str, capture: bool = False) -> subprocess.CompletedProcess:
    print(f"$ {' '.join(argv)}", flush=True)
    try:
        return subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            check=False,
            stdout=subprocess.PIPE if capture else None,
            stderr=subprocess.STDOUT if capture else None,
            text=True,
        )
    except OSError as error:
        return subprocess.CompletedProcess(argv, 127, stdout=f"{argv[0]} cannot run: {error}")


def resolve_target_dir(args_target: str | None, repo: str, env: dict[str, str]) -> str:
    if args_target:
        return os.path.abspath(args_target)
    done = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if done.returncode != 0:
        raise Refusal(f"cargo metadata failed in {repo}: {done.stderr.strip().splitlines()[-1:]}")
    return json.loads(done.stdout)["target_directory"]


def junit_location(nextest_config: str, repo: str) -> str:
    """Where `cargo nextest run --profile isolation` writes its JUnit report under this config.

    nextest resolves `[store] dir` (default `target/nextest`) against the workspace root, not
    cargo's target directory, and the report sits at `<store>/<profile>/<junit path>`.
    """
    try:
        with open(nextest_config, "rb") as handle:
            config = tomllib.load(handle)
    except OSError as error:
        raise Refusal(f"{nextest_config}: the nextest config cannot be read: {error.strerror or error}") from None
    except tomllib.TOMLDecodeError as error:
        raise Refusal(f"{nextest_config}: not valid TOML: {error}") from None
    store = config.get("store", {}).get("dir", os.path.join("target", "nextest"))
    junit = config.get("profile", {}).get(NEXTEST_PROFILE, {}).get("junit", {}).get("path")
    if not isinstance(store, str) or not isinstance(junit, str) or not junit:
        raise Refusal(
            f"{nextest_config}: no [profile.{NEXTEST_PROFILE}.junit] path, so nextest would write "
            "no report to read"
        )
    return os.path.join(repo, store, NEXTEST_PROFILE, junit)


def cmd_run(args: argparse.Namespace, context: RunContext | None = None) -> int:
    evidence = os.path.abspath(args.evidence)
    # Invariant: a refused run leaves no evidence, so the first act is to remove any.
    remove_evidence(evidence)
    context = context or default_context()

    refusal = host_refusal(context.uid, context.proc_status)
    if refusal:
        raise Refusal(refusal)
    repo = repo_root(args.repo)
    classification = load_classification(
        args.classification or os.path.join(repo, DEFAULT_CLASSIFICATION)
    )
    handover = None
    if args.handover:
        handover = load_handover(args.handover)
        chosen, groups = select_handover(classification, handover)
    elif args.all_sets:
        chosen, groups = select_sets(classification, [s.name for s in classification.sets])
    else:
        chosen, groups = select_sets(classification, args.set or [])
    nextest_config = os.path.abspath(args.nextest_config or os.path.join(repo, DEFAULT_NEXTEST_CONFIG))
    if not os.path.isfile(nextest_config):
        raise Refusal(f"{nextest_config}: no nextest config there")
    junit_written = junit_location(nextest_config, repo)
    preflight = os.path.abspath(args.preflight_script or os.path.join(repo, DEFAULT_PREFLIGHT))
    if not os.path.isfile(preflight):
        raise Refusal(f"{preflight}: no preflight script there")
    commit, tree = tree_state(repo)

    env, dropped = filter_environment(context.environ)
    if args.target_dir:
        env["CARGO_TARGET_DIR"] = os.path.abspath(args.target_dir)
    if not context.has_command("cargo") or not nextest_available(env):
        raise Refusal("cargo nextest is not installed: `cargo install cargo-nextest --locked`")
    use_scope = not args.no_scope and context.has_command("systemd-run")
    scope = list(SCOPE_ARGV) if use_scope else []
    target_dir = resolve_target_dir(args.target_dir, repo, env)

    classes = [classification.by_name(n).klass for n in chosen] or [CLASSES[0]]
    klass = max(classes, key=CLASSES.index)
    print(f"isolation-proofs: repo {repo} at {commit} ({'clean' if tree['clean'] else 'changed tree'})")
    print(f"isolation-proofs: sets {', '.join(chosen) or '(unclassified handover tests only)'}; class {klass}")
    print(f"isolation-proofs: dropped from the environment: {', '.join(dropped) or 'nothing'}")
    print(f"isolation-proofs: {'inside a delegated systemd scope' if use_scope else 'without a systemd scope'}")

    build = run_logged(scope + ["cargo", "build", "-p", "murmur-cli", "--bin", "mur"], env, repo)
    if build.returncode != 0:
        raise Refusal(f"cargo build -p murmur-cli --bin mur exited {build.returncode}; nothing ran")
    mur = os.path.join(target_dir, "debug", "mur")
    with tempfile.TemporaryDirectory(prefix="isolation-preflight-home-") as scratch_home:
        preflight_env = dict(env, HOME=scratch_home)
        checked = run_logged(
            scope + [sys.executable, "-I", preflight, "--class", klass, "--mur", mur],
            preflight_env,
            repo,
            capture=True,
        )
    preflight_output = (checked.stdout or "").strip()
    print(preflight_output)
    if checked.returncode != 0:
        raise Refusal(
            f"the preflight says this host does not reach {klass} (exit {checked.returncode}); "
            f"nothing ran. Its output:\n{preflight_output}"
        )

    directory, base = os.path.split(evidence)
    stem = os.path.splitext(base)[0]
    os.makedirs(directory, exist_ok=True)
    runs: list[dict] = []
    tests: list[dict] = []
    for number, group in enumerate(groups, start=1):
        argv = [
            "cargo",
            "nextest",
            "run",
            "--config-file",
            nextest_config,
            "--profile",
            NEXTEST_PROFILE,
            "--retries",
            "0",
            "--no-fail-fast",
            "-p",
            group.package,
            *cargo_target_args(group.target),
        ]
        if group.features:
            argv += ["--features", ",".join(group.features)]
        expression = filter_text(group.terms)
        argv += ["-E", expression]
        try:
            os.unlink(junit_written)
        except FileNotFoundError:
            pass
        done = run_logged(scope + argv, env, repo)
        junit = None
        cases = None
        if os.path.isfile(junit_written):
            copy = os.path.join(directory, f"{stem}.junit-{number}.xml")
            shutil.copyfile(junit_written, copy)
            junit = {"path": copy, "sha256": sha256_file(copy)}
            cases = parse_junit(copy)
        why_absent = (
            "absent from the JUnit report"
            if junit
            else f"nextest wrote no JUnit report (exit {done.returncode})"
        )
        runs.append(
            {
                "package": group.package,
                "target": group.target,
                "filter": expression,
                "argv": scope + argv,
                "exit": done.returncode,
                "junit": junit,
            }
        )
        tests += tests_for_group(group, cases, why_absent)

    totals = totals_of(tests)
    verdict = verdict_of(totals)
    record = {
        "format": EVIDENCE_FORMAT,
        "written_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "repo": repo,
        "commit": commit,
        "tree": tree,
        "classification": {"path": classification.path, "sha256": classification.sha256},
        "request": {
            "sets": chosen,
            "handover": (
                {"path": os.path.abspath(args.handover), "sha256": handover["sha256"]}
                if handover
                else None
            ),
        },
        "host": {
            "kernel": platform.release(),
            "virtualization": virtualization(),
            "uid": context.uid,
            "scope": "systemd-run" if use_scope else "none",
            "preflight": {
                "script": preflight,
                "class": klass,
                "exit": checked.returncode,
                "output": preflight_output,
            },
        },
        "runs": runs,
        "tests": tests,
        "totals": totals,
        "verdict": verdict,
    }
    write_json_atomically(evidence, record)
    for test in tests:
        print(test_line(test))
    print(
        f"VERDICT: {verdict} ({totals['passed']} passed, {totals['failed']} failed, "
        f"{totals['skipped_for_host']} skipped for the host, {totals['not_run']} not run); "
        f"evidence {evidence}"
    )
    return VERDICT_EXIT[verdict]


class Unusable(Exception):
    """Why the judge cannot read the evidence as evidence of this tree: exit 2."""


def check_evidence_shape(record: object) -> dict:
    if not isinstance(record, dict):
        raise Unusable("the evidence is not a JSON object")
    if record.get("format") != EVIDENCE_FORMAT:
        raise Unusable(f"format {json.dumps(record.get('format'))}: expected {EVIDENCE_FORMAT}")
    for key, kind in (
        ("commit", str),
        ("tree", dict),
        ("classification", dict),
        ("request", dict),
        ("runs", list),
        ("tests", list),
    ):
        if not isinstance(record.get(key), kind):
            raise Unusable(f"the evidence has no {key}")
    for test in record["tests"]:
        if (
            not isinstance(test, dict)
            or not isinstance(test.get("binary_id"), str)
            or not isinstance(test.get("name"), str)
            or test.get("status") not in STATUSES
        ):
            raise Unusable(f"the evidence holds a malformed test row: {json.dumps(test)[:200]}")
    return record


def cmd_judge(args: argparse.Namespace) -> int:
    try:
        return judge(args)
    except Unusable as error:
        print(f"JUDGE: unusable evidence: {error}")
        return EXIT_REFUSED


def judge(args: argparse.Namespace) -> int:
    try:
        with open(args.evidence, encoding="utf-8") as handle:
            record = json.load(handle)
    except OSError as error:
        raise Unusable(f"{args.evidence} cannot be read: {error.strerror or error}") from None
    except (ValueError, UnicodeDecodeError) as error:
        raise Unusable(f"{args.evidence} is not JSON: {error}") from None
    record = check_evidence_shape(record)

    try:
        repo = repo_root(args.repo)
        commit, tree = tree_state(repo)
    except Refusal as error:
        raise Unusable(str(error)) from None
    if commit != record["commit"]:
        raise Unusable(f"the evidence was taken at commit {record['commit']}, but {repo} is at {commit}")
    if bool(tree["clean"]) != bool(record["tree"].get("clean")) or tree["diff_sha256"] != record["tree"].get(
        "diff_sha256"
    ):
        raise Unusable("the tree changed since the evidence was taken")

    # Every JUnit report must still hold the bytes the runner hashed, and every row must say
    # what its report says: an edited status is caught here, not believed.
    reported: dict[tuple[str, str], Case] = {}
    for run in record["runs"]:
        junit = run.get("junit") if isinstance(run, dict) else None
        if junit is None:
            continue
        path, recorded = junit.get("path"), junit.get("sha256")
        if not isinstance(path, str) or not os.path.isfile(path):
            raise Unusable(f"the JUnit report {path} is missing")
        if sha256_file(path) != recorded:
            raise Unusable(f"the JUnit report {path} no longer matches its recorded sha256")
        try:
            reported.update(parse_junit(path))
        except Refusal as error:
            raise Unusable(str(error)) from None
    for test in record["tests"]:
        case = reported.get((test["binary_id"], test["name"]))
        expected = case.status if case else "not_run"
        if expected != test["status"]:
            raise Unusable(
                f"the evidence says {test['binary_id']} {test['name']} {test['status']}, "
                f"but its JUnit report says {expected}"
            )

    required_sets = args.require_set or []
    required_tests = args.require_test or []
    missing: list[str] = []
    if required_sets:
        path = args.classification or os.path.join(repo, DEFAULT_CLASSIFICATION)
        try:
            classification = load_classification(path)
        except Refusal as error:
            raise Unusable(str(error)) from None
        if classification.sha256 != record["classification"].get("sha256"):
            raise Unusable(
                f"the evidence was taken with a different classification than {path} "
                f"(sha256 {record['classification'].get('sha256')}, here {classification.sha256})"
            )
        for name in required_sets:
            found = classification.by_name(name)
            if found is None:
                known = ", ".join(s.name for s in classification.sets)
                raise Unusable(f'unknown set "{name}" in {path}; the known sets are: {known}')
            requested = record["request"].get("sets") or []
            members = [t for t in record["tests"] if t.get("set") == name]
            if name not in requested or not members:
                missing.append(f"MISSING set {name}: required but not in the evidence")
                continue
            for term in found.terms:
                if not any(term.matches(t["name"]) and t["binary_id"] == found.binary_id for t in members):
                    what = f"{found.binary_id}::{term.value}" if term.kind == "exact" else term.text()
                    missing.append(f"MISSING test {what} (set {name}): required but not in the evidence")
    for spec in required_tests:
        if not any(f"{t['binary_id']}::{t['name']}" == spec for t in record["tests"]):
            missing.append(f"MISSING test {spec}: required but not in the evidence")

    for test in record["tests"]:
        print(test_line(test))
    for line in missing:
        print(line)
    totals = totals_of(record["tests"])
    no_evidence = totals["skipped_for_host"] + totals["not_run"]
    if not totals["failed"] and not no_evidence and not missing and totals["passed"]:
        print(f"JUDGE: pass ({totals['passed']} tests)")
        return EXIT_PASS
    counts = [f"{totals['passed']} passed", f"{totals['failed']} failed", f"{no_evidence} no evidence"]
    if missing:
        counts.append(f"{len(missing)} missing")
    if not record["tests"]:
        counts.append("the evidence holds no test")
    print(f"JUDGE: fail ({', '.join(counts)})")
    return EXIT_FAIL


# ── Entry point ─────────────────────────────────────────────────────────────────────────────


class Parser(argparse.ArgumentParser):
    """argparse, but a usage error is a refusal (exit 2) printed as one line."""

    def error(self, message: str) -> None:  # type: ignore[override]
        raise Refusal(f"{self.prog}: {message}")


def parser() -> argparse.ArgumentParser:
    top = Parser(prog="isolation-proofs.py", description=__doc__.splitlines()[0])
    commands = top.add_subparsers(dest="command", required=True, parser_class=Parser)

    listing = commands.add_parser("list", help="validate the classification and print its sets")
    listing.add_argument("--classification", help=f"default: <repo>/{DEFAULT_CLASSIFICATION}")
    listing.add_argument("--json", action="store_true", help=f"print {CLASSIFICATION_FORMAT}")
    listing.add_argument("--expand", action="store_true", help="add each set's tests (builds them)")

    handing = commands.add_parser("handover", help="list the tests a JUnit report shows skipped for the host")
    handing.add_argument("junit", help="a nextest JUnit report with passing tests' output stored")
    handing.add_argument("--out", required=True, help=f"where to write {HANDOVER_FORMAT}")
    handing.add_argument("--classification", help=f"default: <repo>/{DEFAULT_CLASSIFICATION}")

    running = commands.add_parser("run", help="run isolation sets on this host and write evidence")
    selection = running.add_mutually_exclusive_group(required=True)
    selection.add_argument("--set", action="append", metavar="NAME", help="a set to run; repeatable")
    selection.add_argument("--all-sets", action="store_true", help="every set in the classification")
    selection.add_argument("--handover", metavar="FILE", help=f"the tests a {HANDOVER_FORMAT} file lists")
    running.add_argument("--evidence", required=True, help=f"where to write {EVIDENCE_FORMAT}")
    running.add_argument("--repo", help="the tree under test (default: the current directory)")
    running.add_argument("--classification", help=f"default: <repo>/{DEFAULT_CLASSIFICATION}")
    running.add_argument("--nextest-config", help=f"default: <repo>/{DEFAULT_NEXTEST_CONFIG}")
    running.add_argument("--preflight-script", help=f"default: <repo>/{DEFAULT_PREFLIGHT}")
    running.add_argument("--target-dir", help="cargo's target directory (default: cargo's own)")
    running.add_argument("--no-scope", action="store_true", help="do not wrap commands in a systemd scope")

    judging = commands.add_parser("judge", help="judge evidence against this tree")
    judging.add_argument("evidence")
    judging.add_argument("--repo", help="the tree the evidence must match (default: the current directory)")
    judging.add_argument("--require-set", action="append", metavar="NAME")
    judging.add_argument("--require-test", action="append", metavar="BINARY_ID::TEST")
    judging.add_argument("--classification", help=f"default: <repo>/{DEFAULT_CLASSIFICATION}")
    return top


def main(argv: list[str] | None = None, context: RunContext | None = None) -> int:
    try:
        args = parser().parse_args(argv)
        if args.command == "list":
            return cmd_list(args)
        if args.command == "handover":
            return cmd_handover(args)
        if args.command == "run":
            return cmd_run(args, context)
        return cmd_judge(args)
    except Refusal as error:
        print(f"isolation-proofs: refused: {error}", file=sys.stderr, flush=True)
        return EXIT_REFUSED
    except KeyboardInterrupt:
        print("isolation-proofs: interrupted", file=sys.stderr, flush=True)
        return EXIT_REFUSED


if __name__ == "__main__":
    sys.exit(main())
