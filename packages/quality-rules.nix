# Immutable portable quality rules: only the three portable Semgrep rule files,
# with a SHA-256 manifest so a changed ruleset is visible in its store hash.
{ pkgs }:
pkgs.runCommand "project-check-quality-rules" { } ''
  mkdir -p "$out"
  cp ${../.semgrep/portable/python-process.yml} "$out/python-process.yml"
  cp ${../.semgrep/portable/python-http.yml} "$out/python-http.yml"
  cp ${../.semgrep/portable/python-sql.yml} "$out/python-sql.yml"
  cd "$out"
  ${pkgs.coreutils}/bin/sha256sum *.yml > SHA256SUMS
''
