"""Validate synthetic rule annotations against an ordinary Semgrep JSON scan.

The pinned engine's --test path crashes on Rust fixtures. Use the same scanner
as production and fail closed on missing files, errors or unmatched annotations.
"""

import json
import re
import sys
from pathlib import Path

ANNOTATION = re.compile(r"^\s*(?:#|//)\s*(ruleid|ok):\s*(.+)$")
RULE_ID = re.compile(r"^\s*- id:\s*([a-z0-9-]+)\s*$", re.MULTILINE)


def load_report(path: Path) -> dict:
    try:
        report = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(
            f"scanner report is unavailable or invalid: {error}"
        ) from error
    if not isinstance(report, dict):
        raise ValueError("scanner report must be a JSON object")
    return report


def validate(report: dict, rules: Path, fixtures: Path) -> int:
    declared_ids = [
        rule_id
        for path in rules.rglob("*.yml")
        for rule_id in RULE_ID.findall(path.read_text())
    ]
    rule_ids = set(declared_ids)
    if len(rule_ids) != len(declared_ids):
        raise ValueError("duplicate rule IDs")
    expected: set[tuple[str, str, int]] = set()
    negative: set[tuple[str, str, int]] = set()
    targets = sorted(
        path for path in fixtures.iterdir() if path.suffix in {".py", ".rs"}
    )
    if not rule_ids or not targets or report.get("errors"):
        raise ValueError("missing rules/fixtures or Semgrep scan errors")
    for path in targets:
        for line, text in enumerate(path.read_text().splitlines(), start=1):
            if match := ANNOTATION.match(text):
                destination = expected if match[1] == "ruleid" else negative
                for rule_id in match[2].split(","):
                    rule_id = rule_id.strip()
                    if rule_id not in rule_ids:
                        raise ValueError(f"unknown rule annotation: {rule_id}")
                    destination.add((rule_id, path.as_posix(), line + 1))
    for annotations in (expected, negative):
        if {item[0] for item in annotations} != rule_ids:
            raise ValueError("every rule requires positive and negative examples")
    scanned = set(report["paths"]["scanned"])
    if not {path.as_posix() for path in targets} <= scanned:
        raise ValueError("Semgrep skipped a fixture")
    actual = {
        (item["check_id"], item["path"], item["start"]["line"])
        for item in report["results"]
    }
    if actual != expected or actual & negative:
        raise ValueError(
            f"fixture mismatch: missing={sorted(expected - actual)}, "
            f"unexpected={sorted(actual - expected)}"
        )
    return len(rule_ids)


def main() -> None:
    try:
        report = load_report(Path(sys.argv[1]))
        rules = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(".semgrep")
        fixtures = Path(sys.argv[3]) if len(sys.argv) > 3 else Path("tests/semgrep")
        count = validate(report, rules, fixtures)
    except (KeyError, ValueError) as error:
        raise SystemExit(f"Semgrep rule tests failed: {error}") from error
    print(f"Semgrep rule tests: {count} rules passed positive and negative fixtures")


if __name__ == "__main__":
    main()
