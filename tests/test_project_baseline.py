import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from runtime import project_check


class ProjectBaselineTests(unittest.TestCase):
    def test_missing_coverage_errors_and_invalid_reports_never_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = (
                (
                    {
                        "paths": {"scanned": ["lib/module.py"]},
                        "results": [],
                        "errors": [],
                    },
                    "passed",
                ),
                (
                    {"paths": {"scanned": []}, "results": [], "errors": []},
                    "not applicable",
                ),
                (
                    {
                        "paths": {"scanned": ["lib/module.py"]},
                        "results": [],
                        "errors": [{}],
                    },
                    "failed",
                ),
                (
                    {
                        "paths": {"scanned": ["lib/module.py"]},
                        "results": [{}],
                        "errors": [],
                    },
                    "failed",
                ),
                ({}, "blocked"),
            )
            for report, status in cases:

                def scan(*_arguments):
                    (root / "semgrep.json").write_text(json.dumps(report))
                    return {"name": "baseline", "status": "passed", "diagnostics": ""}

                with (
                    self.subTest(report=report),
                    patch.object(project_check, "QUALITY_RULES", str(root)),
                    patch.object(project_check, "run_check", side_effect=scan),
                ):
                    self.assertEqual(
                        project_check.baseline(root, root)["status"], status
                    )

    def test_only_known_sql_advisories_are_nonblocking(self):
        advisory = {
            "check_id": "python-review-formatted-sql",
            "extra": {"severity": "WARNING"},
        }
        cases = (
            ([advisory], [], "passed", "passed"),
            ([advisory], [], "failed", "failed"),
            ([advisory], [{}], "passed", "failed"),
            ([advisory, {}], [], "passed", "failed"),
            (
                [{"check_id": "unknown", "extra": {"severity": "WARNING"}}],
                [],
                "passed",
                "failed",
            ),
            ([{**advisory, "extra": {"severity": "ERROR"}}], [], "passed", "failed"),
            ([None], [], "passed", "failed"),
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for findings, errors, runner_status, expected in cases:

                def scan(_root, check, _scratch):
                    argv = check["argv"]
                    self.assertIn("--no-rewrite-rule-ids", argv)
                    exclusions = [
                        argv[index + 1]
                        for index, item in enumerate(argv)
                        if item == "--exclude"
                    ]
                    self.assertEqual(
                        exclusions, ["/tests/semgrep/", "/tests/portable-quality/"]
                    )
                    self.assertEqual(argv[-1], ".")
                    (root / "semgrep.json").write_text(
                        json.dumps(
                            {
                                "paths": {"scanned": ["tests/test_real.py"]},
                                "results": findings,
                                "errors": errors,
                            }
                        )
                    )
                    return {
                        "name": "baseline",
                        "status": runner_status,
                        "diagnostics": "",
                    }

                with (
                    self.subTest(
                        findings=findings, errors=errors, runner=runner_status
                    ),
                    patch.object(project_check, "QUALITY_RULES", str(root)),
                    patch.object(project_check, "run_check", side_effect=scan),
                ):
                    result = project_check.baseline(root, root)
                    self.assertEqual(result["status"], expected)
                    self.assertEqual(result["findings"], findings)

    def test_baseline_does_not_load_project_manifest(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(Path, "cwd", return_value=Path(directory)),
            patch.object(
                project_check, "load", side_effect=AssertionError("manifest read")
            ),
            patch.object(
                project_check, "run", return_value={"status": "passed"}
            ) as run,
        ):
            self.assertEqual(project_check.main(["baseline"]), 0)
            self.assertEqual(run.call_args.args[1], {"checks": []})


if __name__ == "__main__":
    unittest.main()
