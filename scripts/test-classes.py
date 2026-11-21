#!/usr/bin/env python3
"""Sort Murmur's tests by where each can run, and check that every test is sorted.

Every test has one class and one kind:

  class  capsule    runs as a command inside a contained Murmur capsule
         isolation  needs kernel isolation an unprivileged user gets on a prepared host
         host       needs the host itself: root, a sysctl, a browser, egress, the real home
  kind   unit       a lib, bin or proc-macro test target
         e2e        an integration-test binary that runs `mur` or `mur-roost`
         integration every other integration-test binary

The class rules are the `default-filter` of the `class-capsule`, `class-isolation` and
`class-host` profiles in .config/nextest.toml. The `e2e` kind filterset, what each class needs
and the nextest configurations the list covers are in .config/test-classes.toml. The list itself,
.config/test-classes.tsv, is written only by `generate`.

    python3 -I scripts/test-classes.py generate
    python3 -I scripts/test-classes.py check [--strict]
    python3 -I scripts/test-classes.py list [--class C] [--kind K] [--json] [--live]
    python3 -I scripts/test-classes.py filter <class>
    python3 -I scripts/test-classes.py audit <junit.xml> --class C
    python3 -I scripts/test-classes.py scan [--json]

Exit codes:
  0  ok
  1  a finding: a test unclassified or in two classes, drift under --strict, an audit problem
  2  unusable input: a missing or malformed TOML, TSV or JUnit file, nextest failing, an
     unknown class or kind

Standard library only. When GITHUB_STEP_SUMMARY is set, `check` appends its counts and findings
to it; when GITHUB_ACTIONS is "true", a finding is also an ::error:: annotation.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import sys
import tomllib
import xml.etree.ElementTree as ElementTree
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NEXTEST_CONFIG = ROOT / ".config" / "nextest.toml"
RULES = ROOT / ".config" / "test-classes.toml"
LIST = ROOT / ".config" / "test-classes.tsv"

RULES_FORMAT = "murmur.test-class-rules/1"
LIST_FORMAT = "murmur.test-classes/1"
CLASSES = ("capsule", "isolation", "host")
KINDS = ("unit", "integration", "e2e")
COLUMNS = ("binary_id", "test", "class", "kind", "config", "ignored")
UNIT_TARGET_KINDS = {"lib", "bin", "proc-macro", "cdylib", "rlib", "dylib", "staticlib"}
GENERATE_COMMAND = "python3 -I scripts/test-classes.py generate"
DOCS_PAGE = "docs/content/contributing/test-classes.md"

# Lines a test prints when the host cannot back what it asserts. Each is a pass that proved
# nothing, so `audit` fails on it.
HOST_SKIP_MARKERS = ("[SKIP-HOST]", "SKIP — PROVES NOTHING")
HOST_SKIP_PREFIX = "skipping"
# A missing local program or fixture, not a host gap: counted, never failed.
OTHER_SKIP_MARKER = "[SKIP]"


class Unusable(Exception):
    """Input the tool cannot work from; exit 2."""


def profile_name(cls: str) -> str:
    return f"class-{cls}"


def step_summary(lines: list[str]) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")


def annotate(title: str, message: str) -> None:
    if os.environ.get("GITHUB_ACTIONS") == "true":
        print(f"::error title={title}::{message}", flush=True)


# --- configuration ---------------------------------------------------------------------------


def read_toml(path: Path) -> dict:
    try:
        with open(path, "rb") as handle:
            return tomllib.load(handle)
    except FileNotFoundError:
        raise Unusable(f"{display(path)} does not exist")
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise Unusable(f"{path} cannot be read as TOML: {error}")


def display(path: Path) -> str:
    return str(path.relative_to(ROOT)) if path.is_relative_to(ROOT) else str(path)


def check_class(cls: str) -> str:
    if cls not in CLASSES:
        raise Unusable(f"unknown class {cls!r}; the classes are {', '.join(CLASSES)}")
    return cls


def check_kind(kind: str) -> str:
    if kind not in KINDS:
        raise Unusable(f"unknown kind {kind!r}; the kinds are {', '.join(KINDS)}")
    return kind


def class_filters(nextest_config: Path | None = None) -> dict[str, str]:
    """Each class's filterset: the `default-filter` of its profile in .config/nextest.toml."""
    nextest_config = nextest_config or NEXTEST_CONFIG
    profiles = read_toml(nextest_config).get("profile", {})
    filters = {}
    for cls in CLASSES:
        profile = profiles.get(profile_name(cls))
        if not isinstance(profile, dict):
            raise Unusable(f"{nextest_config} has no [profile.{profile_name(cls)}]")
        expression = profile.get("default-filter")
        if not isinstance(expression, str) or not expression.strip():
            raise Unusable(f"[profile.{profile_name(cls)}] in {nextest_config} has no default-filter")
        filters[cls] = expression
    return filters


def load_rules(rules: Path | None = None) -> dict:
    """.config/test-classes.toml, checked for the keys the tool reads."""
    rules = rules or RULES
    data = read_toml(rules)
    if data.get("format") != RULES_FORMAT:
        raise Unusable(f"{rules}: format is {data.get('format')!r}, expected {RULES_FORMAT!r}")
    classes = data.get("classes")
    if not isinstance(classes, dict) or sorted(classes) != sorted(CLASSES):
        raise Unusable(f"{rules}: [classes] must hold exactly {', '.join(CLASSES)}")
    for cls in CLASSES:
        for key in ("needs", "checked_by"):
            if not isinstance(classes[cls].get(key), str):
                raise Unusable(f"{rules}: [classes.{cls}] has no {key} text")
    e2e = data.get("kinds", {}).get("e2e")
    if not isinstance(e2e, str) or not e2e.strip():
        raise Unusable(f"{rules}: [kinds] has no e2e filterset")
    configurations = data.get("configuration")
    if not isinstance(configurations, list) or not configurations:
        raise Unusable(f"{rules}: no [[configuration]] entries")
    names = set()
    for entry in configurations:
        name, args = entry.get("name"), entry.get("args")
        if not isinstance(name, str) or not isinstance(args, list) or not all(isinstance(a, str) for a in args):
            raise Unusable(f"{rules}: every [[configuration]] needs a name and a list of args")
        if name in names:
            raise Unusable(f"{rules}: configuration {name!r} is named twice")
        names.add(name)
    return data


# --- nextest listing -------------------------------------------------------------------------


def parse_list_json(text: str) -> dict[tuple[str, str], dict]:
    """Tests in `cargo nextest list --message-format json` output.

    Fields read: `rust-suites` → each suite's `binary-id` and `kind`, and its `testcases` →
    each test's `ignored` and `filter-match.status` (`matches` when the test is selected).
    """
    try:
        data = json.loads(text)
        suites = data["rust-suites"]
        tests = {}
        for suite in suites.values():
            binary_id, kind = suite["binary-id"], suite["kind"]
            for name, case in suite["testcases"].items():
                tests[(binary_id, name)] = {
                    "kind": kind,
                    "ignored": bool(case.get("ignored", False)),
                    "matched": case["filter-match"]["status"] == "matches",
                }
        return tests
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        raise Unusable(f"nextest list output is not the expected JSON: {error!r}")


def nextest_list(args: list[str], extra: list[str]) -> dict[tuple[str, str], dict]:
    command = ["cargo", "nextest", "list", *args, "--run-ignored", "all", "--message-format", "json", *extra]
    try:
        done = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    except OSError as error:
        raise Unusable(f"cannot run cargo nextest: {error}")
    if done.returncode != 0:
        tail = "\n".join(done.stderr.strip().splitlines()[-15:])
        raise Unusable(f"`{' '.join(command)}` exited {done.returncode}:\n{tail}")
    return parse_list_json(done.stdout)


def nextest_version() -> str:
    try:
        done = subprocess.run(["cargo", "nextest", "--version"], cwd=ROOT, capture_output=True, text=True)
    except OSError as error:
        raise Unusable(f"cannot run cargo nextest: {error}")
    if done.returncode != 0 or not done.stdout.strip():
        raise Unusable("`cargo nextest --version` failed")
    first = done.stdout.strip().splitlines()[0]
    return first.split()[1] if len(first.split()) > 1 else first


def listings(rules: dict, filters: dict[str, str]) -> dict[str, dict]:
    """Every listing `compute` needs, per configuration, run a few at a time.

    Per configuration: the full set (no class profile), one per class profile, and the e2e
    kind filterset.
    """
    jobs = {}
    for entry in rules["configuration"]:
        args = entry["args"]
        jobs[(entry["name"], "all")] = (args, [])
        for cls in CLASSES:
            jobs[(entry["name"], cls)] = (args, ["--profile", profile_name(cls)])
        jobs[(entry["name"], "e2e")] = (args, ["-E", rules["kinds"]["e2e"]])
    results: dict[str, dict] = {entry["name"]: {} for entry in rules["configuration"]}
    # The first listing of each configuration builds; the rest find the build fresh. Running the
    # builds one at a time keeps them from waiting on the cargo lock with a thread each.
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        for entry in rules["configuration"]:
            results[entry["name"]]["all"] = nextest_list(*jobs[(entry["name"], "all")])
        futures = {
            pool.submit(nextest_list, *job): key for key, job in jobs.items() if key[1] != "all"
        }
        for future in concurrent.futures.as_completed(futures):
            name, which = futures[future]
            results[name][which] = future.result()
    return results


def classify(rules: dict, results: dict[str, dict]) -> tuple[dict, list[str]]:
    """Rows keyed by (binary_id, test), and the findings: unclassified, overlap, mismatch."""
    rows: dict[tuple[str, str], dict] = {}
    findings: list[str] = []
    for entry in rules["configuration"]:
        name = entry["name"]
        listing = results[name]
        for key, case in sorted(listing["all"].items()):
            classes = [cls for cls in CLASSES if listing[cls].get(key, {}).get("matched")]
            if case["kind"] in UNIT_TARGET_KINDS:
                kind = "unit"
                if listing["e2e"].get(key, {}).get("matched"):
                    findings.append(
                        f"e2e-unit: {key[0]} {key[1]} is a {case['kind']} test but the e2e "
                        "filterset in .config/test-classes.toml selects it"
                    )
            elif listing["e2e"].get(key, {}).get("matched"):
                kind = "e2e"
            else:
                kind = "integration"
            row = {
                "binary_id": key[0],
                "test": key[1],
                "class": classes[0] if len(classes) == 1 else None,
                "classes": classes,
                "kind": kind,
                "config": name,
                "ignored": case["ignored"],
            }
            if key in rows:
                first = rows[key]
                if (first["classes"], first["kind"]) != (classes, kind):
                    findings.append(
                        f"config-mismatch: {key[0]} {key[1]} is {'/'.join(first['classes']) or 'unclassified'} "
                        f"{first['kind']} in {first['config']} but {'/'.join(classes) or 'unclassified'} "
                        f"{kind} in {name}"
                    )
                continue
            rows[key] = row
    for key, row in sorted(rows.items()):
        if not row["classes"]:
            findings.append(
                f"unclassified: {key[0]} {key[1]} (config {row['config']}) is in no class; add a term "
                f"selecting binary_id({key[0]}) to one class profile's default-filter in "
                f".config/nextest.toml, then run `{GENERATE_COMMAND}` ({DOCS_PAGE})"
            )
        elif len(row["classes"]) > 1:
            findings.append(
                f"overlap: {key[0]} {key[1]} (config {row['config']}) is selected by "
                f"{' and '.join(profile_name(c) for c in row['classes'])}; a test belongs to one class"
            )
    return rows, findings


def compute(rules: dict | None = None, filters: dict[str, str] | None = None) -> tuple[dict, list[str]]:
    rules = rules if rules is not None else load_rules()
    filters = filters if filters is not None else class_filters()
    return classify(rules, listings(rules, filters))


# --- the list --------------------------------------------------------------------------------


def configuration_line(entry: dict) -> str:
    return f"# configuration {entry['name']}: cargo nextest list {' '.join(entry['args'])} --run-ignored all"


def render_tsv(rows: dict, rules: dict, version: str) -> str:
    lines = [
        f"# format: {LIST_FORMAT}",
        f"# generated by: {GENERATE_COMMAND}",
        *(configuration_line(entry) for entry in rules["configuration"]),
        f"# nextest: {version}",
        "\t".join(COLUMNS),
    ]
    for key in sorted(rows):
        row = rows[key]
        lines.append(
            "\t".join(
                [row["binary_id"], row["test"], row["class"], row["kind"], row["config"],
                 "true" if row["ignored"] else "false"]
            )
        )
    return "\n".join(lines) + "\n"


def read_tsv(path: Path | None = None) -> dict[tuple[str, str], dict]:
    path = path or LIST
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise Unusable(f"{path} does not exist; run `{GENERATE_COMMAND}`")
    except (OSError, UnicodeDecodeError) as error:
        raise Unusable(f"{path} cannot be read: {error}")
    lines = text.splitlines()
    headers = [line for line in lines if line.startswith("#")]
    if not headers or headers[0] != f"# format: {LIST_FORMAT}":
        raise Unusable(f"{path}: the first line must be `# format: {LIST_FORMAT}`")
    body = [line for line in lines if not line.startswith("#")]
    if not body or tuple(body[0].split("\t")) != COLUMNS:
        raise Unusable(f"{path}: the column header must be {' '.join(COLUMNS)}, tab-separated")
    rows = {}
    for number, line in enumerate(body[1:], start=len(headers) + 2):
        fields = line.split("\t")
        if len(fields) != len(COLUMNS):
            raise Unusable(f"{path}:{number}: expected {len(COLUMNS)} tab-separated fields")
        row = dict(zip(COLUMNS, fields))
        if row["class"] not in CLASSES or row["kind"] not in KINDS or row["ignored"] not in ("true", "false"):
            raise Unusable(f"{path}:{number}: invalid class, kind or ignored value")
        row["ignored"] = row["ignored"] == "true"
        key = (row["binary_id"], row["test"])
        if key in rows:
            raise Unusable(f"{path}:{number}: {key[0]} {key[1]} is listed twice")
        rows[key] = row
    if list(rows) != sorted(rows):
        raise Unusable(f"{path}: rows are not sorted by binary_id, then test")
    return rows


def drift(computed: dict, listed: dict) -> list[str]:
    out = []
    for key in sorted(set(computed) | set(listed)):
        if key not in listed:
            row = computed[key]
            out.append(f"added: {key[0]} {key[1]} ({row['class']} {row['kind']}, {row['config']})")
        elif key not in computed:
            row = listed[key]
            out.append(f"removed: {key[0]} {key[1]} (listed as {row['class']} {row['kind']})")
        else:
            was, now = listed[key], computed[key]
            changed = [
                f"{column} {was[column]} -> {now[column]}"
                for column in ("class", "kind", "config", "ignored")
                if was[column] != now[column]
            ]
            if changed:
                out.append(f"reclassified: {key[0]} {key[1]}: {', '.join(changed)}")
    return out


def counts(rows) -> dict:
    rows = list(rows)
    return {
        "total": len(rows),
        "class": {cls: sum(1 for r in rows if r["class"] == cls) for cls in CLASSES},
        "kind": {kind: sum(1 for r in rows if r["kind"] == kind) for kind in KINDS},
        "class_kind": {
            f"{cls}/{kind}": sum(1 for r in rows if r["class"] == cls and r["kind"] == kind)
            for cls in CLASSES
            for kind in KINDS
        },
        "ignored": sum(1 for r in rows if r["ignored"]),
    }


def counts_lines(summary: dict) -> list[str]:
    return [
        f"tests {summary['total']} (ignored {summary['ignored']})",
        "class " + " ".join(f"{cls} {summary['class'][cls]}" for cls in CLASSES),
        "kind " + " ".join(f"{kind} {summary['kind'][kind]}" for kind in KINDS),
    ]


# --- subcommands -----------------------------------------------------------------------------


def command_generate(args) -> int:
    rules = load_rules()
    rows, findings = compute(rules)
    if findings:
        for finding in findings:
            print(finding)
        print(f"not written: {len(findings)} finding(s); {display(LIST)} is unchanged")
        return 1
    LIST.write_text(render_tsv(rows, rules, nextest_version()), encoding="utf-8")
    for line in counts_lines(counts(rows.values())):
        print(line)
    print(f"wrote {display(LIST)}")
    return 0


def command_check(args) -> int:
    rules = load_rules()
    rows, findings = compute(rules)
    listed = read_tsv()
    # A test without exactly one class is already a finding; it is not drift as well.
    undecided = {key for key, row in rows.items() if not row["class"]}
    changes = drift(
        {k: r for k, r in rows.items() if k not in undecided},
        {k: r for k, r in listed.items() if k not in undecided},
    )
    unclassified = sum(1 for r in rows.values() if not r["classes"])
    overlap = sum(1 for r in rows.values() if len(r["classes"]) > 1)
    for finding in findings:
        print(finding)
    for change in changes:
        print(f"drift: {change}")
    if changes:
        print(f"the list is out of date; run `{GENERATE_COMMAND}` and commit .config/test-classes.tsv")
    summary = counts(r for r in rows.values() if r["class"])
    lines = counts_lines(summary) + [f"unclassified {unclassified} overlap {overlap} drift {len(changes)}"]
    for line in lines:
        print(line)
    failed = bool(findings) or (args.strict and bool(changes))
    report = ["### Test classes", "", *(f"- {line}" for line in lines)]
    if findings or changes:
        report += ["", "```", *findings, *(f"drift: {c}" for c in changes), "```"]
    if changes and not args.strict:
        report += ["", f"Drift is reported, not failed. Run `{GENERATE_COMMAND}` to update the list."]
    step_summary(report + [""])
    if findings:
        annotate("Test classes", f"{len(findings)} finding(s), first: {findings[0]}")
    elif failed:
        annotate("Test classes", f"{len(changes)} row(s) drifted from .config/test-classes.tsv; run `{GENERATE_COMMAND}`")
    if failed:
        print("CHECK FAILED")
        return 1
    print("PASS")
    return 0


def command_list(args) -> int:
    if args.cls:
        check_class(args.cls)
    if args.kind:
        check_kind(args.kind)
    if args.live:
        rows, findings = compute()
        if findings:
            for finding in findings:
                print(finding, file=sys.stderr)
            return 1
    else:
        rows = read_tsv()
    selected = [
        rows[key]
        for key in sorted(rows)
        if (not args.cls or rows[key]["class"] == args.cls) and (not args.kind or rows[key]["kind"] == args.kind)
    ]
    if args.json:
        tests = [{column: row[column] for column in COLUMNS} for row in selected]
        print(json.dumps({"format": LIST_FORMAT, "tests": tests, "counts": counts(selected)}, indent=1))
        return 0
    try:
        for row in selected:
            print("\t".join([row["binary_id"], row["test"], row["class"], row["kind"], row["config"],
                             "true" if row["ignored"] else "false"]))
    except BrokenPipeError:
        # `list | head` closes the pipe early; that is not an error.
        sys.stdout = open(os.devnull, "w")
    return 0


def command_filter(args) -> int:
    print(class_filters()[check_class(args.cls)].strip())
    return 0


def host_skip_line(line: str) -> bool:
    stripped = line.strip()
    return any(marker in stripped for marker in HOST_SKIP_MARKERS) or stripped.startswith(HOST_SKIP_PREFIX)


def audit_report(report: Path, cls: str, listed: dict) -> tuple[list[str], dict]:
    if not report.is_file():
        raise Unusable(f"no JUnit report at {report}")
    try:
        root = ElementTree.parse(report).getroot()
    except (OSError, ElementTree.ParseError) as error:
        raise Unusable(f"the JUnit report at {report} cannot be read: {error}")
    cases = list(root.iter("testcase"))
    if not cases:
        raise Unusable(f"the JUnit report at {report} holds no test")
    problems: list[str] = []
    totals = {"run": 0, "failed": 0, "host-skips": 0, "other-skips": 0, "outside-class": 0}
    for case in cases:
        binary_id, name = case.get("classname", "?"), case.get("name", "?")
        totals["run"] += 1
        row = listed.get((binary_id, name))
        if row is None:
            totals["outside-class"] += 1
            problems.append(f"outside-class: {binary_id} {name} is not in .config/test-classes.tsv")
        elif row["class"] != cls:
            totals["outside-class"] += 1
            problems.append(f"outside-class: {binary_id} {name} is class {row['class']}, not {cls}")
        if case.find("failure") is not None or case.find("error") is not None:
            totals["failed"] += 1
            problems.append(f"failed: {binary_id} {name}")
        # Every output element of the test, the retries' (flakyFailure, rerunFailure) included.
        text = "".join(case.itertext())
        skip_lines = [line.strip() for line in text.splitlines() if host_skip_line(line)]
        if skip_lines:
            totals["host-skips"] += 1
            first = skip_lines[0]
            problems.append(f"host-skip: {binary_id} {name}: {first[:300]}{' …' if len(first) > 300 else ''}")
        if OTHER_SKIP_MARKER in text:
            totals["other-skips"] += 1
    return problems, totals


def command_audit(args) -> int:
    cls = check_class(args.cls)
    problems, totals = audit_report(Path(args.report), cls, read_tsv())
    for problem in problems:
        print(problem)
    print(" ".join(f"{key} {value}" for key, value in totals.items()))
    if totals["failed"] or totals["host-skips"] or totals["outside-class"]:
        print(f"AUDIT FAILED: the report does not show class {cls} passing cleanly")
        return 1
    print(f"PASS: every test is class {cls}, none failed, none printed a host-skip line")
    return 0


# --- scan (approach A) -----------------------------------------------------------------------

# Markers, strongest class first. A binary is proposed the highest class any marker names.
SCAN_MARKERS: list[tuple[str, str, str]] = [
    ("host", "sudo", r"\bsudo\b"),
    # Changing the host, not reading it: a loaded profile or a written sysctl.
    ("host", "apparmor_parser", r"apparmor_parser"),
    ("host", "sysctl-write", r"sysctl\s+-w|Command::new\(\s*\"sysctl\""),
    ("host", "browser", r"(?i)\b(chrome|chromium|playwright)\b"),
    ("host", "node", r"Command::new\(\s*\"(node|npx|npm)\""),
    ("host", "real-host-url", r"https?://(?!(127\.0\.0\.1|localhost|\[::1\]|0\.0\.0\.0|example\.(com|org|invalid)|[a-z0-9.-]*\.invalid|\{))[a-z0-9.-]+\.[a-z]{2,}"),
    # Reported, not raised: most reads of $HOME follow a staging step that pointed it at a
    # scratch directory. The capsule-shaped run gives HOME an empty directory, which catches a
    # test that needs the real one.
    ("capsule", "home", r"dirs::home_dir|env::var(_os)?\(\s*\"HOME\"\s*\)"),
    ("isolation", "skip_without_host_support", r"skip_without_host_support"),
    ("isolation", "skip_without_egress_namespace", r"skip_without_egress_namespace"),
    ("isolation", "skip_without_seccomp", r"skip_without_seccomp"),
    ("isolation", "skip_without_root_namespace", r"skip_without_root_namespace"),
    ("isolation", "tiers", r"\btiers\("),
    ("isolation", "skip-host", r"\[SKIP-HOST\]"),
    ("isolation", "unshare", r"\bunshare\b"),
    ("isolation", "landlock", r"(?i)\blandlock"),
    ("isolation", "seccomp", r"(?i)\bseccomp"),
    # Reported, not raised: unit tests build ContainmentClass values as plain data.
    ("capsule", "containment", r"\bContainment(Class)?\b"),
    ("isolation", "cgroup", r"(?i)\bcgroup"),
    ("isolation", "systemd-run", r"systemd-run"),
    ("isolation", "mur-run", r"\.args?\(\s*\[?\s*\"run\"|\brun_capsule(_with_env)?\(|\bstage_agent_session\w*\(|\blaunch_session\(|\blaunch_formation\w*\("),
    ("isolation", "unix-socket", r"\bUnix(Listener|Stream|Datagram)\b"),
]
MUR_RUNS = re.compile(r"cargo_bin\(\s*\"mur(-roost)?\"\s*\)|CARGO_BIN_EXE_mur")
FN_DEF = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*(<[^{;]*?>)?\s*\(")
CALL = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*(?:::<[^>]*>)?\s*\(")
CLASS_RANK = {cls: rank for rank, cls in enumerate(CLASSES)}


def strip_comments(source: str) -> str:
    source = re.sub(r"/\*.*?\*/", " ", source, flags=re.S)
    return re.sub(r"//[^\n]*", " ", source)


def function_bodies(source: str) -> dict[str, str]:
    """Each `fn` in a source file with its body, found by brace matching."""
    bodies = {}
    for match in FN_DEF.finditer(source):
        start = source.find("{", match.end())
        semicolon = source.find(";", match.end())
        if start < 0 or (0 <= semicolon < start):
            continue
        depth, index = 0, start
        while index < len(source):
            if source[index] == "{":
                depth += 1
            elif source[index] == "}":
                depth -= 1
                if depth == 0:
                    break
            index += 1
        bodies[match.group(1)] = bodies.get(match.group(1), "") + source[start : index + 1]
    return bodies


def called_names(source: str) -> set[str]:
    return set(CALL.findall(source))


def helper_facts(common_sources: list[str]) -> dict[str, dict]:
    """Each helper function in tests/common: whether it runs `mur` or `mur-roost`, and its markers.

    Both are carried through the helpers it calls, so a binary calling one helper is credited
    with everything that helper reaches.
    """
    bodies: dict[str, str] = {}
    for source in common_sources:
        for name, body in function_bodies(strip_comments(source)).items():
            bodies[name] = bodies.get(name, "") + body
    calls = {name: (called_names(body) & bodies.keys()) - {name} for name, body in bodies.items()}
    facts = {
        name: {"runs_mur": bool(MUR_RUNS.search(body)), "markers": set(markers_in(body))}
        for name, body in bodies.items()
    }
    changed = True
    while changed:
        changed = False
        for name in bodies:
            for other in calls[name]:
                runs = facts[name]["runs_mur"] or facts[other]["runs_mur"]
                markers = facts[name]["markers"] | facts[other]["markers"]
                if runs != facts[name]["runs_mur"] or markers != facts[name]["markers"]:
                    facts[name] = {"runs_mur": runs, "markers": markers}
                    changed = True
    return facts


def markers_in(source: str) -> list[tuple[str, str]]:
    return [(cls, name) for cls, name, pattern in SCAN_MARKERS if re.search(pattern, source)]


def propose(markers: list[tuple[str, str]]) -> str:
    return max((cls for cls, _ in markers), key=CLASS_RANK.__getitem__, default="capsule")


def crate_dirs() -> dict[str, Path]:
    """Workspace members with their package names, from Cargo.toml's [workspace] members."""
    workspace = read_toml(ROOT / "Cargo.toml").get("workspace", {}).get("members", [])
    crates = {}
    for member in workspace:
        manifest = read_toml(ROOT / member / "Cargo.toml")
        crates[manifest["package"]["name"]] = ROOT / member
    return crates


def scan() -> list[dict]:
    results = []
    for package, directory in sorted(crate_dirs().items()):
        tests_dir = directory / "tests"
        common = sorted((tests_dir / "common").glob("*.rs")) if (tests_dir / "common").is_dir() else []
        common_text = [path.read_text(encoding="utf-8") for path in common]
        helpers = helper_facts(common_text)
        for path in sorted(tests_dir.glob("*.rs")) if tests_dir.is_dir() else []:
            source = strip_comments(path.read_text(encoding="utf-8"))
            uses_common = bool(re.search(r"\bmod\s+common\b", source))
            own = markers_in(source)
            called = sorted(called_names(source) & helpers.keys()) if uses_common else []
            through = sorted({m for name in called for m in helpers[name]["markers"]} - set(own))
            runs_mur = bool(MUR_RUNS.search(source)) or any(helpers[name]["runs_mur"] for name in called)
            results.append(
                {
                    "binary_id": f"{package}::{path.stem}",
                    "source": str(path.relative_to(ROOT)),
                    "markers": sorted({name for _, name in own}),
                    "helper_markers": sorted({name for _, name in through}),
                    "runs_mur": runs_mur,
                    "mur_helpers_called": sorted(name for name in called if helpers[name]["runs_mur"]),
                    "proposed_class": propose(own + through),
                    "proposed_kind": "e2e" if runs_mur else "integration",
                }
            )
        src = directory / "src"
        if not src.is_dir():
            continue
        # Unit tests, per module: the test names nextest lists start with the module path.
        modules: dict[str, list[tuple[str, str]]] = {}
        for path in sorted(src.rglob("*.rs")):
            relative = path.relative_to(src).with_suffix("")
            parts = [p for p in relative.parts if p not in ("mod",)]
            if parts in (["lib"], ["main"]):
                module = ""
            elif parts[0] == "bin":
                continue
            else:
                module = "::".join(parts)
            text = path.read_text(encoding="utf-8")
            if not re.search(r"#\[(tokio::)?test\b", text):
                continue
            # Only the test code: production code names cgroups and Landlock all the time.
            cut = text.find("#[cfg(test)]")
            if cut > 0 and not path.stem.endswith("tests"):
                text = text[cut:]
            modules.setdefault(module, []).extend(markers_in(strip_comments(text)))
        for module, found in sorted(modules.items()):
            results.append(
                {
                    "binary_id": f"{package} (unit)",
                    "module": module or "(crate root)",
                    "source": str(src.relative_to(ROOT)),
                    "markers": sorted({name for _, name in found}),
                    "proposed_class": propose(found),
                    "proposed_kind": "unit",
                }
            )
    return results


def command_scan(args) -> int:
    results = scan()
    if args.json:
        print(json.dumps({"format": "murmur.test-class-scan/1", "binaries": results}, indent=1))
        return 0
    per_class = {cls: 0 for cls in CLASSES}
    for entry in results:
        per_class[entry["proposed_class"]] += 1
        where = entry["binary_id"] + (f" {entry['module']}" if "module" in entry else "")
        markers = ", ".join(entry["markers"]) or "-"
        extra = ""
        if entry.get("helper_markers"):
            extra = f"  via helpers: {', '.join(entry['helper_markers'])}"
        print(f"{entry['proposed_class']:<9} {entry['proposed_kind']:<11} {where}  [{markers}]{extra}")
    print("proposed " + " ".join(f"{cls} {per_class[cls]}" for cls in CLASSES) + " (binaries and lib modules)")
    return 0


# --- entry -----------------------------------------------------------------------------------


def parser() -> argparse.ArgumentParser:
    top = argparse.ArgumentParser(description="Sort Murmur's tests by where each can run.")
    sub = top.add_subparsers(dest="command", required=True)
    sub.add_parser("generate", help="rewrite .config/test-classes.tsv")
    check = sub.add_parser("check", help="fail on an unclassified or doubly classified test")
    check.add_argument("--strict", action="store_true", help="also fail when the list has drifted")
    listing = sub.add_parser("list", help="print the list, or part of it")
    listing.add_argument("--class", dest="cls", help="only this class")
    listing.add_argument("--kind", help="only this kind")
    listing.add_argument("--json", action="store_true", help=f"print {LIST_FORMAT} JSON")
    listing.add_argument("--live", action="store_true", help="recompute from nextest instead of reading the list")
    filt = sub.add_parser("filter", help="print a class's filterset, for nextest -E")
    filt.add_argument("cls", metavar="class")
    audit = sub.add_parser("audit", help="check a JUnit report shows one class passing cleanly")
    audit.add_argument("report", help="a nextest JUnit report")
    audit.add_argument("--class", dest="cls", required=True)
    scan_parser = sub.add_parser("scan", help="propose classes from markers in test sources")
    scan_parser.add_argument("--json", action="store_true")
    return top


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    handlers = {
        "generate": command_generate,
        "check": command_check,
        "list": command_list,
        "filter": command_filter,
        "audit": command_audit,
        "scan": command_scan,
    }
    try:
        return handlers[args.command](args)
    except Unusable as error:
        print(f"test-classes: {error}", file=sys.stderr)
        annotate("Test classes", str(error).splitlines()[0])
        return 2


if __name__ == "__main__":
    sys.exit(main())
