#!/usr/bin/env python3
"""Measure end to end line coverage: the workspace source in `mur` and `mur-roost` that the
`e2e` tests execute.

Builds the workspace with `-C instrument-coverage` for the host target into its own target
directory, runs the `e2e` tests in two parts, merges the profiles and writes
`murmur.e2e-baseline/1` JSON with two numbers:

  all_e2e      every `e2e` test (part A, then part B)
  capsule_e2e  the `capsule`-class `e2e` tests alone (part A): the floor a capsule stage can be
               held to

  part A  cargo nextest run --profile class-capsule  -E '<e2e>'
  part B  cargo nextest run --profile class-isolation -E '<e2e>', then --profile class-host

Both parts run in `systemd-run --user --scope -p Delegate=yes -p TasksMax=infinity`, as CI runs
the suite. The `e2e` filterset is `[kinds] e2e` in .config/test-classes.toml, the class rules the
profiles' `default-filter` in .config/nextest.toml.

    python3 -I scripts/e2e-coverage.py --target-dir <CTD> --profiles <dir> --out <json>
        [--only <filterset>] [--keep-going] [--recheck-target-dir <TD>]
        [--build-only | --report-only]

    python3 -I scripts/e2e-coverage.py mutation-sample --baseline <json> --outcomes <outcomes.json>
        --file <path> --test-filter <filterset> --command <text> --chosen-because <text>
        --wall-seconds <n> --time-cap-seconds <n> [--stopped-by-cap]

`--only` intersects every part with one more filterset. Without `--keep-going` a test that fails
under instrumentation fails the script (exit 1) after the JSON is written; with it the failure is
only recorded. `--recheck-target-dir` re-runs each such test once on an uninstrumented build and
records whether it passes there. `--build-only` stops after the build; `--report-only` merges and
exports the profiles and JUnit reports a previous run left in `--profiles`.

`mutation-sample` copies a cargo-mutants `outcomes.json` into the baseline's `mutation_sample`.

Exit codes:
  0  measured and written
  1  a test failed under instrumentation (without --keep-going), or nothing was measured
  2  unusable input or a tool that cannot run: no llvm-tools, a failed build, a bad flag

Standard library only.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
import tomllib
import xml.etree.ElementTree as ElementTree
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FORMAT = "murmur.e2e-baseline/1"
DEFINITION = (
    "line coverage of workspace source compiled into mur and mur-roost, executed while the e2e tests ran"
)
OBJECTS = ("mur", "mur-roost")
FEATURES = "beta-all"
SCOPE = ["systemd-run", "--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes", "-p", "TasksMax=infinity", "--"]
PARTS = (("A", "capsule", "class-capsule"), ("B", "isolation", "class-isolation"), ("B", "host", "class-host"))


class Unusable(Exception):
    pass


def log(message: str) -> None:
    stamp = datetime.datetime.now().strftime("%H:%M:%S")
    print(f"[{stamp}] {message}", flush=True)


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    log("$ " + shlex.join(command))
    return subprocess.run(command, cwd=ROOT, **kwargs)


def output(command: list[str]) -> str:
    try:
        return subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise Unusable(f"`{shlex.join(command)}` failed: {error}")


def host_triple() -> str:
    for line in output(["rustc", "-vV"]).splitlines():
        if line.startswith("host: "):
            return line.split(": ", 1)[1]
    raise Unusable("`rustc -vV` names no host")


def llvm_tools() -> tuple[Path, Path]:
    bindir = Path(output(["rustc", "--print", "sysroot"])) / "lib" / "rustlib" / host_triple() / "bin"
    profdata, cov = bindir / "llvm-profdata", bindir / "llvm-cov"
    if not (os.access(profdata, os.X_OK) and os.access(cov, os.X_OK)):
        raise Unusable(f"no llvm-profdata and llvm-cov in {bindir}; run `rustup component add llvm-tools`")
    return profdata, cov


# The profile runtime's `truncateCurrentFile` marks a process with `setenv("__LLVM_PROFILE_RT_INIT_ONCE")`
# at start-up. `mur __guest-worker` refuses to start with any variable in its environment, so an
# instrumented `mur` cannot start a guest worker and every capsule launch fails. The call is replaced
# with a five-byte NOP in the instrumented `mur` and `mur-roost`. With a `%m` pool pattern, the only
# kind this script and the runtime's default use, the function returns before truncating anything
# whether or not the variable is set, so profiles are unchanged.
SETENV_CALL = re.compile(r"^\s*([0-9a-f]+):\s+e8(?: [0-9a-f]{2}){4}\s+call\s+[0-9a-f]+ <setenv@plt>", re.M)
NOP5 = bytes.fromhex("0f1f440000")


def file_offset(binary: Path, address: int) -> int:
    for line in output(["objdump", "-h", str(binary)]).splitlines():
        fields = line.split()
        # Idx Name Size VMA LMA File-off Algn
        if len(fields) >= 7 and fields[0].isdigit():
            size, vma, offset = int(fields[2], 16), int(fields[3], 16), int(fields[5], 16)
            if vma <= address < vma + size:
                return offset + address - vma
    raise Unusable(f"no section of {binary} holds address {address:#x}")


def neutralize_profile_setenv(binary: Path) -> str:
    """Replace the profile runtime's start-up `setenv` call with a NOP; idempotent."""
    symbols = output(["nm", str(binary)])
    match = re.search(r"^([0-9a-f]+) t truncateCurrentFile$", symbols, re.M)
    if not match:
        raise Unusable(f"{binary} has no truncateCurrentFile; is it instrumented?")
    start = int(match.group(1), 16)
    listing = output(["objdump", "-d", f"--start-address={start:#x}", f"--stop-address={start + 0x100:#x}", str(binary)])
    call = SETENV_CALL.search(listing)
    offset = None
    if call:
        offset = file_offset(binary, int(call.group(1), 16))
    else:
        # Already patched: the NOP sits where the call was.
        nop = re.search(r"^\s*([0-9a-f]+):\s+0f 1f 44 00 00\s+nop", listing, re.M)
        if nop:
            return f"{binary.name}: already patched at {int(nop.group(1), 16):#x}"
        raise Unusable(f"no setenv call in {binary}'s truncateCurrentFile")
    with open(binary, "r+b") as handle:
        handle.seek(offset)
        if handle.read(1) != b"\xe8":
            raise Unusable(f"{binary}: byte at {offset:#x} is not a call")
        handle.seek(offset)
        handle.write(NOP5)
    return f"{binary.name}: setenv call at {int(call.group(1), 16):#x} replaced with a NOP"


def cargo_config(triple: str) -> list[str]:
    """The instrumented build's settings, passed to every cargo invocation so they all agree.

    The rustflags are scoped to the host target, so nothing built for wasm32 is instrumented.
    Dependencies are optimized as in CI's cargo-test job (.github/cargo-test-ci.toml): Cranelift
    compiling capsules at test time is otherwise CPU bound. Workspace crates stay at opt-level 0.
    """
    return [
        "--config", f'target.{triple}.rustflags=["-C","instrument-coverage"]',
        "--config", 'profile.dev.package."*".opt-level=2',
        "--config", 'profile.test.package."*".opt-level=2',
    ]


def e2e_filterset() -> str:
    with open(ROOT / ".config" / "test-classes.toml", "rb") as handle:
        rules = tomllib.load(handle)
    try:
        return rules["kinds"]["e2e"].strip()
    except KeyError:
        raise Unusable(".config/test-classes.toml has no [kinds] e2e")


def scratch_nextest_config(profiles: Path) -> Path:
    """.config/nextest.toml with the store under `profiles`, plus a no-retry recheck profile.

    nextest writes JUnit reports under the workspace's target/nextest whatever the target
    directory, so a coverage run would replace a normal run's report of the same profile.
    """
    base = (ROOT / ".config" / "nextest.toml").read_text(encoding="utf-8")
    store = profiles / "nextest-store"
    extra = f"""
[store]
dir = {json.dumps(str(store))}

[profile.e2e-recheck]
inherits = "ci"
retries = 0
"""
    path = profiles / "nextest.toml"
    path.write_text(base + extra, encoding="utf-8")
    return path


def junit_path(profiles: Path, profile: str) -> Path:
    return profiles / "nextest-store" / profile / "junit.xml"


def read_junit(path: Path) -> dict:
    if not path.is_file():
        return {"tests_run": 0, "passed": 0, "failed": 0, "skipped": 0, "junit_sha256": None, "failures": []}
    try:
        root = ElementTree.parse(path).getroot()
    except ElementTree.ParseError as error:
        raise Unusable(f"{path} cannot be parsed: {error}")
    cases = list(root.iter("testcase"))
    failures = [
        (case.get("classname", "?"), case.get("name", "?"))
        for case in cases
        if case.find("failure") is not None or case.find("error") is not None
    ]
    skipped = sum(1 for case in cases if case.find("skipped") is not None)
    return {
        "tests_run": len(cases),
        "passed": len(cases) - len(failures) - skipped,
        "failed": len(failures),
        "skipped": skipped,
        "junit_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "failures": failures,
    }


def merge(profdata: Path, inputs: list[Path], out: Path) -> bool:
    files = sorted(p for d in inputs if d.is_dir() for p in d.glob("*.profraw"))
    if not files:
        return False
    listing = out.with_suffix(".inputs")
    listing.write_text("\n".join(str(p) for p in files) + "\n", encoding="utf-8")
    done = run([str(profdata), "merge", "-sparse", f"--input-files={listing}", "-o", str(out)])
    if done.returncode != 0:
        raise Unusable(f"llvm-profdata merge into {out} failed")
    return True


def export(cov: Path, profile: Path, objects: list[Path], ignore: str, out: Path) -> dict:
    command = [str(cov), "export", "-summary-only", f"-instr-profile={profile}", str(objects[0])]
    for extra in objects[1:]:
        command += ["-object", str(extra)]
    command.append(f"--ignore-filename-regex={ignore}")
    with open(out, "w", encoding="utf-8") as handle:
        done = run(command, stdout=handle)
    if done.returncode != 0:
        raise Unusable(f"llvm-cov export for {profile} failed")
    return json.loads(out.read_text(encoding="utf-8"))


def lines(covered: int, total: int) -> dict:
    return {
        "lines_total": total,
        "lines_covered": covered,
        "percent": round(100 * covered / total, 2) if total else 0.0,
    }


def totals(exported: dict) -> dict:
    summary = exported["data"][0]["totals"]["lines"]
    return lines(summary["covered"], summary["count"])


def crate_dirs() -> list[tuple[str, str]]:
    """(directory prefix, package name) per workspace member, longest prefix first."""
    with open(ROOT / "Cargo.toml", "rb") as handle:
        members = tomllib.load(handle)["workspace"]["members"]
    pairs = []
    for member in members:
        with open(ROOT / member / "Cargo.toml", "rb") as handle:
            pairs.append((str((ROOT / member).resolve()) + "/", tomllib.load(handle)["package"]["name"]))
    return sorted(pairs, key=lambda pair: -len(pair[0]))


def by_crate(exported: dict) -> dict:
    crates = crate_dirs()
    sums: dict[str, list[int]] = {}
    for entry in exported["data"][0]["files"]:
        name = next((package for prefix, package in crates if entry["filename"].startswith(prefix)), "other")
        count = entry["summary"]["lines"]
        bucket = sums.setdefault(name, [0, 0])
        bucket[0] += count["covered"]
        bucket[1] += count["count"]
    return {name: lines(covered, total) for name, (covered, total) in sorted(sums.items())}


def disk_free_gb() -> float:
    return round(shutil.disk_usage("/").free / 1e9, 1)


def stray_profiles(since: float, profiles: Path, target_dir: Path) -> dict:
    """Profiles written outside `profiles` during the run, which are then deleted.

    A process that clears its environment loses LLVM_PROFILE_FILE and writes default*.profraw
    into its working directory.
    """
    home = Path.home()
    roots = [ROOT, Path("/tmp"), Path("/var/tmp"), home]
    if os.environ.get("TMPDIR"):
        roots.append(Path(os.environ["TMPDIR"]))
    skip = {profiles.resolve(), target_dir.resolve(), (home / ".cache").resolve(), (home / ".rustup").resolve(),
            (home / ".cargo").resolve()}
    found: list[Path] = []
    seen: set[Path] = set()
    for root in roots:
        if not root.is_dir():
            continue
        for directory, subdirs, files in os.walk(root, onerror=lambda error: None):
            here = Path(directory)
            subdirs[:] = [d for d in subdirs if (here / d).resolve() not in skip and not (here / d).is_symlink()]
            for name in files:
                if not name.endswith(".profraw"):
                    continue
                path = here / name
                try:
                    if path.stat().st_mtime >= since and path.resolve() not in seen:
                        seen.add(path.resolve())
                        found.append(path)
                except OSError:
                    continue
    directories = sorted({str(p.parent) for p in found})
    for path in found:
        try:
            path.unlink()
        except OSError:
            pass
    return {"count": len(found), "directories": directories}


def nextest_version() -> str:
    first = output(["cargo", "nextest", "--version"]).splitlines()[0]
    return first.split()[1]


def commit_state() -> tuple[str, bool]:
    commit = output(["git", "rev-parse", "HEAD"])
    dirty = output(["git", "status", "--porcelain", "--", "crates", "Cargo.toml", "Cargo.lock"])
    return commit, not dirty


def recheck(failures: list[tuple[str, str]], target_dir: Path, config: Path) -> dict[tuple[str, str], bool]:
    """Run each failed test once more on an uninstrumented build: does it pass there?"""
    if not failures:
        return {}
    expression = " | ".join(f"(binary_id({b}) & test(={t}))" for b, t in failures)
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
    env.pop("LLVM_PROFILE_FILE", None)
    run(SCOPE + ["cargo", "nextest", "run", "--config-file", str(config), "--profile", "e2e-recheck",
                 "--workspace", "--features", FEATURES, "--no-fail-fast", "--no-tests", "pass",
                 "-E", expression], env=env)
    report = read_junit(junit_path(config.parent, "e2e-recheck"))
    failed_again = set(report["failures"])
    ran = report["tests_run"] > 0
    return {key: ran and key not in failed_again for key in failures}


def measure(args) -> int:
    started = time.time()
    target_dir = Path(args.target_dir).resolve()
    profiles = Path(args.profiles).resolve()
    out = Path(args.out).resolve()
    profdata, cov = llvm_tools()
    triple = host_triple()
    config_args = cargo_config(triple)
    e2e = e2e_filterset()
    expression = f"({e2e})" + (f" & ({args.only})" if args.only else "")
    profiles.mkdir(parents=True, exist_ok=True)
    nextest_config = scratch_nextest_config(profiles)
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
    disk_before = disk_free_gb()
    load_at_start = round(os.getloadavg()[0], 2)
    commit, clean = commit_state()

    if not args.report_only:
        (target_dir / "build-profiles").mkdir(parents=True, exist_ok=True)
        # Build scripts and proc macros are instrumented too; their profiles go here, not into
        # the crate directories they run in.
        build_env = dict(env, LLVM_PROFILE_FILE=str(target_dir / "build-profiles" / "%8m.profraw"))
        done = run(["cargo", "nextest", "list", "--workspace", "--features", FEATURES, *config_args,
                    "--config-file", str(nextest_config), "-E", expression],
                   env=build_env, stdout=subprocess.DEVNULL)
        if done.returncode != 0:
            raise Unusable("the instrumented build failed")
        if args.build_only:
            log("built")
            return 0

    patched = []
    for name in OBJECTS:
        patched.append(neutralize_profile_setenv(target_dir / "debug" / name))
        log(patched[-1])

    runs = []
    failures: list[tuple[str, str]] = []
    for part, name, profile in PARTS:
        report = junit_path(profiles, profile)
        if not args.report_only:
            (profiles / part).mkdir(parents=True, exist_ok=True)
            if name in ("capsule", "isolation"):
                # The first run into each part directory: an earlier run's profiles must not merge in.
                for stale in (profiles / part).glob("*.profraw"):
                    stale.unlink()
            if report.exists():
                report.unlink()
            # %8m: a pool of eight files per binary signature, merged online, so thousands of
            # `mur` processes do not each write a profile of several megabytes.
            part_env = dict(env, LLVM_PROFILE_FILE=str(profiles / part / "%8m.profraw"))
            done = run(SCOPE + ["cargo", "nextest", "run", "--config-file", str(nextest_config), "--profile", profile,
                                "--workspace", "--features", FEATURES, *config_args, "--no-fail-fast",
                                "--no-tests", "pass", "-E", expression], env=part_env)
            # 100: tests failed. Anything else non-zero means nextest could not run them.
            if done.returncode not in (0, 100):
                raise Unusable(f"part {part} ({profile}) did not run: nextest exited {done.returncode}")
        result = read_junit(report)
        failures += result.pop("failures")
        runs.append({"part": name, "profile": profile, **result})
        if result["failed"] and not args.keep_going and not args.report_only:
            log(f"part {part} ({profile}): {result['failed']} test(s) failed; stopping (--keep-going runs on)")
            break

    objects = [target_dir / "debug" / name for name in OBJECTS]
    for path in objects:
        if not path.is_file():
            raise Unusable(f"no instrumented {path}")
    ignore = "|".join([r"/\.cargo/", r"/rustc/", r"/tests/", re.escape(str(target_dir) + "/")])
    if not merge(profdata, [profiles / "A"], profiles / "capsule.profdata"):
        log("part A left no profile; capsule_e2e is zero")
    if not merge(profdata, [profiles / "A", profiles / "B"], profiles / "all.profdata"):
        log("no profile was written")
        return 1
    all_export = export(cov, profiles / "all.profdata", objects, ignore, profiles / "all-export.json")
    if (profiles / "capsule.profdata").exists():
        capsule_export = export(cov, profiles / "capsule.profdata", objects, ignore, profiles / "capsule-export.json")
        capsule = totals(capsule_export)
    else:
        capsule = lines(0, totals(all_export)["lines_total"])

    rechecked = {}
    if args.recheck_target_dir and failures:
        rechecked = recheck(failures, Path(args.recheck_target_dir).resolve(), nextest_config)

    strays = {"count": 0, "directories": []} if args.report_only else stray_profiles(started, profiles, target_dir)
    previous = {}
    if out.exists():
        try:
            previous = json.loads(out.read_text(encoding="utf-8"))
        except ValueError:
            previous = {}
    document = {
        "format": FORMAT,
        "commit": commit,
        "tree_clean": clean,
        "measured_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "host": {
            "kernel": os.uname().release,
            "load_1min_at_start": load_at_start,
            "nextest": nextest_version(),
            "rustc": output(["rustc", "--version"]),
            "disk_free_before_gb": disk_before,
            "disk_free_after_gb": disk_free_gb(),
        },
        "command": "python3 -I scripts/e2e-coverage.py " + shlex.join(sys.argv[1:]),
        "definition": DEFINITION,
        "objects": list(OBJECTS),
        "ignore_filename_regex": ignore,
        "features": FEATURES,
        "cargo_config": [value for flag, value in zip(config_args[::2], config_args[1::2])],
        "profile_runtime_patch": patched,
        "e2e_filterset": e2e,
        "only": args.only,
        "coverage": {"all_e2e": totals(all_export), "capsule_e2e": capsule, "by_crate": by_crate(all_export)},
        "runs": runs,
        "failed_under_instrumentation": [
            {"binary_id": b, "test": t, "passes_uninstrumented": rechecked.get((b, t))} for b, t in failures
        ],
        "stray_profiles": strays,
    }
    if "mutation_sample" in previous:
        document["mutation_sample"] = previous["mutation_sample"]
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    log(f"wrote {out}: all_e2e {document['coverage']['all_e2e']}, capsule_e2e {capsule}")
    if failures and not args.keep_going:
        return 1
    return 0


def mutation_sample(args) -> int:
    baseline = Path(args.baseline)
    try:
        document = json.loads(baseline.read_text(encoding="utf-8"))
        outcomes = json.loads(Path(args.outcomes).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise Unusable(f"cannot read the baseline or outcomes: {error}")
    counts = {"caught": 0, "missed": 0, "timeout": 0, "unviable": 0}
    names = {"CaughtMutant": "caught", "MissedMutant": "missed", "Timeout": "timeout", "Unviable": "unviable"}
    missed = []
    for outcome in outcomes.get("outcomes", []):
        scenario = outcome.get("scenario")
        if not isinstance(scenario, dict) or "Mutant" not in scenario:
            continue
        kind = names.get(outcome.get("summary"))
        if kind is None:
            continue
        counts[kind] += 1
        if kind == "missed":
            mutant = scenario["Mutant"]
            span = mutant.get("span", {}).get("start", {})
            missed.append({
                "file": mutant.get("file"),
                "line": span.get("line"),
                # The name reads "<file>:<line>:<column>: <what was replaced, with what>".
                "mutation": mutant.get("name", "?").split(": ", 1)[-1],
                "function": (mutant.get("function") or {}).get("function_name"),
            })
    viable = counts["caught"] + counts["missed"] + counts["timeout"]
    version = output(["cargo", "mutants", "--version"]).split()[-1]
    document["mutation_sample"] = {
        "tool": f"cargo-mutants {version}",
        "crate": "murmur-cli",
        "files": args.file,
        "test_filter": args.test_filter,
        "command": args.command,
        "chosen_because": args.chosen_because,
        "mutants_total": sum(counts.values()),
        **counts,
        "caught_of_viable": f"{counts['caught']} of {viable}",
        "wall_seconds": args.wall_seconds,
        "time_cap_seconds": args.time_cap_seconds,
        "stopped_by_cap": args.stopped_by_cap,
        "missed_list": missed,
    }
    baseline.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: v for k, v in document["mutation_sample"].items() if k != "missed_list"}, indent=1))
    return 0


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    try:
        if argv[:1] == ["mutation-sample"]:
            parser = argparse.ArgumentParser(prog="e2e-coverage.py mutation-sample")
            parser.add_argument("--baseline", required=True)
            parser.add_argument("--outcomes", required=True)
            parser.add_argument("--file", action="append", required=True)
            parser.add_argument("--test-filter", required=True)
            parser.add_argument("--command", required=True)
            parser.add_argument("--chosen-because", required=True)
            parser.add_argument("--wall-seconds", type=int, required=True)
            parser.add_argument("--time-cap-seconds", type=int, required=True)
            parser.add_argument("--stopped-by-cap", action="store_true")
            return mutation_sample(parser.parse_args(argv[1:]))
        parser = argparse.ArgumentParser(description="Measure end to end line coverage of mur and mur-roost.")
        parser.add_argument("--target-dir", required=True, help="the instrumented build's own target directory")
        parser.add_argument("--profiles", required=True, help="where profiles, JUnit reports and exports go")
        parser.add_argument("--out", required=True, help=f"the {FORMAT} JSON to write")
        parser.add_argument("--only", help="a filterset every part is intersected with")
        parser.add_argument("--keep-going", action="store_true", help="record failures instead of failing on them")
        parser.add_argument("--recheck-target-dir", help="an uninstrumented target directory to re-run failures on")
        mode = parser.add_mutually_exclusive_group()
        mode.add_argument("--build-only", action="store_true")
        mode.add_argument("--report-only", action="store_true")
        return measure(parser.parse_args(argv))
    except Unusable as error:
        print(f"e2e-coverage: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
