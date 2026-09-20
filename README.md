# project-check-nix

Portable, argv-only project verification runner. It reads a per-repository
`.project-checks.json` manifest and runs the declared commands with pinned,
explicit arguments — never implicit project discovery or shell evaluation.

This repository is the standalone packaging of the shared `project-check` runner
previously embedded in the `example-host` toolchain. New repositories point
their `.project-checks.json` at this installed runner instead of copying it.

## What it runs

`project-check <profile>` supports four profiles:

| Profile | Behaviour |
| --- | --- |
| `baseline` | Runs only the immutable portable quality rules; no manifest required. |
| `fast` | Baseline plus every manifest check that declares `fast`. |
| `full` | Baseline plus every manifest check that declares `full`. |
| `watch` | Re-runs `fast` whenever tracked/untracked source changes (batched). |

Every run also reports a scanner coverage summary. Zero findings alone are not
proof of coverage: a run with no supported source files or with parser errors
never reports `passed`.

## Manifest contract

`.project-checks.json` is version 1:

```json
{
  "version": 1,
  "checks": [
    {
      "name": "format",
      "argv": ["nix", "fmt", "--no-write-lock-file", "--", "--ci"],
      "requires": ["nix"],
      "timeout_seconds": 60,
      "profiles": ["fast"]
    }
  ],
  "watch_ignore": [".logs", "*.pyc"]
}
```

- `name` is a unique lowercase identifier.
- `argv` is a nonempty string array; the first entry is the program. Nothing is
  passed through a shell.
- `requires` lists programs that must be on `PATH`; a missing one marks the check
  `blocked`, not failed.
- `cwd` (optional) is a project-relative directory; it may not escape the project.
- `timeout_seconds` bounds each check and kills its whole process group.
- `profiles` is a nonempty subset of `fast` and `full`.
- `watch_ignore` lists glob patterns excluded from change detection.

Warnings fail an otherwise successful check. The exact Nix `warning: Git tree
'…' is dirty` notice is retained as diagnostic context but is not a quality
failure: checking uncommitted edits is the normal workflow. Compiler/linter
warnings, warning counts and nonzero exit statuses still fail the check.

## Offline judgment evaluation

Offline judgment evaluations use the existing manifest contract; no provider
integration or new profile is needed:

```json
{
  "name": "judgment-replay",
  "argv": ["python3", "evaluate.py"],
  "requires": ["python3"],
  "timeout_seconds": 30,
  "profiles": ["fast", "full"]
}
```

Add that entry to `checks`. The evaluator owns questions, fixtures and assertions;
the runner uses its exit status and diagnostics. A model claiming success or
returning high confidence cannot overrule a failing evaluation. Offline replay
proves code behavior, not model accuracy. Live/paid evaluation remains an explicit
separate operation; this runner is not a network sandbox.

## Portable quality rules

The immutable baseline scans Python with three portable Semgrep rules under
`.semgrep/portable`: no shell execution, no disabled TLS verification, and no
interpolated SQL. A changed ruleset changes the store hash of the
`quality-rules` package.

## Using it

From the flake:

```nix
inputs.project-check.url = "git+https://github.com/timfewi/project-check-nix.git?ref=main";
```

Install the runner on the host and make it reachable to agents:

```nix
environment.systemPackages = [ inputs.project-check.packages.x86_64-linux.project-check ];

# Or, inside the lite sandbox, where the minimal PATH excludes the home profile:
liteRuntime.extraPackages = [ inputs.project-check.packages.x86_64-linux.project-check ];
```

## Checks

- `nix build .#project-check` builds the runner.
- `nix build .#checks.x86_64-linux.python-tests` runs the contract unit tests
  (fast, no scanner).
- `nix build .#checks.x86_64-linux.quality-rules` round-trips the portable rules
  against their fixtures with the real Semgrep scanner (opt-in, builds Semgrep).
- `bash scripts/check fast` runs the formatter, `nix flake check --no-build` and
  the Python unit tests.
