import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from runtime import project_check as checks


class ProjectCheckTests(unittest.TestCase):
    def test_manifest_requires_integer_version(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for version in (True, 1.0, "1"):
                with self.subTest(version=version):
                    (root / checks.MANIFEST).write_text(
                        json.dumps(
                            {"version": version, "checks": [self.check("ok", "")]}
                        )
                    )
                    with self.assertRaisesRegex(checks.CheckError, "version 1"):
                        checks.load(root)

    def test_omitted_cwd_defaults_to_project_root(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            check = self.check(
                "judgment-replay",
                "from pathlib import Path; assert Path('evidence.txt').read_text() == 'fixture'",
            )
            del check["cwd"]
            (root / "evidence.txt").write_text("fixture")
            (root / checks.MANIFEST).write_text(
                json.dumps({"version": 1, "checks": [check]})
            )
            declared = checks.load(root)["checks"][0]
            self.assertEqual(declared["cwd"], ".")
            self.assertEqual(checks.run_check(root, declared, root)["status"], "passed")

    def test_offline_evaluation_exit_status_overrules_model_claims(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "answers.json"
            fixture.write_text(json.dumps({"choice": "c0", "expected": "c0"}))
            evaluation = root / "evaluate.py"
            evaluation.write_text(
                "import json\n"
                "from pathlib import Path\n"
                "case = json.loads(Path('answers.json').read_text())\n"
                "print(json.dumps({'model_claim': 'passed', 'confidence': 1.0}))\n"
                "raise SystemExit(0 if case['choice'] == case['expected'] else 1)\n"
            )
            check = self.check("judgment-replay", "")
            check["argv"] = [sys.executable, "evaluate.py"]
            (root / checks.MANIFEST).write_text(
                json.dumps({"version": 1, "checks": [check]})
            )
            declared = checks.load(root)["checks"][0]
            self.assertEqual(checks.run_check(root, declared, root)["status"], "passed")
            fixture.write_text(json.dumps({"choice": "c0", "expected": "none"}))
            result = checks.run_check(root, declared, root)
            self.assertEqual(result["status"], "failed")
            self.assertEqual(result["returncode"], 1)
            self.assertIn('"confidence": 1.0', result["diagnostics"])

    def check(self, name, code, **overrides):
        return {
            "name": name,
            "argv": [sys.executable, "-c", code],
            "cwd": ".",
            "requires": [sys.executable],
            "timeout_seconds": 2,
            "profiles": ["fast", "full"],
            **overrides,
        }

    def test_failures_warnings_missing_tools_and_timeout_do_not_stop_others(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            document = {
                "version": 1,
                "checks": [
                    self.check("failure", "print('native error'); exit(3)"),
                    self.check("warning", "print('warning: synthetic finding')"),
                    self.check("missing", "", requires=["synthetic-missing-checker"]),
                    self.check(
                        "timeout", "import time; time.sleep(30)", timeout_seconds=0.05
                    ),
                    self.check("last", "print('done')"),
                    self.check("full-only", "exit(1)", profiles=["full"]),
                ],
            }
            (root / checks.MANIFEST).write_text(json.dumps(document))
            output = io.StringIO()
            with (
                contextlib.redirect_stdout(output),
                patch.object(
                    checks,
                    "baseline",
                    return_value={
                        "name": "baseline",
                        "status": "passed",
                        "diagnostics": "",
                    },
                ),
            ):
                report = checks.run(root, checks.load(root), "fast", json_output=True)
            self.assertEqual(json.loads(output.getvalue()), report)
            self.assertEqual(
                [item["status"] for item in report["checks"][1:]],
                ["failed", "failed", "blocked", "blocked", "passed", "not applicable"],
            )
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["checks"][1]["returncode"], 3)
            self.assertIn("timeout", report["checks"][4]["diagnostics"])

    def test_contract_rejects_escapes_invalid_timeout_and_duplicate_names(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for overrides in (
                {"cwd": ".."},
                {"cwd": "/tmp"},
                {"timeout_seconds": True},
                {"timeout_seconds": float("nan")},
                {"profiles": ["guessed"]},
            ):
                with self.subTest(overrides=overrides):
                    (root / checks.MANIFEST).write_text(
                        json.dumps(
                            {
                                "version": 1,
                                "checks": [self.check("one", "", **overrides)],
                            }
                        )
                    )
                    with self.assertRaises(checks.CheckError):
                        checks.load(root)
            (root / "escape").symlink_to(root.parent, target_is_directory=True)
            (root / checks.MANIFEST).write_text(
                json.dumps(
                    {
                        "version": 1,
                        "checks": [self.check("one", "", cwd="escape")],
                    }
                )
            )
            with self.assertRaises(checks.CheckError):
                checks.load(root)

    def test_missing_tools_explain_how_to_restore_the_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = checks.run_check(
                root,
                self.check("missing", "", requires=["synthetic-missing-checker"]),
                root,
            )
            self.assertEqual(result["status"], "blocked")
            self.assertIn("synthetic-missing-checker", result["diagnostics"])
            self.assertIn("nix develop path:.", result["diagnostics"])
            self.assertIn("devShells", result["diagnostics"])

    def test_plan_checks_prerequisites_without_running_project_code(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rules = root / "rules"
            rules.mkdir()
            (root / "work").mkdir()
            script = root / "work" / "check"
            script.write_text("#!/bin/sh\ntouch should-not-exist\n")
            script.chmod(0o755)
            document = {
                "checks": [
                    self.check(
                        "ready",
                        "secret-argument",
                        argv=["./check", "secret-argument"],
                        cwd="work",
                    ),
                    self.check(
                        "missing",
                        "",
                        requires=["synthetic-missing-checker"],
                    ),
                    self.check("bad-cwd", "", cwd="gone"),
                    self.check("full-only", "", profiles=["full"]),
                ]
            }
            output = io.StringIO()
            with (
                patch.object(checks, "QUALITY_RULES", str(rules)),
                patch.object(checks, "SEMGREP", sys.executable),
                patch.object(
                    checks, "run_check", side_effect=AssertionError("ran check")
                ),
                patch.object(
                    checks, "baseline", side_effect=AssertionError("ran baseline")
                ),
                contextlib.redirect_stdout(output),
            ):
                report = checks.plan(root, document, "fast", json_output=True)
            self.assertEqual(json.loads(output.getvalue()), report)
            self.assertEqual(report["status"], "blocked")
            self.assertEqual(
                [(item["name"], item["status"]) for item in report["checks"]],
                [
                    ("baseline", "ready"),
                    ("ready", "ready"),
                    ("missing", "blocked"),
                    ("bad-cwd", "blocked"),
                ],
            )
            self.assertTrue(report["checks"][3]["cwd_unavailable"])
            self.assertFalse((root / "work" / "should-not-exist").exists())
            self.assertNotIn("secret-argument", output.getvalue())

    def test_plan_cli_uses_read_only_path(self):
        with (
            patch.object(checks, "load", return_value={"checks": []}),
            patch.object(checks, "plan", return_value={"status": "ready"}) as plan,
            patch.object(checks, "run", side_effect=AssertionError("ran checks")),
        ):
            self.assertEqual(checks.main(["fast", "--plan", "--json"]), 0)
        self.assertEqual(plan.call_args.args[2], "fast")
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as error,
        ):
            checks.main(["watch", "--plan"])
        self.assertEqual(error.exception.code, 2)

    def test_plan_and_run_resolve_relative_requirements_from_check_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "work"
            work.mkdir()
            program = work / "local-tool"
            program.write_text("#!/bin/sh\nexit 0\n")
            program.chmod(0o755)
            rules = root / "rules"
            rules.mkdir()
            check = self.check(
                "local-tool",
                "",
                argv=["./local-tool"],
                cwd="work",
                requires=["./local-tool"],
            )
            with (
                patch.object(checks, "QUALITY_RULES", str(rules)),
                patch.object(checks, "SEMGREP", sys.executable),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                report = checks.plan(root, {"checks": [check]}, "fast")
            self.assertEqual(report["checks"][1]["status"], "ready")
            self.assertEqual(checks.run_check(root, check, root)["status"], "passed")

    def test_changed_plan_annotates_without_skipping_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rules = root / "rules"
            rules.mkdir()
            document = {
                "checks": [
                    self.check("code", "", input_paths=["src/"]),
                    self.check("docs", "", input_paths=["docs/readme.md"]),
                    self.check("other", "", input_paths=["other/"]),
                    self.check("unmapped", ""),
                ]
            }
            output = io.StringIO()
            with (
                patch.object(checks, "QUALITY_RULES", str(rules)),
                patch.object(checks, "SEMGREP", sys.executable),
                patch.object(
                    checks,
                    "changed_paths",
                    return_value=["docs/readme.md", "src/main.py"],
                ),
                contextlib.redirect_stdout(output),
            ):
                report = checks.plan(
                    root, document, "fast", json_output=True, changed=True
                )
            self.assertEqual(json.loads(output.getvalue()), report)
            self.assertEqual(
                [item["relevance"] for item in report["checks"]],
                ["required", "affected", "affected", "unaffected", "unknown"],
            )
            self.assertEqual(len(report["checks"]), 5)
            self.assertTrue(report["change_scope"]["advisory_only"])
            with (
                patch.object(checks, "QUALITY_RULES", str(rules)),
                patch.object(checks, "SEMGREP", sys.executable),
                patch.object(checks, "changed_paths", return_value=[checks.MANIFEST]),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                changed_manifest = checks.plan(root, document, "fast", changed=True)
            self.assertTrue(
                all(
                    item["relevance"] == "affected"
                    for item in changed_manifest["checks"][1:]
                )
            )

    def test_changed_paths_include_both_rename_sides_and_untracked(self):
        status = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=b"R  new.py\0old.py\0?? added.py\0",
            stderr=b"",
        )
        with patch.object(checks.subprocess, "run", return_value=status) as process:
            paths = checks.changed_paths(Path("."))
        self.assertEqual(paths, ["added.py", "new.py", "old.py"])
        self.assertIn("--no-optional-locks", process.call_args.args[0])
        with (
            patch.object(
                checks.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    [], 1, b"", b"not a repository"
                ),
            ),
            self.assertRaisesRegex(checks.CheckError, "Git change list"),
        ):
            checks.changed_paths(Path("."))

    def test_input_paths_reject_ambiguous_or_escaping_patterns(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in ("../outside", "/absolute", "src/*", "src//file", "."):
                with self.subTest(path=path):
                    (root / checks.MANIFEST).write_text(
                        json.dumps(
                            {
                                "version": 1,
                                "checks": [self.check("code", "", input_paths=[path])],
                            }
                        )
                    )
                    with self.assertRaisesRegex(checks.CheckError, "input_paths"):
                        checks.load(root)
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as error,
        ):
            checks.main(["fast", "--changed"])
        self.assertEqual(error.exception.code, 2)

    def test_unavailable_program_reports_blocker_without_exposing_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            program = root / "not-executable"
            program.write_text("not executable")
            for executable in (str(program), "synthetic-missing-checker"):
                with self.subTest(executable=executable):
                    result = checks.run_check(
                        root,
                        self.check(
                            "start", "", argv=[executable, "sensitive-argument"]
                        ),
                        root,
                    )
                    self.assertEqual(result["status"], "blocked")
                    self.assertIn(executable, result["diagnostics"])
                    self.assertIn("nix develop path:.", result["diagnostics"])
                    self.assertNotIn("sensitive-argument", result["diagnostics"])

    def test_shell_exit_codes_and_silent_failures_have_actionable_diagnostics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for code, status in ((126, "blocked"), (127, "blocked"), (23, "failed")):
                with self.subTest(code=code):
                    result = checks.run_check(
                        root, self.check("shell", f"exit({code})"), root
                    )
                    self.assertEqual(result["status"], status)
                    self.assertEqual(result["returncode"], code)
                    self.assertIn(str(code), result["diagnostics"])
                    if status == "blocked":
                        self.assertIn("nix develop path:.", result["diagnostics"])

    def test_environment_only_blockers_remain_blocked_in_text_and_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            document = {
                "checks": [
                    self.check("missing", "", requires=["synthetic-missing-checker"])
                ]
            }
            for json_output in (False, True):
                output = io.StringIO()
                with (
                    contextlib.redirect_stdout(output),
                    patch.object(
                        checks,
                        "baseline",
                        return_value={
                            "name": "baseline",
                            "status": "passed",
                            "diagnostics": "",
                        },
                    ),
                ):
                    report = checks.run(root, document, "fast", json_output=json_output)
                self.assertEqual(report["status"], "blocked")
                if json_output:
                    self.assertEqual(json.loads(output.getvalue()), report)
                else:
                    self.assertIn("project-check fast: blocked", output.getvalue())

    def test_nix_dirty_tree_notice_does_not_hide_real_warnings_or_failures(self):
        notice = "warning: Git tree '/workspace/project' is dirty"
        cases = [
            (notice, 0, "passed"),
            (notice + "\nwarning: unused variable", 0, "failed"),
            (notice + "\n1 warning generated.", 0, "failed"),
            (notice, 1, "failed"),
            (notice + ": warning: unexpected suffix", 0, "failed"),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for output, code, status in cases:
                with self.subTest(output=output, code=code):
                    check = self.check("format", f"print({output!r}); exit({code})")
                    result = checks.run_check(root, check, root)
                    self.assertEqual(result["status"], status)
                    self.assertIn(output, result["diagnostics"])

    def test_expected_warnings_match_exact_lines_with_bounded_counts(self):
        message = "evaluation warning: optional synthetic transport [v1].* is disabled"
        expected = {
            "message": message,
            "reason": "The isolated fixture deliberately disables this transport.",
            "max_count": 2,
        }
        cases = [
            (message, 0, "passed", 1),
            (message + "\n" + message, 0, "passed", 2),
            ("clean output", 0, "passed", 0),
            ("\n".join([message] * 3), 0, "failed", 3),
            (message + "\nwarning: unused variable", 0, "failed", 1),
            (message + "\n1 warning generated.", 0, "failed", 1),
            (message + ": unexpected suffix", 0, "failed", 0),
            ("prefix " + message, 0, "failed", 0),
            ("\x1b[33m" + message + "\x1b[0m", 0, "failed", 0),
            (message, 7, "failed", 1),
            (message + "\nrequired command not found", 1, "blocked", 1),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for output, code, status, count in cases:
                with self.subTest(output=output, code=code):
                    check = self.check(
                        "transport",
                        f"print({output!r}); exit({code})",
                        expected_warnings=[expected],
                    )
                    result = checks.run_check(root, check, root)
                    self.assertEqual(result["status"], status)
                    self.assertEqual(result["returncode"], code)
                    self.assertIn(output, result["diagnostics"])
                    self.assertIn(expected["reason"], result["diagnostics"])
                    self.assertEqual(
                        result["expected_warnings"], [{**expected, "count": count}]
                    )
                    if count > expected["max_count"]:
                        self.assertIn(
                            "occurrence limit exceeded", result["diagnostics"]
                        )
            undeclared = self.check("transport", f"print({message!r})")
            self.assertEqual(
                checks.run_check(root, undeclared, root)["status"], "failed"
            )
            timed_out = self.check(
                "transport",
                f"print({message!r}, flush=True); import time; time.sleep(30)",
                expected_warnings=[expected],
                timeout_seconds=0.5,
            )
            result = checks.run_check(root, timed_out, root)
            self.assertEqual(result["status"], "blocked")
            self.assertIsNone(result["returncode"])
            self.assertEqual(result["expected_warnings"], [{**expected, "count": 1}])

    def test_expected_warnings_contract_requires_exact_text_reason_and_limit(self):
        expected = {
            "message": "warning: optional synthetic transport is disabled",
            "reason": "The fixture disables it deliberately.",
            "max_count": 2,
        }
        invalid = [
            None,
            "warning: guessed",
            [None],
            [expected, expected],
            [
                {**expected, "message": f"warning: synthetic {index}"}
                for index in range(33)
            ],
            [{**expected, "unknown": True}],
            [{key: value for key, value in expected.items() if key != "reason"}],
            [{key: value for key, value in expected.items() if key != "max_count"}],
            [{**expected, "reason": " "}],
            [{**expected, "reason": "first\nsecond"}],
            [{**expected, "reason": "x" * 4097}],
            [{**expected, "message": "not a diagnostic"}],
            [{**expected, "message": "warning: first\nwarning: second"}],
            [{**expected, "message": "warning: first\rsecond"}],
            [{**expected, "message": "warning: first\0second"}],
            [{**expected, "message": "warning: " + "x" * 4096}],
            *[[{**expected, "max_count": count}] for count in (True, 0, -1, 101, 1.5)],
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for declarations in ([expected], []):
                check = self.check("transport", "", expected_warnings=declarations)
                (root / checks.MANIFEST).write_text(
                    json.dumps({"version": 1, "checks": [check]})
                )
                self.assertEqual(
                    checks.load(root)["checks"][0]["expected_warnings"], declarations
                )
            for declarations in invalid:
                with self.subTest(declarations=declarations):
                    check = self.check("transport", "", expected_warnings=declarations)
                    (root / checks.MANIFEST).write_text(
                        json.dumps({"version": 1, "checks": [check]})
                    )
                    with self.assertRaisesRegex(checks.CheckError, "expected_warnings"):
                        checks.load(root)

    def test_expected_warning_reports_remain_visible_in_text_json_and_plan(self):
        message = "warning: optional synthetic transport is disabled"
        expected = {
            "message": message,
            "reason": "The fixture disables it deliberately.",
            "max_count": 1,
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            check = self.check(
                "transport", f"print({message!r})", expected_warnings=[expected]
            )
            (root / checks.MANIFEST).write_text(
                json.dumps({"version": 1, "checks": [check]})
            )
            document = checks.load(root)
            for json_output in (False, True):
                output = io.StringIO()
                with (
                    contextlib.redirect_stdout(output),
                    patch.object(
                        checks,
                        "baseline",
                        return_value={
                            "name": "baseline",
                            "status": "passed",
                            "diagnostics": "",
                        },
                    ),
                ):
                    report = checks.run(root, document, "fast", json_output=json_output)
                self.assertEqual(report["status"], "passed")
                self.assertEqual(
                    report["checks"][1]["expected_warnings"], [{**expected, "count": 1}]
                )
                self.assertIn(expected["reason"], output.getvalue())
                if json_output:
                    self.assertEqual(json.loads(output.getvalue()), report)
                output = io.StringIO()
                with (
                    contextlib.redirect_stdout(output),
                    patch.object(
                        checks, "baseline_tooling", return_value=(root, sys.executable)
                    ),
                    patch.object(
                        checks,
                        "run_check",
                        side_effect=AssertionError("plan executed a check"),
                    ),
                ):
                    report = checks.plan(
                        root, document, "fast", json_output=json_output
                    )
                self.assertEqual(report["status"], "ready")
                self.assertEqual(report["checks"][1]["expected_warnings"], [expected])
                self.assertIn(expected["reason"], output.getvalue())
                if json_output:
                    self.assertEqual(json.loads(output.getvalue()), report)

    def test_argv_and_relative_cwd_are_preserved(self):
        with tempfile.TemporaryDirectory(prefix="project with spaces ") as directory:
            root = Path(directory)
            (root / "sub dir").mkdir()
            check = self.check("args", "import sys; print(sys.argv[1])", cwd="sub dir")
            check["argv"].append("literal ; $(false)")
            result = checks.run_check(root, check, root)
            self.assertEqual(result["status"], "passed")
            self.assertIn("literal ; $(false)", result["diagnostics"])

    def test_build_checks_receive_pinned_ca_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            certificate = root / "ca-bundle.crt"
            certificate.write_text("synthetic certificate bundle")
            check = self.check(
                "certificate",
                "import os; "
                "assert os.environ['NIX_SSL_CERT_FILE'].endswith('ca-bundle.crt'); "
                "assert os.environ['SSL_CERT_FILE'].endswith('ca-bundle.crt')",
            )
            with patch.object(checks, "CA_CERT_FILE", str(certificate)):
                result = checks.run_check(root, check, root)
            self.assertEqual(result["status"], "passed")

    def test_watch_batches_edits_during_run_and_ignores_generated_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "source").write_text("a")
            (root / "target").mkdir()
            (root / "target" / "generated").write_text("a")
            initial = checks.snapshot(root, [])
            (root / "target" / "generated").write_text("generated")
            self.assertEqual(initial, checks.snapshot(root, []))
            (root / "replacement").write_text("b")
            os.replace(root / "replacement", root / "source")
            self.assertNotEqual(initial, checks.snapshot(root, []))
            times = iter([0.0, 0.2, 0.8])
            snapshots = iter(
                [{"source": 1}, {"source": 2}, {"source": 2}, {"source": 2}]
            )
            with (
                patch.object(checks, "load", return_value={"checks": []}),
                patch.object(
                    checks, "snapshot", side_effect=lambda *_: next(snapshots)
                ),
                patch.object(checks.time, "monotonic", side_effect=lambda: next(times)),
                patch.object(
                    checks.time,
                    "sleep",
                    side_effect=[None, None, None, KeyboardInterrupt],
                ),
                patch.object(checks, "run") as run,
                self.assertRaises(KeyboardInterrupt),
            ):
                checks.watch(root, json_output=True)
            self.assertEqual(run.call_count, 2)

    def test_watch_ignore_cannot_hide_manifest_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / checks.MANIFEST
            manifest.write_text("initial")
            (root / "generated").write_text("initial")
            before = checks.snapshot(root, ["*"])
            (root / "generated").write_text("changed")
            self.assertEqual(before, checks.snapshot(root, ["*"]))
            manifest.write_text("fixed manifest")
            self.assertNotEqual(before, checks.snapshot(root, ["*"]))

    def test_watch_recovers_after_temporary_invalid_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            document = {"checks": [], "watch_ignore": []}
            snapshots = iter(
                [
                    {"manifest": 0},
                    {"manifest": 1},
                    {"manifest": 1},
                    {"manifest": 2},
                    {"manifest": 2},
                ]
            )
            times = iter([0.0, 0.0, 0.6, 1.0, 1.0, 1.6])
            output = io.StringIO()
            with (
                contextlib.redirect_stdout(output),
                patch.object(
                    checks,
                    "load",
                    side_effect=[
                        document,
                        checks.CheckError("incomplete manifest"),
                        document,
                    ],
                ),
                patch.object(
                    checks, "snapshot", side_effect=lambda *_: next(snapshots)
                ),
                patch.object(checks.time, "monotonic", side_effect=lambda: next(times)),
                patch.object(
                    checks.time,
                    "sleep",
                    side_effect=[None, None, None, None, KeyboardInterrupt],
                ),
                patch.object(checks, "run") as run,
                self.assertRaises(KeyboardInterrupt),
            ):
                checks.watch(root, json_output=True)
            self.assertEqual(run.call_count, 2)
            self.assertIn("incomplete manifest", output.getvalue())

    def test_watch_can_start_with_invalid_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            document = {"checks": [], "watch_ignore": []}
            snapshots = iter([{"manifest": 0}, {"manifest": 1}, {"manifest": 1}])
            times = iter([0.0, 0.0, 0.6])
            output = io.StringIO()
            with (
                contextlib.redirect_stdout(output),
                patch.object(
                    checks,
                    "load",
                    side_effect=[checks.CheckError("incomplete manifest"), document],
                ),
                patch.object(
                    checks, "snapshot", side_effect=lambda *_: next(snapshots)
                ),
                patch.object(checks.time, "monotonic", side_effect=lambda: next(times)),
                patch.object(
                    checks.time,
                    "sleep",
                    side_effect=[None, None, KeyboardInterrupt],
                ),
                patch.object(checks, "run") as run,
                self.assertRaises(KeyboardInterrupt),
            ):
                checks.watch(root, json_output=True)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(json.loads(output.getvalue())["status"], "blocked")


if __name__ == "__main__":
    unittest.main()
