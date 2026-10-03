"""Render dependency documentation from explicit, source-backed templates."""

import argparse
import ast
import json
import os
import re
import shutil
import subprocess
import tempfile
import tomllib
from pathlib import Path

MANIFEST = ".dependency-docs.json"
LIMIT = 1_000_000
PLACEHOLDER = re.compile(r"\{\{\s*([\w.-]+)(?:\s*\|\s*(json|table))?\s*\}\}")


class DocError(ValueError):
    pass


class DocBlocked(DocError):
    pass


def local_path(root, name, *, output=False):
    if not isinstance(name, str) or not name or "\0" in name:
        raise DocError("Documentation paths must be nonempty strings")
    relative = Path(name)
    if relative.is_absolute() or ".." in relative.parts:
        raise DocError("Documentation paths must stay inside the repository")
    if any(
        part in {".git", ".direnv", ".venv", "node_modules", "__pycache__"}
        for part in relative.parts
    ):
        raise DocError(
            "Documentation paths must not include repository or runtime state"
        )
    candidate = root / relative
    for component in (candidate, *candidate.parents):
        if component == root:
            break
        if component.is_symlink():
            raise DocError("Documentation paths must not use symlinks")
    if output and (candidate.suffix != ".md" or not candidate.parent.is_dir()):
        raise DocError(
            "Documentation targets require an existing directory and .md suffix"
        )
    if candidate.exists() and not candidate.is_file():
        raise DocError("Documentation inputs and targets must be regular files")
    return candidate


def read(path):
    with path.open("rb") as stream:
        data = stream.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise DocError("Documentation source exceeds 1 MB")
    return data.decode("utf-8")


def nix_source(root, attribute):
    if not isinstance(attribute, str) or not re.fullmatch(r"[\w.-]+", attribute):
        raise DocError("Invalid Nix documentation attribute")
    if not shutil.which("nix"):
        raise DocBlocked("Required program is missing: nix; enter the pinned toolchain")
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        try:
            result = subprocess.run(
                [
                    "nix",
                    "eval",
                    "--offline",
                    "--no-write-lock-file",
                    "--json",
                    f"path:.#{attribute}",
                ],
                cwd=root,
                stdout=output,
                stderr=errors,
                timeout=60,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            raise DocBlocked("Nix documentation evaluation is unavailable") from None
        if result.returncode:
            errors.seek(0)
            diagnostic = errors.read(8192).decode("utf-8", errors="replace")
            if any(
                token in diagnostic
                for token in (
                    "cannot fetch",
                    "unable to download",
                    "not valid",
                    "No such file or directory",
                    "offline",
                )
            ):
                raise DocBlocked(
                    "Offline Nix documentation inputs are unavailable; enter the pinned toolchain"
                )
            raise DocError(
                "Nix documentation evaluation failed; check the declared attribute"
            )
        output.seek(0)
        return json.loads(read_stream(output))


def read_stream(stream):
    data = stream.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise DocError("Documentation source exceeds 1 MB")
    return data.decode("utf-8")


def source(root, spec):
    if not isinstance(spec, dict):
        raise DocError("Documentation source must be an object")
    kind = spec.get("kind")
    if kind == "nix":
        if set(spec) != {"kind", "attribute"}:
            raise DocError("Nix sources require only kind and attribute")
        return nix_source(root, spec["attribute"])
    expected = (
        {"kind", "path", "symbol"} if kind == "python-literal" else {"kind", "path"}
    )
    if set(spec) != expected:
        raise DocError("Invalid documentation source fields")
    text = read(local_path(root, spec["path"]))
    if kind == "json":
        return json.loads(text)
    if kind == "toml":
        return tomllib.loads(text)
    if kind == "python-literal":
        for statement in ast.parse(text).body:
            if isinstance(statement, ast.Assign) and any(
                isinstance(target, ast.Name) and target.id == spec["symbol"]
                for target in statement.targets
            ):
                return ast.literal_eval(statement.value)
        raise DocError("Python documentation symbol is missing or is not a literal")
    raise DocError("Unknown documentation source kind")


def render(template, values):
    def replace(match):
        value = values
        for component in match[1].split("."):
            if not isinstance(value, dict) or component not in value:
                raise DocError(f"Unknown documentation placeholder: {match[1]}")
            value = value[component]
        if match[2] == "json":
            return json.dumps(value, indent=2, ensure_ascii=False)
        if match[2] == "table":
            if not isinstance(value, dict) or any(
                isinstance(v, (dict, list)) for v in value.values()
            ):
                raise DocError("Documentation tables require a flat object")

            def cell(item):
                return str(item).replace("|", "\\|").replace("\n", " ")

            rows = ["| Dependency | Pinned version |", "| --- | --- |"]
            rows.extend(f"| {cell(k)} | {cell(v)} |" for k, v in sorted(value.items()))
            return "\n".join(rows)
        if isinstance(value, (dict, list)):
            raise DocError(
                "Structured documentation values require a json or table filter"
            )
        return str(value)

    result = PLACEHOLDER.sub(replace, template)
    if "{{" in result or "}}" in result or len(result.encode()) > LIMIT:
        raise DocError("Malformed documentation placeholder or oversized output")
    return result


def synchronize(root, *, write=False):
    root = root.resolve()
    manifest = json.loads(read(local_path(root, MANIFEST)))
    if (
        not isinstance(manifest, dict)
        or type(manifest.get("version")) is not int
        or manifest["version"] != 1
        or set(manifest) != {"version", "sources", "templates"}
    ):
        raise DocError(
            "Dependency documentation requires version 1, sources and templates"
        )
    if (
        not isinstance(manifest["sources"], dict)
        or not manifest["sources"]
        or len(manifest["sources"]) > 32
    ):
        raise DocError("Documentation requires 1–32 named sources")
    templates = manifest["templates"]
    if not isinstance(templates, list) or not 1 <= len(templates) <= 32:
        raise DocError("Documentation requires 1–32 templates")
    values = {name: source(root, spec) for name, spec in manifest["sources"].items()}
    rendered, targets, inputs = [], set(), set()
    for spec in manifest["sources"].values():
        if "path" in spec:
            inputs.add(local_path(root, spec["path"]))
    inputs.add(root / MANIFEST)
    for spec in templates:
        if not isinstance(spec, dict) or set(spec) != {"source", "target"}:
            raise DocError("Templates require only source and target")
        template = local_path(root, spec["source"])
        target = local_path(root, spec["target"], output=True)
        if target in targets or target == template:
            raise DocError("Duplicate or self-overwriting documentation target")
        targets.add(target)
        inputs.add(template)
        rendered.append((target, render(read(template), values)))
    if targets & inputs:
        raise DocError("Documentation targets must not overwrite source inputs")
    changed = [
        target
        for target, content in rendered
        if not target.exists() or read(target) != content
    ]
    if write:
        for target, content in rendered:
            if target not in changed:
                continue
            mode = target.stat().st_mode & 0o777 if target.exists() else 0o644
            descriptor, temporary = tempfile.mkstemp(
                prefix=".project-docs-", dir=target.parent
            )
            try:
                with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
                    stream.write(content)
                os.chmod(temporary, mode)
                os.replace(temporary, target)
            finally:
                Path(temporary).unlink(missing_ok=True)
    return [str(target.relative_to(root)) for target in changed]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write", action="store_true", help="Update only declared Markdown targets"
    )
    args = parser.parse_args(argv)
    try:
        changed = synchronize(Path.cwd(), write=args.write)
    except DocBlocked as error:
        print(f"project-docs: blocked: {error}")
        return 127
    except DocError as error:
        print(f"project-docs: {error}")
        return 1
    except (OSError, ValueError, SyntaxError, TypeError, KeyError):
        print("project-docs: invalid manifest, source or template")
        return 1
    if changed:
        print(
            ("Updated: " if args.write else "Stale documentation: ")
            + ", ".join(changed)
        )
        return 0 if args.write else 1
    print("Dependency documentation is current")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
