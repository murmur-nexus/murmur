#!/usr/bin/env python3
"""Assert that this host installs one containment class for a real capsule, or fail.

Writes a probe capsule that declares the class and `shell.allow: [bash]`, so it needs a
cgroup scope and a network namespace like any subprocess-capable capsule, then asks the
`mur` under test three questions:

  1. `mur version --json`: does it publish `run --explain-scope --json` at the schema this
     script reads (2), and `doctor --json` at schema 1?
  2. `mur run --explain-scope --json`: is `achieved_containment` the class, `floor_met`
     true, `landlock_abi` set and `cgroup_scope.status` "delegated"?
  3. `mur doctor --json`, run in the probe's directory: is there no `error` finding? This
     is what catches E-CAP-005 (no network namespace) and E-RUN-012 (no cgroup scope),
     which `--explain-scope` alone does not turn into a lower class.

Every unmet check is printed, each naming what was missing in the report's own words, and
the script exits 1. It never skips. Run it inside the scope the capsule would get:

    systemd-run --user --scope --quiet --collect -p Delegate=yes -- \\
        python3 -I .github/scripts/assert-containment-tier.py --class scoped --mur target/debug/mur

Exit codes: 0 the class is installed, 1 at least one check failed, 2 bad arguments.
Standard library only. When GITHUB_STEP_SUMMARY is set a block is appended to it, passed
or failed; when GITHUB_ACTIONS is "true" each failure is also an ::error:: annotation.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile

CLASSES = ("scoped", "sealed")
EXPLAIN_COMMAND = "run --explain-scope --json"
EXPLAIN_SCHEMA = 2
DOCTOR_COMMAND = "doctor --json"
DOCTOR_SCHEMA = 1
# Generous: under load a debug `mur` probing namespaces and a cgroup takes seconds, not minutes.
COMMAND_TIMEOUT_SECONDS = 120


class Leg:
    """The checks of one tier leg, collected so every failure is reported, not the first."""

    def __init__(self, asked: str) -> None:
        self.asked = asked
        self.failures: list[str] = []
        self.facts: dict[str, str] = {}

    def fail(self, message: str) -> None:
        self.failures.append(message)
        print(f"FAIL: {message}", flush=True)


def run_json(leg: Leg, argv: list[str], cwd: str | None, accept_nonzero: bool):
    """Run `argv`, parse its standard output as one JSON object, or record why not."""
    shown = " ".join(argv)
    print(f"$ {shown}" + (f"    (in {cwd})" if cwd else ""), flush=True)
    try:
        done = subprocess.run(
            argv,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    except OSError as error:
        leg.fail(f"could not run `{argv[0]}`: {error}")
        return None
    except subprocess.TimeoutExpired:
        leg.fail(f"`{shown}` did not finish within {COMMAND_TIMEOUT_SECONDS} s")
        return None
    stderr = done.stderr.strip()
    if done.returncode != 0 and not accept_nonzero:
        leg.fail(
            f"`{shown}` exited {done.returncode}; its stderr: {stderr or '(empty)'}"
        )
        return None
    text = done.stdout.strip()
    if not text:
        leg.fail(
            f"`{shown}` printed nothing on stdout (exit {done.returncode}); "
            f"its stderr: {stderr or '(empty)'}"
        )
        return None
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        # Tolerate leading noise: the report is the last line.
        try:
            value = json.loads(text.splitlines()[-1])
        except json.JSONDecodeError as error:
            leg.fail(
                f"`{shown}` did not print valid JSON ({error}); stdout began "
                f"{text[:200]!r}; its stderr: {stderr or '(empty)'}"
            )
            return None
    if not isinstance(value, dict):
        leg.fail(f"`{shown}` printed JSON that is not an object: {text[:200]!r}")
        return None
    return value


def check_version(leg: Leg, mur: str) -> bool:
    """Schema checks. Returns False when `mur` could not be run at all."""
    report = run_json(leg, [mur, "version", "--json"], cwd=None, accept_nonzero=False)
    if report is None:
        return False
    leg.facts["mur version"] = f"{report.get('mur_version')} ({report.get('commit')})"
    print(
        f"mur {report.get('mur_version')}, commit {report.get('commit')}, "
        f"WIT level {report.get('wit_level')}",
        flush=True,
    )
    commands = report.get("commands")
    if not isinstance(commands, dict):
        leg.fail(
            "`mur version --json` has no `commands` object, so this mur does not say "
            f"which schema `{EXPLAIN_COMMAND}` writes"
        )
        return True
    for command, wanted in ((EXPLAIN_COMMAND, EXPLAIN_SCHEMA), (DOCTOR_COMMAND, DOCTOR_SCHEMA)):
        got = commands.get(command)
        print(f"schema of `mur {command}`: {got}", flush=True)
        if got is None:
            leg.fail(
                f"`mur {command}`: this assertion reads schema {wanted}, "
                "this mur does not publish that command"
            )
        elif got != wanted:
            leg.fail(
                f"`mur {command}`: this assertion reads schema {wanted}, "
                f"this mur reports {got}"
            )
    return True


def check_scope(leg: Leg, mur: str, probe: str) -> None:
    report = run_json(
        leg,
        [mur, "run", "--manifest", "murmur.yaml", "--explain-scope", "--json"],
        cwd=probe,
        accept_nonzero=False,
    )
    if report is None:
        leg.facts["class installed"] = "unknown (no --explain-scope report)"
        return
    for key in (
        "declared_containment",
        "achieved_containment",
        "host_containment",
        "floor_met",
        "shortfall_reason",
        "enforcement_tier",
        "landlock_abi",
        "userns_grant",
        "cgroup_scope",
    ):
        if key in report:
            print(f"  {key}: {json.dumps(report[key])}", flush=True)

    achieved = report.get("achieved_containment")
    tier = report.get("enforcement_tier")
    abi = report.get("landlock_abi")
    grant = report.get("userns_grant")
    leg.facts["class installed"] = f"{achieved} (enforcement tier `{tier}`)"
    leg.facts["enforcement tier"] = str(tier)
    leg.facts["Landlock ruleset ABI"] = "none" if abi is None else str(abi)
    leg.facts["userns grant"] = "null" if grant is None else str(grant)

    for key in ("achieved_containment", "floor_met", "enforcement_tier"):
        if key not in report:
            leg.fail(
                f"the --explain-scope report has no `{key}`: a report this assertion "
                "does not understand"
            )

    if "achieved_containment" in report and achieved != leg.asked:
        detail = f"`{leg.asked}` was asked, `{achieved}` was reached"
        if report.get("shortfall_reason"):
            detail += f"; shortfall_reason: {report['shortfall_reason']}"
        detail += f"; userns_grant: {grant}; host_containment: {report.get('host_containment')}"
        leg.fail(detail)
    if "floor_met" in report and report.get("floor_met") is not True:
        leg.fail(
            f"floor_met is {json.dumps(report.get('floor_met'))}: `mur run` would refuse "
            f"a capsule declaring `{report.get('declared_containment')}`"
        )
    if abi is None:
        leg.fail(
            "landlock_abi is none: no Landlock ruleset would be installed, and both "
            "scoped and sealed install one"
        )

    scope = report.get("cgroup_scope")
    if not isinstance(scope, dict) or "status" not in scope:
        leg.facts["cgroup scope"] = "missing"
        leg.fail(
            f"cgroup_scope is {json.dumps(scope)}: a report this assertion does not "
            "understand (it needs `cgroup_scope.status`)"
        )
    else:
        status = scope["status"]
        errno = scope.get("errno")
        reason = scope.get("reason")
        leg.facts["cgroup scope"] = str(status) + (f" ({errno})" if errno else "")
        if status != "delegated":
            detail = f"cgroup_scope.status is `{status}`, not `delegated`"
            if errno:
                detail += f"; errno: {errno}"
            if reason:
                detail += f"; reason: {reason}"
            leg.fail(detail)


def check_doctor(leg: Leg, mur: str, probe: str) -> None:
    # `mur doctor` exits non-zero when it refuses; the JSON, not the exit code, is the answer.
    report = run_json(leg, [mur, "doctor", "--json"], cwd=probe, accept_nonzero=True)
    if report is None:
        leg.facts["mur doctor"] = "no report"
        return
    findings = report.get("findings")
    if not isinstance(findings, list):
        leg.facts["mur doctor"] = "no findings list"
        leg.fail(
            "the `mur doctor --json` report has no `findings` list: a report this "
            "assertion does not understand"
        )
        return
    for finding in findings:
        if isinstance(finding, dict):
            print(
                f"  doctor {finding.get('severity')} {finding.get('code')} "
                f"[{finding.get('block')}]: {finding.get('message')}",
                flush=True,
            )
    errors = [f for f in findings if isinstance(f, dict) and f.get("severity") == "error"]
    warnings = [f for f in findings if isinstance(f, dict) and f.get("severity") == "warning"]
    tally_errors = report.get("errors")
    tally_warnings = report.get("warnings")
    verdict = report.get("verdict")
    tally = (
        f"verdict {verdict}, tally errors={tally_errors} warnings={tally_warnings}, "
        f"findings list errors={len(errors)} warnings={len(warnings)}"
    )
    print(f"  doctor: {tally}", flush=True)
    if tally_errors != len(errors):
        # The findings list is what names a refusal; the tally is printed beside it.
        print(
            f"NOTE: mur doctor's `errors` tally ({tally_errors}) differs from its "
            f"error findings ({len(errors)}); the findings list decides",
            flush=True,
        )
    leg.facts["mur doctor"] = f"{verdict} ({len(errors)} errors, {len(warnings)} warnings)"
    for finding in errors:
        leg.fail(
            f"mur doctor {finding.get('code')} [{finding.get('block')}]: "
            f"{finding.get('message')}"
        )


def write_summary(leg: Leg, kernel: str) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    passed = not leg.failures
    lines = [
        f"### Tier assertion `{leg.asked}`: {'passed' if passed else 'FAILED'}",
        "",
        f"- class asked: {leg.asked}",
        f"- class installed: {leg.facts.get('class installed', 'unknown')}",
        f"- Landlock ruleset ABI: {leg.facts.get('Landlock ruleset ABI', 'unknown')}",
        f"- userns grant: {leg.facts.get('userns grant', 'unknown')}",
        f"- cgroup scope: {leg.facts.get('cgroup scope', 'unknown')}",
        f"- mur doctor: {leg.facts.get('mur doctor', 'unknown')}",
        f"- kernel: {kernel}",
    ]
    if "mur version" in leg.facts:
        lines.append(f"- mur: {leg.facts['mur version']}")
    for failure in leg.failures:
        lines.append(f"- reason: {failure}")
    lines.append("")
    with open(path, "a", encoding="utf-8") as summary:
        summary.write("\n".join(lines) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Fail unless this host installs the given containment class for a real capsule."
    )
    parser.add_argument("--class", dest="asked", required=True, choices=CLASSES)
    parser.add_argument(
        "--mur",
        default="target/debug/mur",
        help="the mur binary to assert against (default: target/debug/mur)",
    )
    args = parser.parse_args()

    leg = Leg(args.asked)
    kernel = platform.release()
    print(f"== tier assertion: {args.asked} (kernel {kernel}, mur {args.mur})", flush=True)

    # A relative path is resolved here: the probe commands run in another directory.
    mur = args.mur
    if os.sep in mur:
        mur = os.path.abspath(mur)

    probe = None
    if check_version(leg, mur):
        probe = tempfile.mkdtemp(prefix=f"tier-probe-{args.asked}-")
        with open(os.path.join(probe, "murmur.yaml"), "w", encoding="utf-8") as manifest:
            manifest.write(
                "name: tier-probe\n"
                "version: 0.0.1\n"
                "capabilities:\n"
                f"  containment: {args.asked}\n"
                "  shell:\n"
                "    allow: [bash]\n"
            )
        print(f"probe capsule: {probe}/murmur.yaml", flush=True)
        check_scope(leg, mur, probe)
        check_doctor(leg, mur, probe)

    write_summary(leg, kernel)

    if leg.failures:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            for failure in leg.failures:
                # Annotations end at a newline; keep each to one line.
                one_line = failure.replace("\r", " ").replace("\n", " ")
                print(f"::error title=Tier assertion {args.asked}::{one_line}", flush=True)
        if probe:
            print(f"probe capsule kept for inspection: {probe}", flush=True)
        print(
            f"TIER ASSERTION FAILED: {args.asked} is not installed on this host "
            f"({len(leg.failures)} unmet check{'s' if len(leg.failures) != 1 else ''})",
            flush=True,
        )
        return 1

    if probe:
        shutil.rmtree(probe, ignore_errors=True)
    print(
        f"PASS: {args.asked} reached, enforcement tier {leg.facts['enforcement tier']}; "
        f"Landlock ruleset ABI {leg.facts['Landlock ruleset ABI']}, userns grant "
        f"{leg.facts['userns grant']}, cgroup scope {leg.facts['cgroup scope']}, "
        f"mur doctor {leg.facts['mur doctor']}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
