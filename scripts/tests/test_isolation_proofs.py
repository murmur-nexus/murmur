"""Unit tests for scripts/isolation-proofs.py.

    python3 -I -m unittest discover -s scripts/tests -v

They need no cargo, no network and no host isolation: the host facts the runner reads (its uid,
/proc/self/status) are injected, and the judge's repository is a scratch git repository. The
JUnit fixtures under fixtures/isolation/ are real nextest reports, trimmed: host-pass-junit.xml
from a run on a host that reaches scoped, capsule-shaped-junit.xml from the same tests run under
`systemd-run --user -p RestrictNamespaces=yes`, which stands in for a capsule's view of the kernel.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(os.path.dirname(HERE), "isolation-proofs.py")
FIXTURES = os.path.join(HERE, "fixtures", "isolation")
HOST_PASS = os.path.join(FIXTURES, "host-pass-junit.xml")
CAPSULE_SHAPED = os.path.join(FIXTURES, "capsule-shaped-junit.xml")

spec = importlib.util.spec_from_file_location("isolation_proofs", SCRIPT)
ip = importlib.util.module_from_spec(spec)
# dataclasses look their module up in sys.modules while the class is being built.
sys.modules[spec.name] = ip
spec.loader.exec_module(ip)

LANDLOCK_OUT = "sandbox::linux_integration_tests::kernel_tier_refuses_rename_out_of_the_workdir"
LANDLOCK_IN = "sandbox::linux_integration_tests::kernel_tier_refuses_rename_into_the_workdir"
LANDLOCK_TRUNCATE = "sandbox::linux_integration_tests::kernel_tier_truncate_outside_the_workdir_is_refused"
WORKER = "tools_worker_runs_under_scoped"

GOOD_TOML = f"""
version = 1

[[set]]
name = "landlock"
package = "capsule-runtime"
target = "lib"
filter = "test(={LANDLOCK_OUT}) | test(={LANDLOCK_IN}) | test(={LANDLOCK_TRUNCATE})"
needs = ["landlock", "user-namespace", "network-namespace"]
class = "scoped"
why = "Landlock refuses moves across the workdir's edge."

[[set]]
name = "wasm-guest-workers"
package = "murmur-cli"
target = "test:guest_workers"
filter = "test(={WORKER})"
needs = ["wasmtime", "seccomp"]
class = "scoped"
why = "Guest workers run confined."
"""

PROC_HOST = "Name:\tpython3\nUid:\t1000\t1000\t1000\t1000\nNoNewPrivs:\t0\nSeccomp:\t0\nSeccomp_filters:\t0\n"


def junit(cases: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<testsuites name="nextest-run" tests="1" failures="0" errors="0">\n'
        f'<testsuite name="capsule-runtime" tests="1">\n{cases}\n</testsuite>\n</testsuites>\n'
    )


def case(name: str, body: str = "", binary: str = "capsule-runtime") -> str:
    return f'<testcase name="{name}" classname="{binary}" time="0.25">{body}</testcase>'


class Scratch(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="isolation-proofs-test-")
        self.addCleanup(shutil.rmtree, self.dir, True)

    def write(self, name: str, text: str) -> str:
        path = os.path.join(self.dir, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def classification(self, text: str = GOOD_TOML) -> str:
        return self.write("isolation-tests.toml", text)


def run_main(argv: list[str], context=None) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = ip.main(argv, context)
    return code, out.getvalue(), err.getvalue()


class ClassificationTests(Scratch):
    def refused(self, text: str) -> str:
        path = self.classification(text)
        with self.assertRaises(ip.Refusal) as caught:
            ip.load_classification(path)
        message = str(caught.exception)
        self.assertIn(path, message)
        return message

    def test_a_good_file_loads_every_set(self) -> None:
        loaded = ip.load_classification(self.classification())
        self.assertEqual([s.name for s in loaded.sets], ["landlock", "wasm-guest-workers"])
        self.assertEqual(loaded.sets[0].binary_id, "capsule-runtime")
        self.assertEqual(loaded.sets[1].binary_id, "murmur-cli::guest_workers")
        self.assertEqual(len(loaded.sets[0].terms), 3)

    def test_the_repo_classification_ships_both_sets(self) -> None:
        repo_file = os.path.join(os.path.dirname(os.path.dirname(HERE)), ".config", "isolation-tests.toml")
        loaded = ip.load_classification(repo_file)
        landlock = loaded.by_name("landlock")
        workers = loaded.by_name("wasm-guest-workers")
        self.assertTrue(landlock.holds("capsule-runtime", LANDLOCK_OUT))
        self.assertTrue(workers.holds("murmur-cli::guest_workers", WORKER))

    def test_an_unknown_need_is_refused(self) -> None:
        message = self.refused(GOOD_TOML.replace('"seccomp"]', '"kvm"]'))
        self.assertIn('set "wasm-guest-workers"', message)
        self.assertIn('"kvm"', message)

    def test_a_bad_target_is_refused(self) -> None:
        message = self.refused(GOOD_TOML.replace('target = "lib"', 'target = "example:x"'))
        self.assertIn('set "landlock"', message)
        self.assertIn('"example:x"', message)

    def test_a_bad_filter_term_is_refused(self) -> None:
        message = self.refused(GOOD_TOML.replace(f'test(={WORKER})"', 'package(foo)"'))
        self.assertIn('set "wasm-guest-workers"', message)
        self.assertIn("package(foo)", message)

    def test_a_duplicate_name_is_refused(self) -> None:
        message = self.refused(GOOD_TOML.replace('name = "wasm-guest-workers"', 'name = "landlock"'))
        self.assertIn('set "landlock"', message)
        self.assertIn("already", message)

    def test_a_missing_field_is_refused(self) -> None:
        message = self.refused(GOOD_TOML.replace('class = "scoped"\nwhy = "Guest workers run confined."', 'why = "x"'))
        self.assertIn("class", message)
        self.assertIn("missing", message)

    def test_a_malformed_file_is_one_line_exit_2_without_a_traceback(self) -> None:
        path = self.classification(GOOD_TOML.replace('"seccomp"]', '"kvm"]'))
        code, out, err = run_main(["list", "--classification", path])
        self.assertEqual(code, 2)
        self.assertNotIn("Traceback", err)
        self.assertIn("kvm", err)


class FilterTests(unittest.TestCase):
    def test_an_exact_term_matches_only_its_name(self) -> None:
        terms = ip.parse_filter(f"test(={LANDLOCK_OUT})")
        self.assertTrue(ip.filter_matches(terms, LANDLOCK_OUT))
        self.assertFalse(ip.filter_matches(terms, LANDLOCK_OUT + "_too"))

    def test_a_regex_term_is_searched(self) -> None:
        terms = ip.parse_filter("test(/kernel_tier_refuses_/)")
        self.assertTrue(ip.filter_matches(terms, LANDLOCK_IN))
        self.assertFalse(ip.filter_matches(terms, LANDLOCK_TRUNCATE))

    def test_several_terms_join_with_a_pipe(self) -> None:
        text = f"test(={WORKER}) | test(/truncate_outside/)"
        terms = ip.parse_filter(text)
        self.assertEqual(len(terms), 2)
        self.assertTrue(ip.filter_matches(terms, WORKER))
        self.assertTrue(ip.filter_matches(terms, LANDLOCK_TRUNCATE))
        self.assertFalse(ip.filter_matches(terms, LANDLOCK_OUT))
        self.assertEqual(ip.filter_text(terms), text)

    def test_other_nextest_filters_are_refused(self) -> None:
        for text in ("package(foo)", "test(a) & test(b)", "not test(=a)", "test(=a) |", "test(~a)", ""):
            with self.subTest(text=text), self.assertRaises(ValueError):
                ip.parse_filter(text)


class TargetMappingTests(unittest.TestCase):
    def test_binary_ids_map_to_cargo_targets_and_back(self) -> None:
        for package, target, binary_id, args in (
            ("capsule-runtime", "lib", "capsule-runtime", ["--lib"]),
            ("murmur-cli", "test:guest_workers", "murmur-cli::guest_workers", ["--test", "guest_workers"]),
            ("murmur-cli", "bin:mur", "murmur-cli::bin/mur", ["--bin", "mur"]),
        ):
            with self.subTest(binary_id=binary_id):
                self.assertEqual(ip.binary_id_for(package, target), binary_id)
                self.assertEqual(ip.target_for_binary_id(binary_id), (package, target))
                self.assertEqual(ip.cargo_target_args(target), args)

    def test_an_example_binary_is_not_selectable(self) -> None:
        with self.assertRaises(ValueError):
            ip.target_for_binary_id("murmur-cli::example/x")


class JunitTests(Scratch):
    def test_the_host_run_fixture_passes_both_named_tests(self) -> None:
        cases = ip.parse_junit(HOST_PASS)
        self.assertEqual(cases[("capsule-runtime", LANDLOCK_OUT)].status, "passed")
        self.assertEqual(cases[("murmur-cli::guest_workers", WORKER)].status, "passed")
        self.assertTrue(all(c.status == "passed" for c in cases.values()))

    def test_the_capsule_shaped_fixture_is_skipped_for_the_host(self) -> None:
        cases = ip.parse_junit(CAPSULE_SHAPED)
        for key in (("capsule-runtime", LANDLOCK_OUT), ("murmur-cli::guest_workers", WORKER)):
            with self.subTest(test=key[1]):
                self.assertEqual(cases[key].status, "skipped_for_host")
                self.assertIn("[SKIP-HOST]", cases[key].detail)

    def test_a_failure_gives_failed_with_the_first_failure_line(self) -> None:
        body = (
            '<failure type="test failure with exit code 101">thread panicked</failure>'
            "<system-out>exit 21\n</system-out>"
            f"<system-err>\nthread '{LANDLOCK_OUT}' panicked at crates/capsule-runtime/src/sandbox.rs:10941:9:\n"
            "assertion `left == right` failed: mv and ln out of the workdir must both fail\n  left: 21\n right: 0\n"
            "</system-err>"
        )
        path = self.write("r.xml", junit(case(LANDLOCK_OUT, body)))
        got = ip.parse_junit(path)[("capsule-runtime", LANDLOCK_OUT)]
        self.assertEqual(got.status, "failed")
        self.assertIn("panicked at crates/capsule-runtime/src/sandbox.rs:10941:9:", got.detail)
        self.assertIn("mv and ln out of the workdir must both fail", got.detail)

    def test_a_flaky_failure_is_a_failure(self) -> None:
        body = '<flakyFailure type="test failure">first try</flakyFailure><system-out>ok</system-out>'
        path = self.write("r.xml", junit(case(LANDLOCK_OUT, body)))
        self.assertEqual(ip.parse_junit(path)[("capsule-runtime", LANDLOCK_OUT)].status, "failed")

    def test_a_failure_beats_a_skip_line(self) -> None:
        body = '<failure type="x">boom</failure><system-err>[SKIP-HOST] t: no</system-err>'
        path = self.write("r.xml", junit(case(LANDLOCK_OUT, body)))
        self.assertEqual(ip.parse_junit(path)[("capsule-runtime", LANDLOCK_OUT)].status, "failed")

    def test_an_unprefixed_skipping_line_is_a_host_skip(self) -> None:
        line = "skipping kernel_tier_refuses_rename_out_of_the_workdir: needs EnforcementTier::KernelFull"
        path = self.write("r.xml", junit(case(LANDLOCK_OUT, f"<system-err>{line}\n</system-err>")))
        got = ip.parse_junit(path)[("capsule-runtime", LANDLOCK_OUT)]
        self.assertEqual(got.status, "skipped_for_host")
        self.assertEqual(got.detail, line)

    def test_a_mention_of_skipping_mid_line_is_not_a_skip(self) -> None:
        path = self.write("r.xml", junit(case(LANDLOCK_OUT, "<system-out>not skipping anything: fine\n</system-out>")))
        self.assertEqual(ip.parse_junit(path)[("capsule-runtime", LANDLOCK_OUT)].status, "passed")

    def test_an_absent_test_is_not_run(self) -> None:
        path = self.write("r.xml", junit(case(LANDLOCK_OUT)))
        cases = ip.parse_junit(path)
        group = ip.Group("capsule-runtime", "lib", [], ip.parse_filter(f"test(={LANDLOCK_OUT}) | test(={LANDLOCK_IN})"), ["landlock", "landlock"])
        rows = {r["name"]: r for r in ip.tests_for_group(group, cases, "absent from the JUnit report")}
        self.assertEqual(rows[LANDLOCK_OUT]["status"], "passed")
        self.assertEqual(rows[LANDLOCK_IN]["status"], "not_run")
        self.assertEqual(rows[LANDLOCK_IN]["set"], "landlock")

    def test_a_regex_term_that_selects_nothing_is_not_run(self) -> None:
        group = ip.Group("capsule-runtime", "lib", [], ip.parse_filter("test(/no_such_/)"), ["landlock"])
        rows = ip.tests_for_group(group, None, "nextest wrote no JUnit report (exit 101)")
        self.assertEqual([r["status"] for r in rows], ["not_run"])
        self.assertIn("exit 101", rows[0]["detail"])


class VerdictTests(unittest.TestCase):
    def rows(self, *statuses: str) -> list[dict]:
        return [{"status": s} for s in statuses]

    def test_fail_beats_no_evidence_beats_pass(self) -> None:
        for statuses, verdict in (
            (("passed", "passed"), "pass"),
            (("passed", "skipped_for_host"), "no_evidence"),
            (("passed", "not_run"), "no_evidence"),
            (("skipped_for_host", "failed", "passed"), "fail"),
            ((), "no_evidence"),
        ):
            with self.subTest(statuses=statuses):
                got = ip.verdict_of(ip.totals_of(self.rows(*statuses)))
                self.assertEqual(got, verdict)

    def test_exit_codes(self) -> None:
        self.assertEqual(ip.VERDICT_EXIT, {"pass": 0, "fail": 1, "no_evidence": 3})
        self.assertEqual(ip.EXIT_REFUSED, 2)

    def test_the_capsule_shaped_report_is_no_evidence_exit_3(self) -> None:
        classification = ip.validate_classification(__import__("tomllib").loads(GOOD_TOML), "x")
        cases = ip.parse_junit(CAPSULE_SHAPED)
        rows = []
        for found in classification:
            group = ip.Group(found.package, found.target, [], found.terms, [found.name] * len(found.terms))
            rows += ip.tests_for_group(group, cases, "absent")
        self.assertTrue(rows)
        self.assertTrue(all(r["status"] == "skipped_for_host" for r in rows), rows)
        verdict = ip.verdict_of(ip.totals_of(rows))
        self.assertEqual(verdict, "no_evidence")
        self.assertEqual(ip.VERDICT_EXIT[verdict], 3)


class EnvironmentTests(unittest.TestCase):
    def test_the_allowlist_keeps_cargo_and_drops_secrets(self) -> None:
        environ = {
            "PATH": "/usr/bin",
            "HOME": "/home/op",
            "CARGO_HOME": "/home/op/.cargo",
            "RUSTUP_TOOLCHAIN": "stable",
            "RUST_BACKTRACE": "1",
            "GITHUB_TOKEN": "sekrit-1",
            "ANTHROPIC_API_KEY": "sekrit-2",
            "CARGO_REGISTRY_TOKEN": "sekrit-3",
            "MURMUR_HOME": "/elsewhere",
            "CARGO_secret_thing": "sekrit-4",
        }
        kept, dropped = ip.filter_environment(environ)
        self.assertEqual(set(kept), {"PATH", "HOME", "CARGO_HOME", "RUSTUP_TOOLCHAIN", "RUST_BACKTRACE"})
        self.assertEqual(
            dropped,
            sorted(["GITHUB_TOKEN", "ANTHROPIC_API_KEY", "CARGO_REGISTRY_TOKEN", "MURMUR_HOME", "CARGO_secret_thing"]),
        )
        self.assertFalse(any("sekrit" in name for name in dropped))


class RefusalTests(Scratch):
    def test_root_is_refused(self) -> None:
        self.assertIn("root", ip.host_refusal(0, PROC_HOST))

    def test_no_new_privs_is_refused_as_a_sandbox(self) -> None:
        message = ip.host_refusal(1000, PROC_HOST.replace("NoNewPrivs:\t0", "NoNewPrivs:\t1"))
        self.assertIn("proves the outer sandbox, not the code", message)

    def test_a_seccomp_filter_is_refused_as_a_sandbox(self) -> None:
        message = ip.host_refusal(1000, PROC_HOST.replace("Seccomp:\t0", "Seccomp:\t2"))
        self.assertIn("Seccomp: 2", message)

    def test_an_unreadable_status_is_refused(self) -> None:
        self.assertIsNotNone(ip.host_refusal(1000, None))
        self.assertIsNotNone(ip.host_refusal(1000, "Name:\tx\n"))

    def test_the_host_is_accepted(self) -> None:
        self.assertIsNone(ip.host_refusal(1000, PROC_HOST))

    def context(self, uid: int = 1000, status: str = PROC_HOST) -> object:
        def no_command(name: str) -> bool:
            raise AssertionError(f"a refused run must not look for {name}")

        return ip.RunContext(uid=uid, proc_status=status, environ={"PATH": "/usr/bin"}, has_command=no_command)

    def test_a_refusal_removes_the_evidence_first(self) -> None:
        cases = (
            (self.context(uid=0), ["--set", "landlock"], "root"),
            (self.context(status=PROC_HOST.replace("Seccomp:\t0", "Seccomp:\t2")), ["--set", "landlock"], "sandbox"),
            (self.context(), ["--set", "no-such-set"], "landlock, wasm-guest-workers"),
            (self.context(), ["--handover", os.path.join(self.dir, "nope.json")], "handover"),
        )
        classification = self.classification()
        for context, selection, reason in cases:
            with self.subTest(reason=reason):
                evidence = self.write("ev.json", '{"stale": true}')
                stale_junit = self.write("ev.junit-1.xml", "<old/>")
                argv = ["run", *selection, "--evidence", evidence, "--classification", classification, "--repo", HERE]
                code, _, err = run_main(argv, context)
                self.assertEqual(code, 2, err)
                self.assertIn(reason, err)
                self.assertFalse(os.path.exists(evidence))
                self.assertFalse(os.path.exists(stale_junit))

    def test_evidence_is_written_atomically(self) -> None:
        path = os.path.join(self.dir, "ev.json")
        ip.write_json_atomically(path, {"format": ip.EVIDENCE_FORMAT})
        with open(path, encoding="utf-8") as handle:
            self.assertEqual(json.load(handle), {"format": ip.EVIDENCE_FORMAT})
        with self.assertRaises(TypeError):
            ip.write_json_atomically(os.path.join(self.dir, "bad.json"), {"x": object()})
        self.assertEqual(sorted(os.listdir(self.dir)), ["ev.json"])


class GitRepo(Scratch):
    """A scratch repository with one commit, and evidence built from a fixture against it."""

    def setUp(self) -> None:
        super().setUp()
        self.repo = os.path.join(self.dir, "repo")
        os.makedirs(self.repo)
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.git_env = env

        def git(*args: str) -> None:
            subprocess.run(
                ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-C", self.repo, *args],
                check=True,
                capture_output=True,
                env=env,
            )

        self.git = git
        git("init", "-q")
        with open(os.path.join(self.repo, "README.md"), "w", encoding="utf-8") as handle:
            handle.write("scratch\n")
        git("add", "README.md")
        git("commit", "-q", "-m", "one")
        self.toml = self.classification()

    def evidence(self, report: str, sets=("landlock", "wasm-guest-workers"), name: str = "ev.json") -> str:
        """Evidence as `run` writes it, from `report` standing in for both nextest runs."""
        classification = ip.load_classification(self.toml)
        commit, tree = ip.tree_state(self.repo)
        _, groups = ip.select_sets(classification, list(sets))
        junit_copy = os.path.join(self.dir, name.replace(".json", ".junit-1.xml"))
        shutil.copyfile(report, junit_copy)
        cases = ip.parse_junit(junit_copy)
        tests: list[dict] = []
        runs = []
        for group in groups:
            tests += ip.tests_for_group(group, cases, "absent from the JUnit report")
            runs.append(
                {
                    "package": group.package,
                    "target": group.target,
                    "filter": ip.filter_text(group.terms),
                    "argv": [],
                    "exit": 0,
                    "junit": {"path": junit_copy, "sha256": ip.sha256_file(junit_copy)},
                }
            )
        totals = ip.totals_of(tests)
        record = {
            "format": ip.EVIDENCE_FORMAT,
            "written_at": "2026-11-20T00:00:00Z",
            "repo": self.repo,
            "commit": commit,
            "tree": tree,
            "classification": {"path": classification.path, "sha256": classification.sha256},
            "request": {"sets": list(sets), "handover": None},
            "host": {},
            "runs": runs,
            "tests": tests,
            "totals": totals,
            "verdict": ip.verdict_of(totals),
        }
        path = os.path.join(self.dir, name)
        ip.write_json_atomically(path, record)
        return path

    def judge(self, evidence: str, *extra: str) -> tuple[int, str]:
        code, out, err = run_main(["judge", evidence, "--repo", self.repo, "--classification", self.toml, *extra])
        return code, out + err


class JudgeTests(GitRepo):
    REQUIRE = ("--require-set", "landlock", "--require-set", "wasm-guest-workers")

    def test_passing_evidence_passes(self) -> None:
        code, out = self.judge(self.evidence(HOST_PASS), *self.REQUIRE)
        self.assertEqual(code, 0, out)
        self.assertIn(f"PASS capsule-runtime {LANDLOCK_OUT}", out)
        self.assertIn(f"PASS murmur-cli::guest_workers {WORKER}", out)
        self.assertIn("JUDGE: pass (4 tests)", out)

    def test_a_failed_test_fails_with_its_line(self) -> None:
        body = '<failure type="x"/><system-err>thread \'t\' panicked at a.rs:1:2:\nmust both fail\n</system-err>'
        report = self.write("fail.xml", junit(case(LANDLOCK_OUT, body)))
        code, out = self.judge(self.evidence(report, sets=("landlock",)), "--require-set", "landlock")
        self.assertEqual(code, 1, out)
        self.assertIn(f"FAIL capsule-runtime {LANDLOCK_OUT}: thread 't' panicked at a.rs:1:2: must both fail", out)
        self.assertIn("JUDGE: fail", out)

    def test_the_capsule_shaped_report_is_no_evidence(self) -> None:
        code, out = self.judge(self.evidence(CAPSULE_SHAPED), *self.REQUIRE)
        self.assertEqual(code, 1, out)
        self.assertIn(f"NO-EVIDENCE capsule-runtime {LANDLOCK_OUT}: skipped for the host: [SKIP-HOST]", out)
        self.assertIn(f"NO-EVIDENCE murmur-cli::guest_workers {WORKER}: skipped for the host: [SKIP-HOST]", out)
        self.assertNotIn("PASS ", out)
        self.assertIn("JUDGE: fail", out)

    def test_a_missing_required_set_or_test_fails(self) -> None:
        evidence = self.evidence(HOST_PASS, sets=("landlock",))
        code, out = self.judge(evidence, *self.REQUIRE)
        self.assertEqual(code, 1, out)
        self.assertIn("MISSING set wasm-guest-workers: required but not in the evidence", out)
        missing = "capsule-runtime::sandbox::linux_integration_tests::no_such_test"
        code, out = self.judge(evidence, "--require-test", missing)
        self.assertEqual(code, 1, out)
        self.assertIn(f"MISSING test {missing}: required but not in the evidence", out)
        code, out = self.judge(evidence, "--require-test", f"capsule-runtime::{LANDLOCK_OUT}")
        self.assertEqual(code, 0, out)

    def test_a_required_set_with_a_test_absent_from_the_report_fails(self) -> None:
        report = self.write("partial.xml", junit(case(LANDLOCK_OUT)))
        code, out = self.judge(self.evidence(report, sets=("landlock",)), "--require-set", "landlock")
        self.assertEqual(code, 1, out)
        self.assertIn(f"NO-EVIDENCE capsule-runtime {LANDLOCK_IN}: not run", out)

    def test_another_commit_is_unusable(self) -> None:
        evidence = self.evidence(HOST_PASS)
        self.git("commit", "-q", "--allow-empty", "-m", "two")
        code, out = self.judge(evidence)
        self.assertEqual(code, 2, out)
        self.assertIn("was taken at commit", out)

    def test_a_changed_tree_is_unusable(self) -> None:
        evidence = self.evidence(HOST_PASS)
        with open(os.path.join(self.repo, "README.md"), "a", encoding="utf-8") as handle:
            handle.write("more\n")
        code, out = self.judge(evidence)
        self.assertEqual(code, 2, out)
        self.assertIn("the tree changed since the evidence was taken", out)
        self.git("checkout", "--", "README.md")
        code, out = self.judge(evidence, *self.REQUIRE)
        self.assertEqual(code, 0, out)

    def test_a_changed_junit_report_is_unusable(self) -> None:
        evidence = self.evidence(HOST_PASS)
        with open(os.path.join(self.dir, "ev.junit-1.xml"), "a", encoding="utf-8") as handle:
            handle.write("<!-- edited -->\n")
        code, out = self.judge(evidence)
        self.assertEqual(code, 2, out)
        self.assertIn("no longer matches its recorded sha256", out)

    def test_a_missing_junit_report_is_unusable(self) -> None:
        evidence = self.evidence(HOST_PASS)
        os.unlink(os.path.join(self.dir, "ev.junit-1.xml"))
        code, out = self.judge(evidence)
        self.assertEqual(code, 2, out)
        self.assertIn("is missing", out)

    def test_an_edited_status_is_unusable(self) -> None:
        evidence = self.evidence(CAPSULE_SHAPED)
        with open(evidence, encoding="utf-8") as handle:
            record = json.load(handle)
        for test in record["tests"]:
            test["status"], test["detail"] = "passed", None
        ip.write_json_atomically(evidence, record)
        code, out = self.judge(evidence)
        self.assertEqual(code, 2, out)
        self.assertIn("but its JUnit report says skipped_for_host", out)

    def test_a_wrong_format_is_unusable(self) -> None:
        evidence = self.evidence(HOST_PASS)
        with open(evidence, encoding="utf-8") as handle:
            record = json.load(handle)
        record["format"] = "murmur.isolation-evidence/0"
        ip.write_json_atomically(evidence, record)
        code, out = self.judge(evidence)
        self.assertEqual(code, 2, out)
        self.assertIn("format", out)
        self.assertEqual(self.judge(self.write("junk.json", "not json"))[0], 2)

    def test_another_classification_is_unusable_for_a_required_set(self) -> None:
        evidence = self.evidence(HOST_PASS)
        self.toml = self.write("other.toml", GOOD_TOML.replace("Guest workers run confined.", "Changed."))
        code, out = self.judge(evidence, "--require-set", "landlock")
        self.assertEqual(code, 2, out)
        self.assertIn("different classification", out)


class HandoverTests(Scratch):
    def test_the_capsule_shaped_report_hands_over_every_skipped_test(self) -> None:
        out_path = os.path.join(self.dir, "handover.json")
        code, out, err = run_main(["handover", CAPSULE_SHAPED, "--out", out_path, "--classification", self.classification()])
        self.assertEqual(code, 0, err)
        with open(out_path, encoding="utf-8") as handle:
            record = json.load(handle)
        self.assertEqual(record["format"], ip.HANDOVER_FORMAT)
        self.assertEqual(record["source"]["sha256"], ip.sha256_file(CAPSULE_SHAPED))
        by_name = {t["name"]: t for t in record["tests"]}
        self.assertEqual(by_name[LANDLOCK_OUT]["set"], "landlock")
        self.assertEqual(by_name[WORKER]["set"], "wasm-guest-workers")
        self.assertIn("[SKIP-HOST]", by_name[LANDLOCK_OUT]["line"])
        unclassified = [t for t in record["tests"] if t["set"] is None]
        self.assertEqual(len(unclassified), 1, record["tests"])
        self.assertEqual(record["unclassified"], 1)
        self.assertIn("1 unclassified", out)

    def test_a_report_without_skips_hands_over_nothing(self) -> None:
        out_path = os.path.join(self.dir, "handover.json")
        code, out, _ = run_main(["handover", HOST_PASS, "--out", out_path, "--classification", self.classification()])
        self.assertEqual(code, 0)
        self.assertIn("0 tests printed a host-skip line", out)

    def test_a_handover_selects_its_tests_by_binary(self) -> None:
        out_path = os.path.join(self.dir, "handover.json")
        run_main(["handover", CAPSULE_SHAPED, "--out", out_path, "--classification", self.classification()])
        classification = ip.load_classification(self.classification())
        chosen, groups = ip.select_handover(classification, ip.load_handover(out_path))
        self.assertEqual(sorted(chosen), ["landlock", "wasm-guest-workers"])
        by_binary = {g.binary_id: g for g in groups}
        self.assertEqual(set(by_binary), {"capsule-runtime", "murmur-cli::guest_workers"})
        self.assertIn(f"test(={LANDLOCK_OUT})", ip.filter_text(by_binary["capsule-runtime"].terms))
        self.assertIn(None, by_binary["capsule-runtime"].term_sets)


if __name__ == "__main__":
    unittest.main()
