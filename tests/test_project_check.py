import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from runtime import project_check as checks


class ProjectCheckTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
