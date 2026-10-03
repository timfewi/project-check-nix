# project-check-nix

Portable, argv-only project verification runner. It reads a per-repository
`.project-checks.json` manifest and runs the declared commands with pinned,
explicit arguments — never implicit project discovery or shell evaluation.
Packages, formatters and checks are exported for `x86_64-linux` and
`aarch64-linux`; the fast gate selects the running machine's platform and
evaluates both platforms without building the scanner.

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

Use `project-check fast --plan` (or `full --plan`) to list the selected checks
and inspect local executable, rules and working-directory availability before a
run. Add `--json` for a machine-readable report. Planning reads the manifest
and filesystem only; it never runs the baseline scanner or project commands.
Its `ready` status means prerequisites are present, not that checks passed. The
report names missing programs but omits command arguments.

`project-check fast --plan --changed` adds advisory relevance from Git's tracked
and untracked paths. A check with `input_paths` is marked affected when a changed
path matches; an unmapped check is `unknown`. The baseline is always required,
and changing `.project-checks.json`, `flake.nix` or `flake.lock` marks every
check affected. The plan **never skips checks** and does not change `fast` or
`full` execution. Git ignored paths are outside this change view; unavailable
Git status blocks the changed plan. `--changed` requires `--plan`.

The change view is limited to the selected project, including projects inside
a larger Git checkout. Paths and both sides of renames are project-relative.
Submodule commit changes are included; source changes inside submodules belong
to their own project plans. Git output is bounded to 4 MiB per command and each
command has a 15-second timeout. Inherited `GIT_*` overrides, global/system
configuration, filesystem monitor hooks and external filters are disabled.
The comparison uses Git's built-in normalization rather than external filter
results such as LFS pointers. Repository Git metadata must remain trusted and
stable during the query; the runner is not a filesystem sandbox.

Watch reports a temporarily invalid manifest as blocked and keeps monitoring.
It resumes the fast checks after the manifest becomes valid again.

Every run also reports a scanner coverage summary. Zero findings alone are not
proof of coverage: a run with no supported source files or with parser errors
never reports `passed`.

## Dependency documentation

`project-docs` is bundled with the runner. Repositories opt in with
`.dependency-docs.json`; explicit `project-check fast`, `full` and `watch` runs
then refresh only the declared Markdown targets before project checks. Baseline,
discovery and `--plan` never generate documentation. Nothing is staged or
published. Existing repositories without the manifest keep their checks.

Version-sensitive documentation belongs in templates, with versions read from
the lockfile, dependency manifests, Python literal defaults or a declared Nix
attribute. This prevents a changed version leaving a stale handwritten copy.
The same workflow works in every agent harness; it needs no conversation hook.

```json
{
  "version": 1,
  "sources": {
    "package": {"kind": "json", "path": "package.json"},
    "lock": {"kind": "json", "path": "flake.lock"},
    "versions": {"kind": "nix", "attribute": "lib.dependencyVersions"}
  },
  "templates": [
    {"source": "README.md.in", "target": "README.md"}
  ]
}
```

Templates use `{{package.dependencies.example}}`, `{{versions | table}}` or
`{{package | json}}`. TOML sources use `kind: "toml"`; Python defaults use
`kind: "python-literal"` plus a top-level `symbol`. Python is parsed with AST
literal evaluation and is never imported. Nix uses offline, no-lockfile-write
evaluation of the explicit attribute; it does not build or update dependencies.
That evaluation still trusts the selected repository's Nix source.

`project-docs` alone checks for drift without writing and exits 1 for stale
documentation. `project-docs --write` refreshes it. All templates are rendered
before the first write. Source/target paths must stay in the repository, cannot
use symlinks, and outputs must be Markdown in existing directories. Unknown
placeholders, overlapping inputs/outputs and oversized sources fail. Missing
tools or offline Nix inputs report an environment blocker.

Older pinned runners can declare an explicit `project-docs --write` fast/full
check, or bootstrap a reviewed copy of `runtime/project_docs.py` until the new
runner is released and pinned. A source change here does not update installed
host tools or existing repositories' pins; publication and host activation are
separate operations.

## Check manifest contract

`.project-checks.json` is version 1:

```json
{
  "version": 1,
  "checks": [
    {
      "name": "format",
      "argv": ["nix", "fmt", "--no-write-lock-file", "--", "--ci"],
      "requires": ["nix"],
      "input_paths": ["flake.nix", "src/"],
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
- `requires` lists programs that must be on `PATH`; explicit relative paths
  resolve from the check's `cwd`. A missing one marks the check `blocked`, not failed.
- `cwd` (optional) is a project-relative directory; it may not escape the project.
- `timeout_seconds` bounds each check and kills its whole process group.
- `profiles` is a nonempty subset of `fast` and `full`.
- `input_paths` (optional) lists exact project-relative files or directories
  ending in `/` for the advisory changed plan. Omit it when scope is unknown;
  wildcards and parent traversal are rejected.
- `expected_warnings` (optional) declares exact warning lines with a reason and
  an occurrence limit, as described below. Older runners reject this field;
  update the selected runner before adopting it.
- `watch_ignore` lists glob patterns excluded from change detection. The check
  manifest is always watched so an invalid manifest can be repaired.

Warnings fail an otherwise successful check. The exact Nix `warning: Git tree
'…' is dirty` notice is retained as diagnostic context but is not a quality
failure: checking uncommitted edits is the normal workflow. Undeclared
compiler/linter warnings and warning counts still fail the check; exceptions
cannot overrule a nonzero exit status.

Each check captures at most 4 MiB of combined stdout and stderr. Exceeding that
limit stops the check's own process group and reports `failed`, retaining the
bounded prefix with an explicit truncation diagnostic (`output_truncated` in
JSON). Partial output cannot prove success or turn an overflow into an
environment blocker. The remaining checks still run. The timeout also applies
when a check closes its output or a descendant keeps its output pipe open.

A check may declare a known, intentional diagnostic:

```json
"expected_warnings": [{
  "message": "evaluation warning: optional synthetic transport is disabled",
  "reason": "This isolated fixture deliberately disables the transport.",
  "max_count": 1
}]
```

Add this field to the owning check. Matching uses the complete output line,
including whitespace, with no regex, wildcard or substring matching. The
original diagnostic stays visible; text and JSON results report the reason,
observed count and limit. Zero occurrences are allowed. Additional warning
lines, counts above the limit, nonzero exits and environment blockers keep
their normal failure/blocker behavior. Exceptions do not apply to the immutable
baseline scanner.

The manifest accepts at most 32 distinct expected warning lines per check.
Each entry requires exactly `message`, `reason` and `max_count`; both text fields
must be nonempty single lines of at most 4096 characters, the message must
contain a recognized warning marker, and the limit must be an integer from 1
to 100. The read-only plan includes every declaration for review before running
project commands.

Missing requirements, process-start errors and shell exit codes 126/127 report
`blocked`, with instructions to enter `nix develop path:.` or repair the declared
`devShells` tools. Nonzero command exits include their exit status even when the
command printed nothing. Diagnostics from the runner do not echo argv arguments;
the invoked tools remain responsible for their own output.

The overall text/JSON status is `blocked` when selected checks have environment
blockers but no failures. A real failure takes precedence over a blocker. Both
statuses exit 1, so CI never treats a blocked run as success; `passed` exits 0.

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

Declare the runner in each project's development shell so Just recipes also work
in a normal terminal, independently of an agent's toolbox:

```nix
inputs.project-check.inputs.nixpkgs.follows = "nixpkgs";
# Include project-check in the outputs function's arguments.
devShells.${system}.default = pkgs.mkShell {
  packages = [ project-check.packages.${system}.project-check ];
};
```

Here `system` is the platform of the surrounding per-system outputs block;
use the same value for its `pkgs` and the runner package.

Keep `flake.lock` reviewed and pinned. New `repo-scaffold-nix` templates include
this dependency and forward `just lint --json` to `project-check fast --json`.
Existing projects need a deliberate flake/Justfile update.

Host and agent-only installations are also supported:

```nix
environment.systemPackages = [
  inputs.project-check.packages.${pkgs.stdenv.hostPlatform.system}.project-check
];

# Or, inside the lite sandbox, where the minimal PATH excludes the home profile:
liteRuntime.extraPackages = [
  inputs.project-check.packages.${pkgs.stdenv.hostPlatform.system}.project-check
];
```

## Checks

- `nix build .#project-check` builds the runner.
- `nix build .#checks.x86_64-linux.python-tests` runs Ruff lint/format and the
  contract unit tests (fast, no scanner).
- `nix build .#checks.x86_64-linux.quality-rules` round-trips the portable rules
  against their fixtures with the real Semgrep scanner (opt-in, builds Semgrep).
- `bash scripts/check fast` runs the Nix formatter, `nix flake check --no-build`,
  Ruff lint/format and the Python unit tests.

The 2026-09-30 portability review passed the fast gate, including 25 runner
contract tests in the native Nix unit check, and evaluated both Linux output
sets. ShellCheck, deadnix, statix and the tracked-source privacy scan passed.
ARM64 execution and the opt-in Semgrep replay were not run; source input pins
and portable rule contents are unchanged.

The 2026-10-01 expected-diagnostic review passed `project-check fast` with 28
native contract tests, Ruff and Nix evaluation for both Linux platforms. The
exact-line regression failed before the change, then passed with coverage for
additional warnings, count overflow, nonzero exits, blockers and timeouts.
ShellCheck, deadnix, statix and a complete tracked-source privacy scan passed
with zero findings. Only the small runner script package was built; its
dependencies were already present. That package also passed the real runtime
fast gate with four declared MicroVM notices retained in its report. Source
input pins and baseline rules are unchanged; no scanner rebuild, ARM64
execution, optional replay or host installation ran.

The 2026-10-02 output-bound review reproduced an unchecked output overflow and
passed the fast gate with 32 native contract tests, Ruff and both Linux output
evaluations. Regressions cover benign and warning-bearing overflow, environment
diagnostics in a truncated prefix, exact-boundary capture and timeout after
closed output. Cleanup also signals each owned process group only once. The
packaged CLI rejected a 4 MiB overflow while completing the
following check; its real Python baseline passed. Deadnix and statix passed.
Rules, dependencies and manifest fields are unchanged. No full scanner replay,
ARM64 execution or host installation ran.

The 2026-10-02 changed-plan review reproduced inherited Git overrides, filesystem
monitor and filter execution, and incorrectly scoped nested-project paths. Git
metadata capture now reuses the runner's bounded process collection; change
paths are relative to the selected project. External filters are overridden
within explicit configuration bounds, and submodule contents use their own
plans while changed submodule commits remain visible. The implementation is in
`runtime/project_check.py`; `flake.nix` declares Git for the real fixture tests.

The fast gate passed with 39 native contract tests, Ruff and both Linux output
evaluations. Deadnix and statix passed. A real source CLI changed plan returned
the current four changed paths without executing checks. Regression fixtures
cover repository/configuration overrides, hooks, clean/process filters,
newlines in project paths, renamed/untracked files, submodule ownership,
unrepresentable/excessive filter configuration and output bounds. Upstream
[git-status documentation](https://git-scm.com/docs/git-status) confirmed
root-relative porcelain paths and the NUL-delimited rename contract; the
implementation was verified with Git 2.55.

Dependency pins and portable rules are unchanged. No scanner replay, ARM64
execution, publication or host installation ran. Next: review malformed Git
records and linked-worktree plans.
