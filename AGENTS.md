# Repository policy

- This repository owns the portable `project-check` verification runner
  and its immutable portable quality rules.
- Keep code, comments, documentation, and user-facing text in English.
- Do not add credentials, authentication stores, personal files, private keys,
  caches, or other machine state.
- Prefer Nix and small shell helpers. Both runtime programs are native Rust
  binaries; keep first-party Rust `#![forbid(unsafe_code)]`, argv-only and
  dependency-pinned with Cargo.lock. Do not add a Python runtime or interpreter.
- Keep the portable quality rules minimal and language-portable. The
  `.semgrep/portable` rules ship unchanged into the immutable rules package; a
  changed ruleset is a changed store hash.
- Run `project-check fast`, or its declared `bash scripts/check fast` entrypoint
  when the runner is unavailable. That gate runs the Nix formatter,
  all-system no-build flake evaluation, justfile formatting, rustfmt, Clippy
  with denied warnings, native Rust unit/integration tests, and Ruff for the
  rule-replay helper.
  The `quality-rules` Semgrep check is opt-in: run it when the portable rules
  or fixtures change.
- Task-scoped staging is authorized. After review and required checks, commit
  the task changes and push to the current branch's existing upstream. Preserve
  unrelated work.
- Deployment, activation, installation, releases, visibility changes and history
  rewrites require separate authorization.
- Keep operator identities, personal checkout paths, device labels and account
  choices in the consuming host. Functional upstream repository coordinates are
  not credentials.
