"""Unit tests for scripts/test-classes.py.

    python3 -I -m unittest discover -s scripts/tests -p 'test_test_classes*.py' -v

The nextest listing and JUnit fixtures under fixtures/test-classes/ are trimmed from real
output (fixtures/test-classes/README.md says where each came from). No test runs cargo.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures" / "test-classes"
SCRIPT = HERE.parent / "test-classes.py"

spec = importlib.util.spec_from_file_location("test_classes_tool", SCRIPT)
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)

LIST_FIXTURE = (FIXTURES / "nextest-list.json").read_text(encoding="utf-8")

# The fixture's tests, sorted into classes the way the rules in fixtures/nextest.toml would.
CAPSULE_BINARIES = {"murmur-artifact", "mur-roost::version_output"}
ISOLATION_BINARIES = {"murmur-cli::guest_workers", "murmur-cli::deploy_precompile"}
E2E_BINARIES = {"mur-roost::version_output", "murmur-cli::guest_workers", "murmur-cli::deploy_precompile"}


def listing_for(selected) -> dict:
    """A class or e2e listing of the fixture: every test, `matched` when `selected` says so."""
    out = {}
    for key, case in tool.parse_list_json(LIST_FIXTURE).items():
        out[key] = dict(case, matched=bool(selected(key)))
    return out


def results(capsule=CAPSULE_BINARIES, isolation=ISOLATION_BINARIES, host=frozenset(), e2e=E2E_BINARIES,
            extra_configs=None) -> dict:
    full = tool.parse_list_json(LIST_FIXTURE)
    base = {
        "all": {k: dict(v, matched=True) for k, v in full.items()},
        "capsule": listing_for(lambda key: key[0] in capsule),
        "isolation": listing_for(lambda key: key[0] in isolation),
        "host": listing_for(lambda key: key[0] in host),
        "e2e": listing_for(lambda key: key[0] in e2e),
    }
    empty = {which: {} for which in ("all", "capsule", "isolation", "host", "e2e")}
    out = {"beta-all": base, "default": empty}
    out.update(extra_configs or {})
    return out


class Workspace:
    """A scratch copy of the three config files, with the tool's paths pointed at it."""

    def __init__(self, testcase: unittest.TestCase):
        self.dir = Path(tempfile.mkdtemp(prefix="test-classes-"))
        testcase.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        shutil.copy(FIXTURES / "nextest.toml", self.dir / "nextest.toml")
        shutil.copy(FIXTURES / "test-classes.toml", self.dir / "test-classes.toml")
        self.list = self.dir / "test-classes.tsv"
        for name, value in (
            ("NEXTEST_CONFIG", self.dir / "nextest.toml"),
            ("RULES", self.dir / "test-classes.toml"),
            ("LIST", self.list),
        ):
            patcher = mock.patch.object(tool, name, value)
            patcher.start()
            testcase.addCleanup(patcher.stop)
        for name, value in (("nextest_version", lambda: "0.9.148"),):
            patcher = mock.patch.object(tool, name, value)
            patcher.start()
            testcase.addCleanup(patcher.stop)

    def listings(self, value):
        return mock.patch.object(tool, "listings", lambda rules, filters: value)


def run_main(argv) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err), mock.patch.dict(
        os.environ, {"GITHUB_STEP_SUMMARY": "", "GITHUB_ACTIONS": ""}
    ):
        try:
            code = tool.main(argv)
        except SystemExit as exit_:
            code = exit_.code
    return code, out.getvalue(), err.getvalue()


class ParseListJson(unittest.TestCase):
    def test_reads_binary_ids_kinds_ignored_and_filter_match_from_real_output(self):
        tests = tool.parse_list_json(LIST_FIXTURE)
        self.assertIn(("murmur-artifact", "artifact::tests::load_manifest_reads_packed_entry"), tests)
        lib = tests[("murmur-artifact", "artifact::tests::load_manifest_reads_packed_entry")]
        self.assertEqual(lib["kind"], "lib")
        self.assertFalse(lib["ignored"])
        self.assertTrue(lib["matched"])
        kinds = {case["kind"] for case in tests.values()}
        self.assertEqual(kinds, {"lib", "test"})
        self.assertTrue(any(case["ignored"] for case in tests.values()), "the fixture holds an ignored test")

    def test_a_filtered_listing_holds_only_the_selected_tests(self):
        # nextest 0.9.148 leaves a test the filter does not select out of `testcases`; the
        # unselected binary's suite is still there, empty.
        tests = tool.parse_list_json((FIXTURES / "nextest-list-filtered.json").read_text(encoding="utf-8"))
        self.assertTrue(tests)
        self.assertTrue(all(key[0] == "murmur-cli::unconfined" and case["matched"] for key, case in tests.items()))

    def test_a_mismatch_status_reads_as_not_matched(self):
        data = json.loads(LIST_FIXTURE)
        suite = data["rust-suites"]["murmur-artifact"]
        name = next(iter(suite["testcases"]))
        suite["testcases"][name]["filter-match"] = {"status": "mismatch", "reason": "expression"}
        tests = tool.parse_list_json(json.dumps(data))
        self.assertFalse(tests[("murmur-artifact", name)]["matched"])

    def test_malformed_output_is_unusable(self):
        for text in ("not json", "{}", '{"rust-suites": {"x": {"binary-id": "x"}}}'):
            with self.assertRaises(tool.Unusable):
                tool.parse_list_json(text)


class Classify(unittest.TestCase):
    def setUp(self):
        self.rules = tool.load_rules(FIXTURES / "test-classes.toml")

    def test_every_test_gets_one_class_and_a_kind(self):
        rows, findings = tool.classify(self.rules, results())
        self.assertEqual(findings, [])
        self.assertEqual(len(rows), len(tool.parse_list_json(LIST_FIXTURE)))
        by_binary = {(r["binary_id"], r["class"], r["kind"]) for r in rows.values()}
        self.assertIn(("murmur-artifact", "capsule", "unit"), by_binary)
        self.assertIn(("mur-roost::version_output", "capsule", "e2e"), by_binary)
        self.assertIn(("murmur-cli::guest_workers", "isolation", "e2e"), by_binary)

    def test_a_test_no_class_selects_is_unclassified_and_named(self):
        rows, findings = tool.classify(self.rules, results(capsule={"murmur-artifact"}))
        unclassified = [f for f in findings if f.startswith("unclassified:")]
        self.assertTrue(unclassified)
        self.assertTrue(all("mur-roost::version_output" in f for f in unclassified))
        self.assertIn("binary_id(mur-roost::version_output)", unclassified[0])
        self.assertIn("generate", unclassified[0])

    def test_a_test_two_classes_select_is_an_overlap_naming_both(self):
        rows, findings = tool.classify(self.rules, results(host={"mur-roost::version_output"}))
        overlap = [f for f in findings if f.startswith("overlap:")]
        self.assertTrue(overlap)
        self.assertIn("class-capsule and class-host", overlap[0])
        self.assertIn("mur-roost::version_output", overlap[0])

    def test_an_e2e_filterset_selecting_a_unit_test_is_a_finding(self):
        rows, findings = tool.classify(self.rules, results(e2e=E2E_BINARIES | {"murmur-artifact"}))
        self.assertTrue(any(f.startswith("e2e-unit:") for f in findings))

    def test_a_test_in_two_configurations_takes_the_first_and_must_agree(self):
        second = results()["beta-all"]
        rows, findings = tool.classify(self.rules, results(extra_configs={"default": second}))
        self.assertEqual(findings, [])
        self.assertEqual({r["config"] for r in rows.values()}, {"beta-all"})
        disagree = results(capsule={"murmur-artifact"}, isolation=ISOLATION_BINARIES | {"mur-roost::version_output"})
        rows, findings = tool.classify(self.rules, results(extra_configs={"default": disagree["beta-all"]}))
        self.assertTrue(any(f.startswith("config-mismatch:") for f in findings))

    def test_a_test_only_the_second_configuration_lists_is_marked_with_it(self):
        only_default = results()["beta-all"]
        key = ("murmur-cli::new_roster", "the_default_build_has_only_the_roster_form")
        case = {"kind": "test", "ignored": False, "matched": True}
        for which in ("all", "capsule", "isolation", "host", "e2e"):
            only_default[which] = dict(only_default[which])
            only_default[which][key] = dict(case, matched=which not in ("capsule", "host"))
        rows, findings = tool.classify(self.rules, results(extra_configs={"default": only_default}))
        self.assertEqual(findings, [])
        self.assertEqual(rows[key]["config"], "default")
        self.assertEqual((rows[key]["class"], rows[key]["kind"]), ("isolation", "e2e"))


class ListFile(unittest.TestCase):
    def setUp(self):
        self.rules = tool.load_rules(FIXTURES / "test-classes.toml")
        self.rows, _ = tool.classify(self.rules, results())
        self.dir = Path(tempfile.mkdtemp(prefix="test-classes-"))
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def test_round_trip_is_sorted_and_byte_identical(self):
        text = tool.render_tsv(self.rows, self.rules, "0.9.148")
        path = self.dir / "list.tsv"
        path.write_text(text, encoding="utf-8")
        back = tool.read_tsv(path)
        self.assertEqual(list(back), sorted(self.rows))
        for key, row in back.items():
            for column in tool.COLUMNS:
                self.assertEqual(row[column], self.rows[key][column])
        self.assertEqual(tool.render_tsv(back, self.rules, "0.9.148"), text)
        lines = text.splitlines()
        self.assertEqual(lines[0], "# format: murmur.test-classes/1")
        self.assertIn("# generated by: python3 -I scripts/test-classes.py generate", lines)
        self.assertIn("binary_id\ttest\tclass\tkind\tconfig\tignored", lines)
        self.assertTrue(any(line.startswith("# configuration beta-all: ") for line in lines))
        self.assertTrue(any(line.startswith("# nextest: ") for line in lines))

    def test_unsorted_duplicate_or_misheaded_lists_are_unusable(self):
        text = tool.render_tsv(self.rows, self.rules, "0.9.148")
        header, body = text.split("binary_id\ttest\tclass\tkind\tconfig\tignored\n")
        rows = body.splitlines()
        cases = {
            "unsorted": header + "binary_id\ttest\tclass\tkind\tconfig\tignored\n" + "\n".join(reversed(rows)) + "\n",
            "duplicate": text + rows[0] + "\n",
            "format": text.replace("murmur.test-classes/1", "murmur.test-classes/9"),
            "columns": text.replace("binary_id\ttest", "binary\ttest"),
            "class": text.replace("\tcapsule\t", "\tcupboard\t", 1),
        }
        for name, bad in cases.items():
            path = self.dir / f"{name}.tsv"
            path.write_text(bad, encoding="utf-8")
            with self.subTest(name), self.assertRaises(tool.Unusable):
                tool.read_tsv(path)
        with self.assertRaises(tool.Unusable):
            tool.read_tsv(self.dir / "missing.tsv")

    def test_drift_names_added_removed_and_reclassified_rows(self):
        listed = {k: dict(v) for k, v in self.rows.items()}
        first, second, third = sorted(listed)[:3]
        del listed[first]
        listed[second]["class"] = "host"
        listed[("gone", "test")] = dict(listed[third], binary_id="gone", test="test")
        changes = tool.drift(self.rows, listed)
        self.assertTrue(any(c.startswith(f"added: {first[0]} {first[1]}") for c in changes))
        self.assertTrue(any(c.startswith(f"reclassified: {second[0]} {second[1]}: class host -> ") for c in changes))
        self.assertTrue(any(c.startswith("removed: gone test") for c in changes))
        self.assertEqual(len(changes), 3)


class Audit(unittest.TestCase):
    """`audit` on each real JUnit excerpt, against a list holding exactly their tests."""

    def setUp(self):
        self.listed = {}
        for name in ("junit-pass.xml", "junit-failure.xml", "junit-skip-host.xml", "junit-proves-nothing.xml",
                     "junit-skipping.xml", "junit-skip-other.xml"):
            import xml.etree.ElementTree as ElementTree

            for case in ElementTree.parse(FIXTURES / name).getroot().iter("testcase"):
                key = (case.get("classname"), case.get("name"))
                self.listed[key] = {"binary_id": key[0], "test": key[1], "class": "capsule", "kind": "unit",
                                    "config": "beta-all", "ignored": False}

    def audit(self, name, cls="capsule", listed=None):
        return tool.audit_report(FIXTURES / name, cls, self.listed if listed is None else listed)

    def test_a_passing_report_is_clean(self):
        problems, totals = self.audit("junit-pass.xml")
        self.assertEqual(problems, [])
        self.assertGreater(totals["run"], 0)
        self.assertEqual((totals["failed"], totals["host-skips"], totals["outside-class"]), (0, 0, 0))

    def test_a_failure_is_named(self):
        problems, totals = self.audit("junit-failure.xml")
        self.assertGreater(totals["failed"], 0)
        self.assertTrue(any(p.startswith("failed: ") for p in problems))

    def test_each_host_skip_spelling_is_a_problem(self):
        for name, needle in (
            ("junit-skip-host.xml", "[SKIP-HOST]"),
            ("junit-proves-nothing.xml", "SKIP — PROVES NOTHING"),
            ("junit-skipping.xml", "skipping"),
        ):
            with self.subTest(name):
                problems, totals = self.audit(name)
                self.assertGreater(totals["host-skips"], 0)
                lines = [p for p in problems if p.startswith("host-skip: ")]
                self.assertTrue(lines and needle in lines[0], problems)

    def test_a_missing_fixture_skip_is_counted_not_failed(self):
        problems, totals = self.audit("junit-skip-other.xml")
        self.assertGreater(totals["other-skips"], 0)
        self.assertEqual(problems, [])

    def test_a_test_of_another_class_or_none_is_outside_class(self):
        problems, totals = self.audit("junit-pass.xml", cls="isolation")
        self.assertEqual(totals["outside-class"], totals["run"])
        self.assertTrue(all("is class capsule, not isolation" in p for p in problems))
        problems, totals = self.audit("junit-pass.xml", listed={})
        self.assertTrue(all("not in .config/test-classes.tsv" in p for p in problems))

    def test_a_missing_or_malformed_report_is_unusable(self):
        with tempfile.NamedTemporaryFile("w", suffix=".xml") as bad:
            bad.write("<testsuites><testsuite>")
            bad.flush()
            for path in (Path(bad.name), FIXTURES / "nope.xml"):
                with self.subTest(str(path)), self.assertRaises(tool.Unusable):
                    tool.audit_report(path, "capsule", self.listed)


class ExitCodes(unittest.TestCase):
    """Each subcommand's exit codes through main(), with the listing stubbed from the fixture."""

    def setUp(self):
        self.ws = Workspace(self)

    def generate(self):
        with self.ws.listings(results()):
            code, out, err = run_main(["generate"])
        self.assertEqual(code, 0, out + err)
        return out

    def test_generate_writes_the_list_and_check_strict_passes(self):
        out = self.generate()
        self.assertIn("wrote", out)
        first = self.ws.list.read_bytes()
        self.generate()
        self.assertEqual(self.ws.list.read_bytes(), first, "regenerating an unchanged suite is byte-identical")
        with self.ws.listings(results()):
            code, out, _ = run_main(["check", "--strict"])
        self.assertEqual(code, 0, out)
        self.assertIn("unclassified 0 overlap 0 drift 0", out)
        self.assertIn("class capsule", out)

    def test_generate_refuses_on_an_unclassified_test_and_leaves_the_list(self):
        self.generate()
        before = self.ws.list.read_bytes()
        with self.ws.listings(results(capsule={"murmur-artifact"})):
            code, out, _ = run_main(["generate"])
        self.assertEqual(code, 1)
        self.assertIn("unclassified: mur-roost::version_output", out)
        self.assertEqual(self.ws.list.read_bytes(), before)

    def test_check_fails_on_unclassified_and_overlap_strict_or_not(self):
        self.generate()
        for broken in (results(capsule={"murmur-artifact"}), results(host={"murmur-artifact"})):
            for argv in (["check"], ["check", "--strict"]):
                with self.subTest(argv=argv), self.ws.listings(broken):
                    code, out, _ = run_main(argv)
                self.assertEqual(code, 1, out)
                self.assertIn("drift 0", out, "an undecided test is a finding, not drift too")

    def test_audit_of_a_clean_capsule_report_exits_0(self):
        import xml.etree.ElementTree as ElementTree

        rules = tool.load_rules()
        rows = {}
        for case in ElementTree.parse(FIXTURES / "junit-pass.xml").getroot().iter("testcase"):
            key = (case.get("classname"), case.get("name"))
            rows[key] = {"binary_id": key[0], "test": key[1], "class": "capsule", "kind": "e2e",
                         "config": "beta-all", "ignored": False}
        self.ws.list.write_text(tool.render_tsv(rows, rules, "0.9.148"), encoding="utf-8")
        code, out, _ = run_main(["audit", str(FIXTURES / "junit-pass.xml"), "--class", "capsule"])
        self.assertEqual(code, 0, out)
        self.assertIn("failed 0 host-skips 0 other-skips 0 outside-class 0", out)
        code, out, _ = run_main(["audit", str(FIXTURES / "junit-skip-host.xml"), "--class", "capsule"])
        self.assertEqual(code, 1, out)

    def test_drift_warns_and_fails_only_with_strict(self):
        self.generate()
        lines = self.ws.list.read_text(encoding="utf-8").splitlines(keepends=True)
        removed = lines.pop()
        self.ws.list.write_text("".join(lines), encoding="utf-8")
        binary_id, test = removed.split("\t")[:2]
        with self.ws.listings(results()):
            code, out, _ = run_main(["check"])
        self.assertEqual(code, 0, out)
        self.assertIn(f"drift: added: {binary_id} {test}", out)
        self.assertIn("python3 -I scripts/test-classes.py generate", out)
        with self.ws.listings(results()):
            code, out, _ = run_main(["check", "--strict"])
        self.assertEqual(code, 1, out)

    def test_check_appends_to_the_step_summary(self):
        self.generate()
        summary = self.ws.dir / "summary.md"
        with self.ws.listings(results()), mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(tool.main(["check"]), 0)
        self.assertIn("### Test classes", summary.read_text(encoding="utf-8"))

    def test_unusable_inputs_exit_2(self):
        with self.ws.listings(results()):
            code, _, err = run_main(["check"])
        self.assertEqual(code, 2, "no list yet")
        self.assertIn("does not exist", err)
        self.assertEqual(run_main(["filter", "cupboard"])[0], 2)
        self.assertEqual(run_main(["list", "--class", "cupboard"])[0], 2)
        self.assertEqual(run_main(["list", "--kind", "smoke"])[0], 2)
        self.assertEqual(run_main(["audit", str(FIXTURES / "nope.xml"), "--class", "capsule"])[0], 2)
        self.assertEqual(run_main(["frobnicate"])[0], 2)
        (self.ws.dir / "test-classes.toml").write_text("format = 'something/9'\n", encoding="utf-8")
        self.assertEqual(run_main(["generate"])[0], 2)

    def test_nextest_failing_exits_2(self):
        failed = subprocess.CompletedProcess(args=[], returncode=101, stdout="", stderr="error: could not compile")
        with mock.patch.object(tool.subprocess, "run", return_value=failed):
            code, _, err = run_main(["generate"])
        self.assertEqual(code, 2)
        self.assertIn("could not compile", err)

    def test_audit_exit_codes(self):
        self.generate()
        listed = tool.read_tsv(self.ws.list)
        # The JUnit fixtures' tests are not in the scratch list, so every one is outside the class.
        code, out, _ = run_main(["audit", str(FIXTURES / "junit-pass.xml"), "--class", "capsule"])
        self.assertEqual(code, 1)
        self.assertIn("outside-class", out)
        self.assertTrue(listed)


class FilterAndList(unittest.TestCase):
    def setUp(self):
        self.ws = Workspace(self)
        with self.ws.listings(results()):
            self.assertEqual(run_main(["generate"])[0], 0)

    def test_filter_prints_the_profiles_default_filter(self):
        code, out, _ = run_main(["filter", "isolation"])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), tool.class_filters()["isolation"].strip())
        self.assertIn("murmur-cli::guest_workers", out)

    def test_list_filters_by_class_and_kind(self):
        code, out, _ = run_main(["list", "--class", "capsule", "--kind", "e2e"])
        self.assertEqual(code, 0)
        rows = [line.split("\t") for line in out.splitlines()]
        self.assertTrue(rows)
        self.assertTrue(all(r[0] == "mur-roost::version_output" and r[2:4] == ["capsule", "e2e"] for r in rows))

    def test_list_json_has_the_format_one_entry_per_row_and_counts(self):
        code, out, _ = run_main(["list", "--json"])
        self.assertEqual(code, 0)
        data = json.loads(out)
        self.assertEqual(data["format"], "murmur.test-classes/1")
        self.assertEqual(len(data["tests"]), len(tool.read_tsv(self.ws.list)))
        self.assertEqual(set(data["tests"][0]), set(tool.COLUMNS))
        self.assertEqual(data["counts"]["total"], len(data["tests"]))
        self.assertEqual(sum(data["counts"]["class"].values()), len(data["tests"]))
        self.assertTrue(all(t["class"] in tool.CLASSES and t["kind"] in tool.KINDS for t in data["tests"]))

    def test_list_live_recomputes_and_fails_on_a_finding(self):
        with self.ws.listings(results()):
            self.assertEqual(run_main(["list", "--live", "--class", "isolation"])[0], 0)
        with self.ws.listings(results(capsule=set())):
            self.assertEqual(run_main(["list", "--live"])[0], 1)


if __name__ == "__main__":
    unittest.main()
