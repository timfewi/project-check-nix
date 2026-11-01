# List the available development commands.
default:
    @just --list

alias check := fast

# Run the pinned runner and the repository's authoritative fast gate.
fast:
    nix run --no-write-lock-file .#project-check -- fast

# Inspect the fast plan without running checks.
plan:
    nix run --no-write-lock-file .#project-check -- fast --plan

# Format only owned Rust, Nix, helper and recipe sources.
fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --all
    mapfile -d '' -t nix_files < <(git ls-files --cached --others --exclude-standard -z -- '*.nix')
    nix fmt --no-write-lock-file -- "${nix_files[@]}"
    ruff format --no-cache scripts/check-semgrep-tests.py
    just --fmt

# Check formatting without changing source files.
fmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --all -- --check
    mapfile -d '' -t nix_files < <(git ls-files --cached --others --exclude-standard -z -- '*.nix')
    nix fmt --no-write-lock-file -- --check "${nix_files[@]}"
    ruff format --check --no-cache scripts/check-semgrep-tests.py
    just --fmt --check

# Run Rust linting with warnings denied.
lint:
    cargo clippy --locked --all-targets -- -D warnings

# Run the native unit and integration contracts.
test:
    cargo test --locked

# Build both packaged binaries without creating result links or installing them.
build:
    nix build --no-link --max-jobs 1 --cores 2 --no-write-lock-file .#project-check .#project-docs
