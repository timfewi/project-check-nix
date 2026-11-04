#!/usr/bin/env bash
# Install a downloaded native bundle into a new, explicit user-owned prefix.
set -euo pipefail

bundle="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
prefix=""
python="python3"
while (($#)); do
  case "$1" in
    --prefix|--python)
      if (($# < 2)); then echo "Missing value for $1" >&2; exit 2; fi
      if [[ "$1" == --prefix ]]; then prefix="$2"; else python="$2"; fi
      shift 2
      ;;
    *) echo "Usage: bash install.sh --prefix /absolute/new/directory [--python python3]" >&2; exit 2 ;;
  esac
done
if [[ "$prefix" != /* || "$prefix" == / ]]; then
  echo "Choose a new absolute installation directory with --prefix." >&2
  exit 2
fi
if [[ -e "$prefix" || -L "$prefix" ]]; then
  echo "Installation directory already exists; choose a new prefix." >&2
  exit 1
fi
command -v "$python" >/dev/null || { echo "Python 3.10+ with venv is required for Semgrep." >&2; exit 1; }
python_version="$("$python" --version 2>&1)"
if [[ ! "$python_version" =~ ^Python[[:space:]]+([0-9]+)\.([0-9]+) ]] \
  || ((BASH_REMATCH[1] < 3 || BASH_REMATCH[1] == 3 && BASH_REMATCH[2] < 10)); then
  echo "Python 3.10+ with venv is required for Semgrep." >&2
  exit 1
fi
[[ -x "$bundle/bin/project-check" && -x "$bundle/bin/project-docs" ]] || {
  echo "Run this installer from an extracted native project-check bundle." >&2; exit 1;
}
mkdir -p -- "$(dirname -- "$prefix")"
mkdir -- "$prefix"
# Leave an interrupted installation visible; never delete caller-supplied trees.
mkdir -p -- "$prefix/bin" "$prefix/share/project-check" "$prefix/lib"
# Install fail-closed metadata before any runnable binary can be discovered.
cp -- "$bundle/share/project-check/tools.json" "$prefix/share/project-check/tools.json"
cp -- "$bundle/bin/project-check" "$bundle/bin/project-docs" "$prefix/bin/"
cp -R -- "$bundle/share/project-check/rules" "$prefix/share/project-check/"
cp -- "$bundle/LICENSE" "$prefix/LICENSE"
"$python" -m venv "$prefix/lib/semgrep"
"$prefix/lib/semgrep/bin/python" -m pip install --disable-pip-version-check --only-binary=:all: --require-hashes \
  -r "$bundle/semgrep-requirements.txt"
SEMGREP_ENABLE_VERSION_CHECK=0 SEMGREP_SEND_METRICS=off "$prefix/lib/semgrep/bin/semgrep" --version
cat > "$prefix/share/project-check/tools.json" <<'JSON'
{"version":2,"rules":"share/project-check/rules","scanner":"lib/semgrep/bin/semgrep","docs":"bin/project-docs","ca":null}
JSON
"$prefix/bin/project-check" --help >/dev/null
printf 'Installed project-check in %s\nAdd %s/bin to PATH.\n' "$prefix" "$prefix"
printf 'Keep this installation at its chosen prefix; rerun the installer into a new prefix to move it.\n'
