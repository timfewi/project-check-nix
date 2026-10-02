"""Explicit, argv-only project checks; never invoked by project discovery."""

from __future__ import annotations

import argparse
import fnmatch
import json
import math
import os
import re
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

MANIFEST = ".project-checks.json"
MAX_OUTPUT_BYTES = 4 * 1024 * 1024
QUALITY_RULES = "@qualityRules@"
SEMGREP = "@semgrep@"
CA_CERT_FILE = "@cacert@/etc/ssl/certs/ca-bundle.crt"
TOOLCHAIN_HINT = (
    "Enter the project toolchain with `nix develop path:.` (or reload direnv), "
    "then retry. If the tool is still unavailable, declare it in flake.nix "
    "devShells and check its executable permissions/interpreter."
)

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
# Checking uncommitted edits is the normal agent workflow. Nix's dirty-tree
# notice describes input identity, not a lint finding; retain it in diagnostics.
NIX_DIRTY_NOTICE = re.compile(r"warning: Git tree '[^\r\n]+' is dirty")
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
    if (
        not isinstance(document, dict)
        or type(document.get("version")) is not int
        or document["version"] != 1
    ):
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
            "input_paths",
            "expected_warnings",
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
        cwd = check.setdefault("cwd", ".")
        if (
            not isinstance(cwd, str)
            or not cwd
            or Path(cwd).is_absolute()
            or ".." in Path(cwd).parts
        ):
            raise CheckError(f"{name}: cwd must be relative without parent traversal")
        if not (root / cwd).resolve().is_relative_to(root):
            raise CheckError(f"{name}: cwd escapes the project")
        inputs = check.get("input_paths")
        if inputs is not None:
            strings(inputs, f"{name}.input_paths")
            if any(
                path.startswith("/")
                or path == "."
                or ".." in Path(path).parts
                or Path(path.removesuffix("/")).as_posix() != path.removesuffix("/")
                or any(char in path for char in "*?[\\")
                for path in inputs
            ):
                raise CheckError(
                    f"{name}.input_paths must be exact relative paths or directories ending in /"
                )
        expected = check.get("expected_warnings", [])
        if not isinstance(expected, list) or len(expected) > 32:
            raise CheckError(
                f"{name}.expected_warnings must be an array of at most 32 entries"
            )
        messages = set()
        for entry in expected:
            if not isinstance(entry, dict) or set(entry) != {
                "message",
                "reason",
                "max_count",
            }:
                raise CheckError(
                    f"{name}.expected_warnings require message, reason and max_count"
                )
            if any(
                not isinstance(text, str)
                or not text.strip()
                or len(text) > 4096
                or len(text.splitlines()) != 1
                or "\0" in text
                for text in (entry["message"], entry["reason"])
            ):
                raise CheckError(
                    f"{name}.expected_warnings require bounded, nonempty single-line text"
                )
            message = entry["message"]
            if not WARNING.search(message) or message in messages:
                raise CheckError(
                    f"{name}.expected_warnings require distinct warning lines"
                )
            messages.add(message)
            if (
                type(entry["max_count"]) is not int
                or not 1 <= entry["max_count"] <= 100
            ):
                raise CheckError(
                    f"{name}.expected_warnings max_count must be an integer between 1 and 100"
                )
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


def program_available(program: str, cwd: Path) -> bool:
    """Resolve explicit paths where the check will run; otherwise use PATH."""
    if "/" in program:
        path = Path(program)
        if not path.is_absolute():
            path = cwd / path
        return path.is_file() and os.access(path, os.X_OK)
    return shutil.which(program) is not None


def collect_output(process: subprocess.Popen, output, timeout: float) -> str | None:
    """Bound both output storage and waiting, including inherited child pipes."""
    deadline = time.monotonic() + timeout
    cleaned_up = False
    try:
        with process.stdout, selectors.DefaultSelector() as poller:
            poller.register(process.stdout, selectors.EVENT_READ)
            while True:
                if process.poll() is not None and not cleaned_up:
                    stop(process)
                    cleaned_up = True
                if not poller.get_map():
                    try:
                        process.wait(timeout=max(0, deadline - time.monotonic()))
                    except subprocess.TimeoutExpired:
                        return "timeout"
                    return None
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return "timeout"
                for key, _ in poller.select(min(remaining, 0.1)):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        poller.unregister(key.fileobj)
                        continue
                    room = MAX_OUTPUT_BYTES - output.tell()
                    output.write(chunk[:room])
                    if len(chunk) > room:
                        return "output limit"
    finally:
        # Never signal an old process-group id again after cleanup: a long
        # inherited-pipe wait could outlive that id and permit its reuse.
        if not cleaned_up:
            stop(process)


def run_check(root: Path, check: dict, scratch: Path) -> dict:
    result = {
        "name": check["name"],
        "status": "blocked",
        "returncode": None,
        "diagnostics": "",
    }
    cwd = (root / check["cwd"]).resolve()
    if not cwd.is_relative_to(root) or not cwd.is_dir():
        result["diagnostics"] = (
            "working directory is unavailable or escapes the project"
        )
        return result
    missing = [name for name in check["requires"] if not program_available(name, cwd)]
    if missing:
        result["diagnostics"] = (
            "missing required programs: " + ", ".join(missing) + "\n" + TOOLCHAIN_HINT
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
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as error:
            result["diagnostics"] = (
                f"runner could not start {check['argv'][0]!r}: {error}\n{TOOLCHAIN_HINT}"
            )
            return result
        capture_error = collect_output(process, output, check["timeout_seconds"])
        if capture_error == "timeout":
            result["diagnostics"] = (
                f"timeout after {check['timeout_seconds']} seconds\n"
            )
        elif capture_error:
            result["returncode"] = process.returncode
            result["status"] = "failed"
            result["output_truncated"] = True
            result["diagnostics"] = (
                f"output limit exceeded ({MAX_OUTPUT_BYTES} bytes); "
                "partial diagnostics cannot establish a passing check\n"
            )
        else:
            result["returncode"] = process.returncode
            result["status"] = "passed" if process.returncode == 0 else "failed"
        output.seek(0)
        diagnostics = output.read().decode("utf-8", errors="replace")
    result["diagnostics"] += diagnostics
    warnings = Counter(
        line
        for line in diagnostics.splitlines()
        if WARNING.search(line) and not NIX_DIRTY_NOTICE.fullmatch(line)
    )
    allowed = {}
    if check.get("expected_warnings"):
        result["expected_warnings"] = []
        for expected in check["expected_warnings"]:
            count = warnings[expected["message"]]
            result["expected_warnings"].append({**expected, "count": count})
            result["diagnostics"] += (
                f"\nExpected diagnostic ({count}/{expected['max_count']} occurrences): "
                f"{expected['reason']}\n"
            )
            allowed[expected["message"]] = expected["max_count"]
            if count > expected["max_count"]:
                result["diagnostics"] += (
                    "Expected diagnostic occurrence limit exceeded.\n"
                )
    if result.get("output_truncated"):
        # Incomplete output cannot downgrade a real overflow into an
        # environment blocker, even if its prefix contains that diagnostic.
        result["status"] = "failed"
    elif result["returncode"] in (126, 127):
        result["status"] = "blocked"
        result["diagnostics"] += (
            f"\nCommand unavailable (exit {result['returncode']}).\n{TOOLCHAIN_HINT}\n"
        )
    elif ENVIRONMENT_FAILURE.search(diagnostics):
        result["status"] = "blocked"
    elif result["status"] == "passed" and any(
        count > allowed.get(line, 0) for line, count in warnings.items()
    ):
        result["status"] = "failed"
        result["diagnostics"] += "\nWarnings make this required check unsuccessful.\n"
    if result["status"] == "failed" and result["returncode"]:
        result["diagnostics"] += (
            f"\nCommand exited with status {result['returncode']}.\n"
        )
    result["duration_seconds"] = round(time.monotonic() - started, 3)
    return result


def baseline_tooling() -> tuple[Path, str]:
    rules = Path(QUALITY_RULES)
    scanner = SEMGREP
    if QUALITY_RULES.startswith("@"):
        rules = Path(__file__).resolve().parent.parent / ".semgrep" / "portable"
        scanner = shutil.which("semgrep") or "semgrep"
    return rules, scanner


def baseline(root: Path, scratch: Path) -> dict:
    """Run the immutable portable pack and require explicit coverage evidence."""
    rules, scanner = baseline_tooling()
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
            raise TypeError("invalid scanner report")
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
        else "blocked"
        if selected and not any(result["status"] == "failed" for result in selected)
        else "failed",
        "checks": results,
    }
    if json_output:
        print(json.dumps(report), flush=True)
    else:
        for result in results:
            print(f"{result['status']}: {result['name']}")
        print(f"project-check {profile}: {report['status']}", flush=True)
    return report


def git_output(
    root: Path,
    command: list[str],
    env: dict[str, str],
    allowed_codes: tuple[int, ...] = (0,),
) -> bytes:
    """Capture Git metadata without exposing diagnostics or unbounded output."""
    with tempfile.TemporaryFile() as output:
        try:
            process = subprocess.Popen(
                command,
                cwd=root,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            failure = collect_output(process, output, 15)
        except OSError as error:
            raise CheckError("cannot inspect Git changes") from error
        if failure or process.returncode not in allowed_codes:
            raise CheckError("Git change list unavailable or too large")
        output.seek(0)
        return output.read()


def changed_paths(root: Path) -> list[str]:
    """Read project-relative Git changes without running repository hooks or filters."""
    env = {
        key: value for key, value in os.environ.items() if not key.startswith("GIT_")
    }
    env.update(
        GIT_CONFIG_NOSYSTEM="1",
        GIT_CONFIG_GLOBAL=os.devnull,
        GIT_OPTIONAL_LOCKS="0",
        GIT_NO_REPLACE_OBJECTS="1",
        GIT_TERMINAL_PROMPT="0",
    )
    command = [
        "git",
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]
    names = git_output(
        root,
        command
        + [
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process|required)$",
        ],
        env,
        (0, 1),
    )
    drivers = {name.rsplit(b".", 1)[0] for name in names.split(b"\0") if name}
    if (
        len(names) > 32768
        or len(drivers) > 64
        or any(b"=" in driver for driver in drivers)
    ):
        raise CheckError(
            "Git filter configuration cannot be overridden within safe bounds"
        )
    for driver in sorted(drivers):
        for setting, value in (
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ):
            command.extend(["-c", f"{os.fsdecode(driver)}.{setting}={value}"])
    prefix = git_output(root, command + ["rev-parse", "--show-prefix"], env)
    if not prefix.endswith(b"\n"):
        raise CheckError("incomplete Git project prefix")
    raw = git_output(
        root,
        command
        + [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
            "--",
            ".",
        ],
        env,
    )
    if raw and not raw.endswith(b"\0"):
        raise CheckError("incomplete Git status output")
    paths = set()
    records = iter(raw.split(b"\0")[:-1])
    try:
        project_prefix = prefix[:-1].decode("utf-8")
        for record in records:
            if len(record) < 4 or record[2:3] != b" ":
                raise CheckError("invalid Git status record")
            names = [record[3:]]
            if b"R" in record[:2] or b"C" in record[:2]:
                names.append(next(records))
            for name in names:
                path = name.decode("utf-8")
                if path.startswith(project_prefix):
                    paths.add(path[len(project_prefix) :])
    except StopIteration as error:
        raise CheckError("incomplete Git rename record") from error
    except UnicodeDecodeError as error:
        raise CheckError("non-UTF-8 Git path is unsupported") from error
    return sorted(paths)


def relevance(check: dict, paths: list[str]) -> dict:
    if any(path in {MANIFEST, "flake.nix", "flake.lock"} for path in paths):
        return {
            "relevance": "affected",
            "matched_total": 0,
            "matched_examples": ["check or toolchain declaration changed"],
        }
    inputs = check.get("input_paths")
    if inputs is None:
        return {"relevance": "unknown", "matched_total": 0, "matched_examples": []}
    matched = [
        path
        for path in paths
        if any(
            path.startswith(item) if item.endswith("/") else path == item
            for item in inputs
        )
    ]
    return {
        "relevance": "affected" if matched else "unaffected",
        "matched_total": len(matched),
        "matched_examples": matched[:10],
    }


def plan(
    root: Path,
    document: dict,
    profile: str,
    *,
    json_output: bool = False,
    changed: bool = False,
) -> dict:
    """Report selected checks and local prerequisites without running them."""

    rules, scanner = baseline_tooling()
    paths = changed_paths(root) if changed else []

    checks = []
    baseline_missing = [] if program_available(scanner, root) else [scanner]
    checks.append(
        {
            "name": "baseline",
            "status": "ready" if not baseline_missing and rules.is_dir() else "blocked",
            "missing_programs": baseline_missing,
            "cwd_unavailable": False,
            "rules_unavailable": not rules.is_dir(),
        }
    )
    if changed:
        checks[0].update(
            {"relevance": "required", "matched_total": 0, "matched_examples": []}
        )
    for check in document["checks"]:
        if profile not in check["profiles"]:
            continue
        cwd = (root / check["cwd"]).resolve()
        cwd_unavailable = not cwd.is_relative_to(root) or not cwd.is_dir()
        programs = dict.fromkeys([check["argv"][0], *check["requires"]])
        missing = [name for name in programs if not program_available(name, cwd)]
        check_plan = {
            "name": check["name"],
            "status": "blocked" if missing or cwd_unavailable else "ready",
            "missing_programs": missing,
            "cwd_unavailable": cwd_unavailable,
            "timeout_seconds": check["timeout_seconds"],
        }
        if check.get("expected_warnings"):
            check_plan["expected_warnings"] = check["expected_warnings"]
        if changed:
            check_plan.update(relevance(check, paths))
        checks.append(check_plan)
    report = {
        "version": 1,
        "mode": "plan",
        "profile": profile,
        "status": "blocked"
        if any(c["status"] == "blocked" for c in checks)
        else "ready",
        "checks": checks,
    }
    if changed:
        report["change_scope"] = {
            "corpus": (
                "Git tracked and untracked paths beneath selected project; "
                "ignored paths and submodule contents excluded"
            ),
            "total": len(paths),
            "examples": paths[:20],
            "advisory_only": True,
        }
    if json_output:
        print(json.dumps(report), flush=True)
    else:
        for check in checks:
            problems = list(check["missing_programs"])
            if check["cwd_unavailable"]:
                problems.append("working directory unavailable")
            if check.get("rules_unavailable"):
                problems.append("portable rules unavailable")
            detail = f" ({', '.join(problems)})" if problems else ""
            change = f" [{check['relevance']}]" if changed else ""
            print(f"{check['status']}: {check['name']}{change}{detail}")
            for expected in check.get("expected_warnings", []):
                print(
                    f"  expected diagnostic (at most {expected['max_count']} occurrences): "
                    f"{expected['message']}\n  reason: {expected['reason']}"
                )
        print(f"project-check plan {profile}: {report['status']}", flush=True)
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
            if path.is_symlink() or (
                relative != MANIFEST
                and any(fnmatch.fnmatch(relative, pattern) for pattern in patterns)
            ):
                continue
            try:
                stat = path.stat()
                result[relative] = (stat.st_mtime_ns, stat.st_size, stat.st_ino)
            except FileNotFoundError:
                continue
    return result


def report_blocked(error: Exception, *, json_output: bool) -> None:
    if json_output:
        print(
            json.dumps(
                {
                    "version": 1,
                    "status": "blocked",
                    "diagnostics": str(error),
                    "checks": [],
                }
            ),
            flush=True,
        )
    else:
        print(f"project-check: blocked: {error}", file=sys.stderr)


def watch(root: Path, *, json_output: bool) -> int:
    try:
        document = load(root)
    except (CheckError, OSError) as error:
        document = {"watch_ignore": []}
        initial_error = error
    else:
        initial_error = None
    previous = snapshot(root, document.get("watch_ignore", []))
    if initial_error is None:
        run(root, document, "fast", json_output=json_output)
    else:
        report_blocked(initial_error, json_output=json_output)
    changed_at = None
    while True:
        time.sleep(0.2)
        current = snapshot(root, document.get("watch_ignore", []))
        if current != previous:
            previous = current
            changed_at = time.monotonic()
        if changed_at is not None and time.monotonic() - changed_at >= 0.5:
            # Keep the pre-run snapshot so edits during a run schedule one more.
            changed_at = None
            try:
                document = load(root)
            except (CheckError, OSError) as error:
                report_blocked(error, json_output=json_output)
            else:
                run(root, document, "fast", json_output=json_output)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", choices=("baseline", "fast", "full", "watch"))
    parser.add_argument(
        "--plan",
        action="store_true",
        help="inspect prerequisites without running checks",
    )
    parser.add_argument(
        "--changed",
        action="store_true",
        help="annotate a plan with Git change relevance",
    )
    parser.add_argument(
        "--json", action="store_true", help="emit JSON (JSON lines in watch mode)"
    )
    arguments = parser.parse_args(argv)
    if arguments.plan and arguments.profile == "watch":
        parser.error("--plan does not support watch")
    if arguments.changed and not arguments.plan:
        parser.error("--changed requires --plan")
    root = Path.cwd().resolve()
    try:
        if arguments.profile == "watch":
            return watch(root, json_output=arguments.json)
        document = {"checks": []} if arguments.profile == "baseline" else load(root)
        if arguments.plan:
            report = plan(
                root,
                document,
                arguments.profile,
                json_output=arguments.json,
                changed=arguments.changed,
            )
            return 0 if report["status"] == "ready" else 1
        report = run(root, document, arguments.profile, json_output=arguments.json)
        return 0 if report["status"] == "passed" else 1
    except (CheckError, OSError) as error:
        report_blocked(error, json_output=arguments.json)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
