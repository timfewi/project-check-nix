import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from runtime import project_check, project_docs


class DocumentationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "package.json").write_text('{"dependencies":{"example":"2.4.0"}}')
        (self.root / "README.md.in").write_text(
            "Version: {{package.dependencies.example}}\n"
        )
        self.manifest = {
            "version": 1,
            "sources": {"package": {"kind": "json", "path": "package.json"}},
            "templates": [{"source": "README.md.in", "target": "README.md"}],
        }
        self.write_manifest()

    def write_manifest(self):
        (self.root / project_docs.MANIFEST).write_text(json.dumps(self.manifest))

    def test_version_change_updates_docs_and_read_only_check_detects_drift(self):
        self.assertEqual(project_docs.synchronize(self.root), ["README.md"])
        self.assertFalse((self.root / "README.md").exists())
        project_docs.synchronize(self.root, write=True)
        self.assertEqual((self.root / "README.md").read_text(), "Version: 2.4.0\n")
        self.assertEqual(project_docs.synchronize(self.root), [])
        (self.root / "package.json").write_text('{"dependencies":{"example":"3.0.0"}}')
        project_docs.synchronize(self.root, write=True)
        self.assertEqual((self.root / "README.md").read_text(), "Version: 3.0.0\n")

    def test_python_literals_are_read_without_importing_or_executing_the_module(self):
        (self.root / "app.py").write_text(
            "raise RuntimeError('must never execute')\nDEFAULTS = {'model': 'example/v2'}\n"
        )
        value = project_docs.source(
            self.root,
            {"kind": "python-literal", "path": "app.py", "symbol": "DEFAULTS"},
        )
        self.assertEqual(
            project_docs.render("{{defaults.model}}", {"defaults": value}), "example/v2"
        )

    def test_toml_and_json_filter(self):
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
        value = project_docs.source(self.root, {"kind": "toml", "path": "Cargo.toml"})
        self.assertEqual(
            project_docs.render("{{cargo.package.version}}", {"cargo": value}), "1.2.3"
        )
        self.assertEqual(
            json.loads(project_docs.render("{{cargo | json}}", {"cargo": value})), value
        )

    def test_all_templates_are_validated_before_the_first_write(self):
        self.manifest["templates"].append({"source": "bad.md.in", "target": "other.md"})
        (self.root / "bad.md.in").write_text("{{missing.version}}")
        self.write_manifest()
        with self.assertRaises(project_docs.DocError):
            project_docs.synchronize(self.root, write=True)
        self.assertFalse((self.root / "README.md").exists())

    def test_path_escapes_symlinks_and_source_overwrites_are_rejected(self):
        for target in (
            "../outside.md",
            "/tmp/outside.md",
            "package.json",
            "README.md.in",
        ):
            self.manifest["templates"][0]["target"] = target
            self.write_manifest()
            with self.subTest(target=target), self.assertRaises(project_docs.DocError):
                project_docs.synchronize(self.root, write=True)
        self.manifest["templates"][0]["target"] = "README.md"
        (self.root / "README.md").symlink_to(self.root / "package.json")
        self.write_manifest()
        with self.assertRaises(project_docs.DocError):
            project_docs.synchronize(self.root, write=True)

    def test_same_source_and_target_is_rejected_even_for_markdown(self):
        self.manifest["templates"][0] = {"source": "source.md", "target": "source.md"}
        (self.root / "source.md").write_text("{{package.dependencies.example}}")
        self.write_manifest()
        with self.assertRaises(project_docs.DocError):
            project_docs.synchronize(self.root, write=True)

    def test_missing_nix_is_an_environment_blocker(self):
        with (
            patch.object(project_docs.shutil, "which", return_value=None),
            self.assertRaises(project_docs.DocBlocked),
        ):
            project_docs.nix_source(self.root, "lib.versions")

    def test_runner_automatically_enrolls_opted_in_docs_but_not_baseline(self):
        document = {"checks": []}
        self.assertEqual(
            project_check.documentation_checks(self.root, document, "baseline"), []
        )
        check = project_check.documentation_checks(self.root, document, "fast")[0]
        self.assertEqual(check["name"], "dependency-docs")
        self.assertEqual(check["argv"][-1], "--write")
        result = project_check.run_check(self.root, check, self.root)
        self.assertEqual(result["status"], "passed")
        self.assertEqual((self.root / "README.md").read_text(), "Version: 2.4.0\n")

    def test_plan_reports_docs_without_modifying_sources(self):
        with (
            patch.object(
                project_check,
                "baseline_tooling",
                return_value=(self.root, "synthetic-scanner"),
            ),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            report = project_check.plan(self.root, {"checks": []}, "fast")
        self.assertIn("dependency-docs", [check["name"] for check in report["checks"]])
        self.assertFalse((self.root / "README.md").exists())

    def test_existing_check_names_remain_usable(self):
        document = {"checks": [{"name": "dependency-docs"}]}
        checks = project_check.documentation_checks(self.root, document, "fast")
        self.assertEqual(
            [check["name"] for check in checks],
            ["dependency-docs-auto", "dependency-docs"],
        )

    def test_runtime_state_and_non_regular_paths_are_rejected(self):
        for name in (".git/config", ".direnv/state.json", "node_modules/fixture.json"):
            with self.subTest(name=name), self.assertRaises(project_docs.DocError):
                project_docs.local_path(self.root, name)
        (self.root / "directory.md").mkdir()
        with self.assertRaises(project_docs.DocError):
            project_docs.local_path(self.root, "directory.md", output=True)
