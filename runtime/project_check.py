"""Explicit, argv-only project checks; never invoked by project discovery."""

from __future__ import annotations

import argparse
import fnmatch
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

MANIFEST = ".project-checks.json"
QUALITY_RULES = "@qualityRules@"
SEMGREP = "@semgrep@"
CA_CERT_FILE = "@cacert@/etc/ssl/certs/ca-bundle.crt"

IGNORED = frozenset(
    {
        ".git",
        ".direnv",
        ".cargo-tmp",
        ".ruff_cache",
        ".pytest_cache",
        "__pycache__",
        "target",
        "node_modules",
        "vendor",
        "result",
        "obsidian",
        "law-main",
    }
)
WARNING = re.compile(
    r"(?im)\b(?:[A-Za-z]+Warning|warning)(?:\s*\[|:)|\b[1-9]\d* warnings?\b"
)
ENVIRONMENT_FAILURE = re.compile(
    r"no matching package named|failed to download|can't find crate for|"
    r"required command not found|TOOLCHAIN_(?:STALE|NOT_REGISTERED)|"
    r"Could not find platform independent libraries"
)


class CheckError(ValueError):
    """Invalid project contract."""


def strings(value: object, label: str) -> list[str]:
    if (
        not isinstance(value, list)
        or not value
        or not all(
            isinstance(item, str) and item and "\0" not in item for item in value
        )
    ):
        raise CheckError(f"{label} must be a nonempty string array")
    return value


def load(root: Path) -> dict:
    try:
        document = json.loads((root / MANIFEST).read_text())
    except (OSError, ValueError) as error:
        raise CheckError(f"cannot read {MANIFEST}: {error}") from error
    if not isinstance(document, dict) or document.get("version") != 1:
        raise CheckError("project checks require version 1")
    if set(document) - {"version", "checks", "watch_ignore"}:
        raise CheckError("unknown project-checks field")
    checks = document.get("checks")
    if not isinstance(checks, list) or not checks:
        raise CheckError("checks must be a nonempty array")
    names = set()
    for check in checks:
        if not isinstance(check, dict) or set(check) - {
            "name",
            "argv",
            "cwd",
            "requires",
            "timeout_seconds",
            "profiles",
        }:
            raise CheckError("invalid check fields")
        name = check.get("name")
        if (
            not isinstance(name, str)
            or not re.fullmatch(r"[a-z][a-z0-9-]*", name)
            or name in names
        ):
            raise CheckError("check names must be unique lowercase identifiers")
        names.add(name)
        argv = check.get("argv")
        if (
            not isinstance(argv, list)
            or not argv
            or not all(isinstance(item, str) and "\0" not in item for item in argv)
            or not argv[0]
        ):
            raise CheckError(f"{name}.argv must contain a program and string arguments")
        strings(check.get("requires"), f"{name}.requires")
        profiles = strings(check.get("profiles"), f"{name}.profiles")
        if set(profiles) - {"fast", "full"}:
            raise CheckError(f"{name}: unknown profile")
        timeout = check.get("timeout_seconds")
        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not math.isfinite(timeout)
            or not 0 < timeout <= 3600
        ):
            raise CheckError(f"{name}: timeout must be between 0 and 3600 seconds")
        cwd = check.get("cwd")
        if (
            not isinstance(cwd, str)
            or not cwd
            or Path(cwd).is_absolute()
            or ".." in Path(cwd).parts
        ):
            raise CheckError(f"{name}: cwd must be relative without parent traversal")
        if not (root / cwd).resolve().is_relative_to(root):
            raise CheckError(f"{name}: cwd escapes the project")
    ignore = document.get("watch_ignore", [])
    if not isinstance(ignore, list) or not all(
        isinstance(item, str) for item in ignore
    ):
        raise CheckError("watch_ignore must be a string array")
    return document


def stop(process: subprocess.Popen) -> None:
    # Descendants must not survive a timeout and overlap the next check.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def run_check(root: Path, check: dict, scratch: Path) -> dict:
    result = {
        "name": check["name"],
        "status": "blocked",
        "returncode": None,
        "diagnostics": "",
    }
    missing = [name for name in check["requires"] if shutil.which(name) is None]
    if missing:
        result["diagnostics"] = "missing required programs: " + ", ".join(missing)
        return result
    cwd = (root / check["cwd"]).resolve()
    if not cwd.is_relative_to(root) or not cwd.is_dir():
        result["diagnostics"] = (
            "working directory is unavailable or escapes the project"
        )
        return result
    environment = dict(os.environ)
    environment.update(
        {
            "TMPDIR": str(scratch),
            "CARGO_TARGET_DIR": str(scratch / "cargo-target"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "RUFF_CACHE_DIR": str(scratch / "ruff"),
            "RUSTFLAGS": environment.get("RUSTFLAGS", "") + " -D warnings",
        }
    )
    if not CA_CERT_FILE.startswith("@"):
        # Nix build sandboxes do not expose host trust anchors. Pin both names
        # because Semgrep's native telemetry client uses the system CA lookup.
        environment["NIX_SSL_CERT_FILE"] = CA_CERT_FILE
        environment["SSL_CERT_FILE"] = CA_CERT_FILE
    started = time.monotonic()
    with tempfile.TemporaryFile(dir=scratch) as output:
        try:
            process = subprocess.Popen(
                check["argv"],
                cwd=cwd,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as error:
            result["diagnostics"] = f"runner could not start: {error}"
            return result
        try:
            process.wait(timeout=check["timeout_seconds"])
        except subprocess.TimeoutExpired:
            stop(process)
            result["diagnostics"] = (
                f"timeout after {check['timeout_seconds']} seconds\n"
            )
        except BaseException:
            stop(process)
            raise
        else:
            result["returncode"] = process.returncode
            result["status"] = "passed" if process.returncode == 0 else "failed"
            # A completed check must not leave background descendants for watch.
            stop(process)
        output.seek(0)
        diagnostics = output.read().decode("utf-8", errors="replace")
    result["diagnostics"] += diagnostics
    if ENVIRONMENT_FAILURE.search(diagnostics):
        result["status"] = "blocked"
    elif result["status"] == "passed" and WARNING.search(diagnostics):
        result["status"] = "failed"
        result["diagnostics"] += "\nWarnings make this required check unsuccessful.\n"
    result["duration_seconds"] = round(time.monotonic() - started, 3)
    return result


def baseline(root: Path, scratch: Path) -> dict:
    """Run the immutable portable pack and require explicit coverage evidence."""
    rules = Path(QUALITY_RULES)
    scanner = SEMGREP
    if QUALITY_RULES.startswith("@"):
        rules = Path(__file__).resolve().parent.parent / ".semgrep" / "portable"
        scanner = shutil.which("semgrep") or "semgrep"
    report_path = scratch / "semgrep.json"
    check = {
        "name": "baseline",
        "argv": [
            scanner,
            "scan",
            "--config",
            str(rules),
            "--strict",
            "--no-rewrite-rule-ids",
            # Dedicated rule tests scan these intentional positive fixtures.
            "--exclude",
            "/tests/semgrep/",
            "--exclude",
            "/tests/portable-quality/",
            "--json-output",
            str(report_path),
            "--metrics=off",
            "--disable-version-check",
            "--jobs",
            "1",
            ".",
        ],
        "cwd": ".",
        "requires": [scanner],
        "timeout_seconds": 180,
    }
    if not rules.is_dir():
        return {
            "name": "baseline",
            "status": "blocked",
            "diagnostics": "portable quality rules are unavailable",
        }
    previous = {
        key: os.environ.get(key)
        for key in (
            "SEMGREP_SETTINGS_FILE",
            "SEMGREP_LOG_FILE",
            "SEMGREP_SEND_METRICS",
        )
    }
    os.environ.update(
        {
            "SEMGREP_SETTINGS_FILE": str(scratch / "semgrep-settings.yml"),
            "SEMGREP_LOG_FILE": str(scratch / "semgrep.log"),
            "SEMGREP_SEND_METRICS": "off",
        }
    )
    try:
        result = run_check(root, check, scratch)
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
    result["rules_package"] = str(rules)
    result["language_coverage"] = ["python"]
    try:
        report = json.loads(report_path.read_text())
        if (
            not isinstance(report, dict)
            or not isinstance(report.get("paths"), dict)
            or not isinstance(report["paths"].get("scanned"), list)
            or not isinstance(report.get("results"), list)
            or not isinstance(report.get("errors"), list)
        ):
            raise ValueError("invalid scanner report")
        scanned = report["paths"]["scanned"]
        result["scanned_files"] = len(scanned)
        result["findings"] = report.get("results", [])
        result["parser_errors"] = report.get("errors", [])
        result["advisory_findings"] = [
            finding
            for finding in result["findings"]
            if isinstance(finding, dict)
            and finding.get("check_id") == "python-review-formatted-sql"
            and isinstance(finding.get("extra"), dict)
            and finding["extra"].get("severity") == "WARNING"
        ]
        blocking = len(result["findings"]) - len(result["advisory_findings"])
        result["diagnostics"] += (
            f"\nBaseline policy: {blocking} blocking findings, "
            f"{len(result['advisory_findings'])} SQL review advisories, "
            f"{len(result['parser_errors'])} parser errors.\n"
        )
        if blocking or result["parser_errors"]:
            result["status"] = "failed"
        elif not scanned and result["status"] == "passed":
            result["status"] = "not applicable"
            result["diagnostics"] += "\nNo supported source files were scanned.\n"
        result["diagnostics"] += (
            "\nBaseline covers Python only; "
            "project checks must cover other languages.\n"
        )
    except (
        OSError,
        ValueError,
        TypeError,
    ):
        result["status"] = "blocked"
        result["diagnostics"] += "\nScanner did not produce a valid coverage report.\n"
    return result


def run(root: Path, document: dict, profile: str, *, json_output: bool = False) -> dict:
    results = []
    # The runner owns scratch, outside sources; the isolated worker supplies an
    # executable TMPDIR for native checks on systems with a noexec /tmp.
    with tempfile.TemporaryDirectory(prefix="project-check-") as temporary:
        scratch = Path(temporary)
        result = baseline(root, scratch)
        results.append(result)
        if not json_output:
            print(result["diagnostics"])
        for check in document["checks"]:
            if profile not in check["profiles"]:
                result = {
                    "name": check["name"],
                    "status": "not applicable",
                    "diagnostics": "",
                }
            else:
                if not json_output:
                    print(f"==> {check['name']}", flush=True)
                result = run_check(root, check, scratch)
            results.append(result)
            if not json_output and result["diagnostics"]:
                print(
                    result["diagnostics"],
                    end="" if result["diagnostics"].endswith("\n") else "\n",
                )
    selected = [result for result in results if result["status"] != "not applicable"]
    report = {
        "version": 1,
        "profile": profile,
        "status": "passed"
        if selected and all(result["status"] == "passed" for result in selected)
        else "failed",
        "checks": results,
    }
    if json_output:
        print(json.dumps(report))
    else:
        for result in results:
            print(f"{result['status']}: {result['name']}")
        print(f"project-check {profile}: {report['status']}")
    return report


def snapshot(root: Path, patterns: list[str]) -> dict:
    result = {}
    for directory, directories, files in os.walk(root, followlinks=False):
        directories[:] = sorted(
            name
            for name in directories
            if name not in IGNORED
            and not (Path(directory) / name).is_symlink()
            and not any(
                fnmatch.fnmatch(
                    str((Path(directory) / name).relative_to(root)), pattern
                )
                for pattern in patterns
            )
        )
        for name in files:
            path = Path(directory) / name
            relative = str(path.relative_to(root))
            if path.is_symlink() or any(
                fnmatch.fnmatch(relative, pattern) for pattern in patterns
            ):
                continue
            try:
                stat = path.stat()
                result[relative] = (stat.st_mtime_ns, stat.st_size, stat.st_ino)
            except FileNotFoundError:
                continue
    return result


def watch(root: Path, *, json_output: bool) -> int:
    document = load(root)
    previous = snapshot(root, document.get("watch_ignore", []))
    run(root, document, "fast", json_output=json_output)
    changed_at = None
    while True:
        time.sleep(0.2)
        current = snapshot(root, document.get("watch_ignore", []))
        if current != previous:
            previous = current
            changed_at = time.monotonic()
        if changed_at is not None and time.monotonic() - changed_at >= 0.5:
            document = load(root)
            # Keep the pre-run snapshot so edits during a run schedule one more.
            changed_at = None
            run(root, document, "fast", json_output=json_output)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", choices=("baseline", "fast", "full", "watch"))
    parser.add_argument(
        "--json", action="store_true", help="emit JSON (JSON lines in watch mode)"
    )
    arguments = parser.parse_args(argv)
    root = Path.cwd().resolve()
    try:
        if arguments.profile == "watch":
            return watch(root, json_output=arguments.json)
        document = {"checks": []} if arguments.profile == "baseline" else load(root)
        report = run(root, document, arguments.profile, json_output=arguments.json)
        return 0 if report["status"] == "passed" else 1
    except (CheckError, OSError) as error:
        if arguments.json:
            print(
                json.dumps(
                    {
                        "version": 1,
                        "status": "blocked",
                        "diagnostics": str(error),
                        "checks": [],
                    }
                )
            )
        else:
            print(f"project-check: blocked: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
