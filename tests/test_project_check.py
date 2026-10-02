import contextlib
import io
import json
import os
import shlex
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

    def test_excessive_output_fails_without_accepting_partial_diagnostics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for prefix in (
                "",
                "warning: late finding\n",
                "required command not found\n",
            ):
                with self.subTest(prefix=prefix):
                    check = self.check(
                        "noisy",
                        f"import os; os.write(2, {prefix.encode()!r}); "
                        "os.write(1, b'x' * 65536)",
                    )
                    with patch.object(checks, "MAX_OUTPUT_BYTES", 8192):
                        result = checks.run_check(root, check, root)
                    self.assertEqual(result["status"], "failed")
                    self.assertTrue(result["output_truncated"])
                    self.assertTrue("output limit" in result["diagnostics"])
                    self.assertLess(len(result["diagnostics"]), 9000)
            self.assertEqual(
                checks.run_check(root, self.check("next", "print('done')"), root)[
                    "status"
                ],
                "passed",
            )

    def test_output_limit_preserves_complete_output_at_the_boundary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            check = self.check("boundary", "import os; os.write(1, b'x' * 4096)")
            with patch.object(checks, "MAX_OUTPUT_BYTES", 4096):
                result = checks.run_check(root, check, root)
            self.assertEqual(result["status"], "passed")
            self.assertEqual(result["diagnostics"], "x" * 4096)
            self.assertNotIn("output_truncated", result)

    def test_closed_output_does_not_bypass_the_process_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            check = self.check(
                "closed-output",
                "import os, time; os.close(1); os.close(2); time.sleep(30)",
                timeout_seconds=0.1,
            )
            result = checks.run_check(root, check, root)
            self.assertEqual(result["status"], "blocked")
            self.assertIn("timeout", result["diagnostics"])
            self.assertLess(result["duration_seconds"], 2)

    def test_completed_output_cleans_the_process_group_only_once(self):
        with (
            tempfile.TemporaryFile() as output,
            subprocess.Popen(
                [sys.executable, "-c", "print('done')"],
                stdout=subprocess.PIPE,
                start_new_session=True,
            ) as process,
            patch.object(checks, "stop", wraps=checks.stop) as cleanup,
        ):
            process.wait(timeout=2)
            self.assertIsNone(checks.collect_output(process, output, 2))
            cleanup.assert_called_once_with(process)
            output.seek(0)
            self.assertEqual(output.read(), b"done\n")

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
        with patch.object(
            checks,
            "git_output",
            side_effect=[b"", b"\n", b"R  new.py\0old.py\0?? added.py\0"],
        ) as process:
            paths = checks.changed_paths(Path("."))
        self.assertEqual(paths, ["added.py", "new.py", "old.py"])
        self.assertIn("--no-optional-locks", process.call_args.args[1])
        with (
            patch.object(
                checks,
                "git_output",
                side_effect=checks.CheckError("Git change list unavailable"),
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


class ChangedPathsTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GIT_")
        }
        self.environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
        self.git("init", "-q", "--initial-branch=main")
        self.source = self.root / "tracked.py"
        self.source.write_text("before\n")
        self.git("add", ".")
        self.commit("fixture")
        self.source.write_text("after\n")

    def git(self, *arguments):
        return subprocess.run(
            [
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                *arguments,
            ],
            cwd=self.root,
            env=self.environment,
            capture_output=True,
            check=True,
        )

    def commit(self, message, root=None):
        self.git(
            "-C",
            str(root or self.root),
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            message,
        )

    def test_ignores_inherited_repository_and_configuration_overrides(self):
        foreign = self.root / "foreign"
        foreign.mkdir()
        self.git("-C", str(foreign), "init", "-q", "--initial-branch=main")
        (foreign / "foreign.py").write_text("foreign\n")
        config = self.root / "invalid-config"
        config.write_text("not a git configuration\n")
        with patch.dict(
            os.environ,
            {
                "GIT_DIR": str(foreign / ".git"),
                "GIT_WORK_TREE": str(foreign),
                "GIT_INDEX_FILE": str(foreign / ".git/index"),
                "GIT_CONFIG_GLOBAL": str(config),
                "GIT_CONFIG_COUNT": "1",
                "GIT_CONFIG_KEY_0": "core.fsmonitor",
                "GIT_CONFIG_VALUE_0": "unavailable-program",
            },
        ):
            paths = checks.changed_paths(self.root)
        self.assertIn("tracked.py", paths)
        self.assertNotIn("foreign.py", paths)

    def test_paths_are_relative_to_selected_project_with_rename_sides(self):
        project = self.root / "nested project\n"
        project.mkdir()
        source = project / "old.py"
        source.write_text("before\n")
        self.git("add", str(source))
        self.commit("nested")
        source.rename(project / "new.py")
        self.git("add", str(project))
        (project / "added file.py").write_text("untracked\n")
        self.assertEqual(
            checks.changed_paths(project), ["added file.py", "new.py", "old.py"]
        )
        self.assertEqual(
            checks.relevance(
                {"input_paths": ["new.py"]}, checks.changed_paths(project)
            )["relevance"],
            "affected",
        )

    def test_does_not_execute_repository_fsmonitor(self):
        marker = self.root / "hook-executed"
        hook = self.root / "fsmonitor.py"
        hook.write_text(
            f"#!{sys.executable}\nfrom pathlib import Path\n"
            f"Path({str(marker)!r}).write_text('executed')\nprint('token\\0', end='')\n"
        )
        hook.chmod(0o700)
        self.git("config", "core.fsmonitor", shlex.quote(str(hook)))
        self.assertIn("tracked.py", checks.changed_paths(self.root))
        self.assertFalse(marker.exists())

    def test_does_not_execute_required_clean_or_process_filters(self):
        self.source.write_text("after!\n")
        os.utime(self.source, (1000, 1000))
        for kind in ("clean", "process"):
            with self.subTest(kind=kind):
                marker = self.root / f"{kind}-executed"
                program = self.root / f"{kind}.py"
                program.write_text(
                    "from pathlib import Path\n"
                    f"Path({str(marker)!r}).write_text('executed')\nprint('filtered')\n"
                )
                (self.root / ".gitattributes").write_text(
                    "tracked.py filter=fixture.driver\n"
                )
                self.git(
                    "config",
                    f"filter.fixture.driver.{kind}",
                    shlex.join([sys.executable, str(program)]),
                )
                self.git("config", "filter.fixture.driver.required", "true")
                self.assertIn("tracked.py", checks.changed_paths(self.root))
                self.assertFalse(marker.exists())
                self.git("config", "--remove-section", "filter.fixture.driver")

    def test_submodule_content_is_owned_by_its_project_plan(self):
        child = self.root / "child"
        child.mkdir()
        self.git("-C", str(child), "init", "-q", "--initial-branch=main")
        source = child / "tracked.py"
        source.write_text("before\n")
        self.git("-C", str(child), "add", ".")
        self.commit("child", child)
        self.git("add", "child")
        self.commit("submodule")
        marker = child / "filter-executed"
        program = child / "filter.py"
        program.write_text(
            "from pathlib import Path\n"
            f"Path({str(marker)!r}).write_text('executed')\nprint('filtered')\n"
        )
        (child / ".gitattributes").write_text("tracked.py filter=fixture\n")
        self.git(
            "-C",
            str(child),
            "config",
            "filter.fixture.clean",
            shlex.join([sys.executable, str(program)]),
        )
        source.write_text("after!\n")
        os.utime(source, (1000, 1000))
        self.assertNotIn("child", checks.changed_paths(self.root))
        self.assertFalse(marker.exists())
        self.assertIn("tracked.py", checks.changed_paths(child))
        self.assertFalse(marker.exists())
        self.git("-C", str(child), "config", "--remove-section", "filter.fixture")
        self.git("-C", str(child), "add", "tracked.py")
        self.commit("update", child)
        self.assertIn("child", checks.changed_paths(self.root))

    def test_unrepresentable_and_excessive_filters_block_before_status(self):
        for names in (
            ("bad=driver",),
            tuple(f"driver{index}" for index in range(65)),
            ("x" * 32768,),
        ):
            with self.subTest(count=len(names)):
                for name in names:
                    self.git("config", f"filter.{name}.required", "true")
                with self.assertRaisesRegex(checks.CheckError, "filter configuration"):
                    checks.changed_paths(self.root)
                for name in names:
                    self.git("config", "--remove-section", f"filter.{name}")

    def test_git_capture_bounds_output_and_reports_unavailable_commands(self):
        with patch.object(checks, "MAX_OUTPUT_BYTES", 256):
            exact = checks.git_output(
                self.root,
                [sys.executable, "-c", "import os; os.write(1, b'x'*256)"],
                self.environment,
            )
            self.assertEqual(exact, b"x" * 256)
            with self.assertRaisesRegex(checks.CheckError, "too large"):
                checks.git_output(
                    self.root,
                    [sys.executable, "-c", "import os; os.write(1, b'x'*257)"],
                    self.environment,
                )
        with self.assertRaisesRegex(checks.CheckError, "cannot inspect Git"):
            checks.git_output(
                self.root, [str(self.root / "unavailable")], self.environment
            )


if __name__ == "__main__":
    unittest.main()
