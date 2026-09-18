# Repository policy

- This private repository owns the portable `project-check` verification runner
  and its immutable portable quality rules.
- Keep code, comments, documentation, and user-facing text in English.
- Do not add credentials, authentication stores, personal files, private keys,
  caches, or other machine state.
- Prefer Nix and small shell helpers. The runner is a single dependency-free
  Python file (`runtime/project_check.py`); keep it standard-library-only and
  argv-only. Do not add a third-party Python dependency.
- Keep the portable quality rules minimal and language-portable. The
  `.semgrep/portable` rules ship unchanged into the immutable rules package; a
  changed ruleset is a changed store hash.
- Run `project-check fast`, or its declared `bash scripts/check fast` entrypoint
  when the runner is unavailable. That gate runs the Nix formatter,
  `nix flake check --no-build` and the Python unit tests. The `quality-rules`
  Semgrep check is opt-in: run it when the portable rules or fixtures change.
- Do not stage, commit, push, activate, deploy, install, or rewrite history
  without explicit authorization.
- Keep operator identities, personal checkout paths, device labels and account
  choices in the consuming host. Functional upstream repository coordinates are
  not credentials.
