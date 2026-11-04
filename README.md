# project-check-nix

Native Rust, argv-only project verification runner. It reads a per-repository
`.project-checks.json` manifest and runs the declared commands with pinned,
explicit arguments — never implicit project discovery or shell evaluation.
Both `project-check` and `project-docs` forbid unsafe code in first-party Rust.
Native backends target Windows, macOS and Linux. Nix remains an optional,
reproducible packaging and development path. Project commands are still explicit:
a portable runner does not translate Bash scripts or supply a project's compiler.

This repository is the standalone packaging of the shared `project-check` runner
previously embedded in the `example-host` toolchain. New repositories point
their `.project-checks.json` at this installed runner instead of copying it.

## Native installation without Nix

The `Portable verification` workflow builds archives and checksums as CI
artifacts. Publication as a GitHub Release is a separate operation. Download the
artifact for your platform, check its SHA-256, and extract the `.tar.gz` before
installing. The archive contains both Rust binaries, fixed rules, dependency
hashes, and the two installers. A bare `cargo install` or copied executable is
not a complete installation.

| Native CI target | Runtime and scanner installation |
| --- | --- |
| Linux x64 | Ubuntu 22.04 / glibc 2.35 baseline; Python 3.10+ with `venv` |
| Linux ARM64 | Ubuntu 22.04 / glibc 2.35 baseline; Python 3.10+ with `venv` |
| macOS Apple Silicon | ARM64 Python 3.10+ with `venv` |
| macOS Intel | x64 Python 3.10+; portable scanner pinned to Intel-compatible Semgrep 1.182.0 |
| Windows x64 | Native PowerShell installation; x64 Python 3.10+ with `venv`; Semgrep's Windows support is beta |
| Windows WSL2 | Use the Linux archive or the Nix installation inside WSL2 |

The Windows Rust binaries statically link the C runtime; CI checks their DLL imports.

The workflow runs native contracts and a real scanner replay on each native
target; a cross-compile alone is not evidence of runtime support. Other Linux
distributions need compatible glibc and Python; Alpine/musl and other CPU
architectures are not native archive targets. Nix can supply an isolated runtime
on its supported Linux hosts. WSL2 follows the Linux path; it does not run native
Windows project tools.

From an extracted archive, choose a **new absolute installation directory**:

```sh
bash install.sh --prefix "$HOME/.local/share/project-check/0.1.0" --python python3
export PATH="$HOME/.local/share/project-check/0.1.0/bin:$PATH"
```

On Windows, run the extracted script in PowerShell:

```powershell
./install.ps1 -Prefix "$env:LOCALAPPDATA/Programs/project-check/0.1.0" -Python python
$env:Path = "$env:LOCALAPPDATA/Programs/project-check/0.1.0/bin;$env:Path"
```

These PATH changes affect the current terminal. Persist the same `bin` directory
in your user PATH if desired. Installation needs package-index access and
downloads the scanner's hash-locked dependencies into `lib/semgrep`, using your
existing Python. Runner and renderer remain native Rust and do not execute
Python source. No global Python packages, hooks, Nix, or administrator privileges
are required. On Debian/Ubuntu, Python's `venv` support may be a separate package.

Installers refuse existing prefixes. Use a new prefix when upgrading and point
PATH at its `bin`; removing an old prefix uninstalls that copy. A failed install
leaves its partial directory for inspection. The installed scanner environment
depends on its original prefix and Python installation: reinstall into a new
prefix to move it. Package-relative runner assets themselves are relocatable.
Do not remove `share/project-check/tools.json` or copy only the binaries; installed
metadata fixes the scanner and rules paths without runtime environment overrides.

Confirm the installation from a project directory:

```sh
project-check --help
project-check baseline --json
project-check fast --plan --json
```

The `baseline` profile needs no manifest. `fast` and `full` need the project's
`.project-checks.json` and declared tools. For example, this check uses the same
native command on all three operating systems:

```json
{
  "version": 2,
  "checks": [{
    "name": "rust-format",
    "argv": ["cargo", "fmt", "--all", "--", "--check"],
    "requires": ["cargo"],
    "timeout_seconds": 60,
    "profiles": ["fast", "full"]
  }]
}
```

On Windows, executable lookup supports native `.exe`/`.com` programs and explicit
paths. `.cmd`/`.bat` shims do not trigger an implicit shell. Name an interpreter
explicitly when a check needs one; for JavaScript tools, prefer `node` followed
by the tool's script path. Manifest versions 1/2 retain their existing semantics;
no checks are silently omitted because of the operating system. Nix commands and
Nix documentation sources still require Nix; JSON/TOML/Python-literal sources do
not. The repository's own authoritative gate intentionally remains Nix-based.

For development without Nix, use Rust 1.88+ and Git:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-support -- -D warnings
cargo test --locked --features test-support
cargo build --release --locked --bin project-check --bin project-docs
```

The feature builds a native child-process fixture for the common contracts; it
is excluded from installed packages. Unix-specific signal and shell contracts
remain additional tests. Run `bash scripts/package TARGET /new/output/directory`
to assemble an archive payload from the checkout. Rust does not need Nix;
running project checks also needs the scanner installation above. Update portable
scanner pins deliberately, then replay the immutable rules on all targets:

```sh
uv pip compile --universal --python-version 3.10 --generate-hashes --no-header \
  packaging/semgrep.in --output-file packaging/semgrep-requirements.txt
```

Semgrep 1.183+ no longer supplies an Intel macOS engine. The portable lock stays
at 1.182.0; Nix retains its separately pinned scanner. The Nix flake exports the
complete runner, core, renderer, dev shells and native test checks for Linux
x64/ARM64 and Apple Silicon macOS. The pinned nixpkgs has dropped Intel macOS
entirely, so Intel Mac users should use the native Rust binaries and portable
installer. Intel macOS remains covered by the native CI job.

## What it runs

`project-check <profile>` supports four profiles and one maintenance command:

| Profile | Behaviour |
| --- | --- |
| `baseline` | Runs only the immutable portable quality rules; no manifest required. |
| `fast` | Baseline plus every manifest check that declares `fast`. |
| `full` | Baseline plus every manifest check that declares `full`. |
| `watch` | Re-runs `fast` whenever tracked/untracked source changes (batched). |
| `migrate` | Previews (`--write` applies) a manifest schema upgrade; runs no checks. |

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
check using the separately exported native companion. A source change here does
not update installed host tools or existing repositories' pins; publication and
host activation remain separate operations.

## Check manifest contract

`.project-checks.json` declares its schema `version`; this runner accepts 1
and 2:

```json
{
  "version": 2,
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

| Field | Since | Meaning |
| --- | --- | --- |
| `name` | 1 | Unique lowercase identifier. |
| `argv` | 1 | Nonempty string array; the first entry is the program. Nothing is passed through a shell. |
| `requires` | 1 | Programs that must be on `PATH`; explicit relative paths resolve from the check's `cwd`. A missing one marks the check `blocked`, not failed. |
| `cwd` | 1 | Optional project-relative directory; it may not escape the project. |
| `timeout_seconds` | 1 | Bounds each check and kills its whole process group. |
| `profiles` | 1 | Nonempty subset of `fast` and `full`. |
| `watch_ignore` | 1 | Top-level glob patterns excluded from change detection. The manifest itself is always watched so an invalid manifest can be repaired. |
| `input_paths` | 2 | Optional exact project-relative files, or directories ending in `/`, for the advisory changed plan. Omit it when scope is unknown; wildcards and parent traversal are rejected. |
| `expected_warnings` | 2 | Optional exact warning lines with a reason and an occurrence limit, described below. |
| `warnings` | 2 | Optional `fail` or `report`, at top level (default for all checks) or per check (override), described below. |

### Schema evolution

Every field records the version that introduced it, and a new field always
raises the schema version. Errors name the side that is outdated:

- A manifest newer than the runner reports both versions and the runner's path:
  update the pinned `project-check` instead of editing the manifest.
- A field newer than the declared version names the field and the version it
  needs: `project-check migrate --write` upgrades the manifest.
- An unknown field is named, together with the runner's supported versions.

`project-check migrate` changes a manifest only when its declared version
cannot express its content. A manifest valid under its own version stays
current, even when older than the runner: the runner keeps every earlier
version's semantics, and older runners keep accepting it. Without `--write` an
outdated manifest exits 1 and nothing is written. Migrations preserve behavior
and the result is validated before an atomic replace. An upgrade that only
adds top-level settings rewrites the version line and inserts the new lines
beneath it with the same indentation, so formatter-owned layout stays intact;
other upgrades are written with two-space indentation.

Adding a field means: register it with the next version, raise
`SCHEMA_VERSION`, add a `MIGRATIONS` step that keeps older manifests' behavior
explicit, and release the runner before manifests adopt the field. Version 1 is
the field set of runners released before `input_paths`; those runners report a
version 2 manifest as `project checks require version 1`.

### Warnings

Complete Nix notices about the caller's environment are retained as diagnostic
context but are never quality failures: the dirty Git tree notice, an ignored
untrusted substituter, an ignored restricted client setting and a transient
download retry (`…; retrying in N ms (attempt i/n)`).

Other warning lines (`warning:`, `…Warning:`, `N warnings`) follow the check's
`warnings` policy. The exit status always decides first; no policy or
exception can overrule a nonzero exit.

| Policy | Effect |
| --- | --- |
| `fail` | Undeclared warning lines and counts fail an otherwise successful check. |
| `report` | Warning lines stay visible and are counted, but do not fail the check. It cannot be combined with `expected_warnings`. |

The effective policy is the check's `warnings`, else `fail` for a check with
`expected_warnings`, else the manifest's top-level `warnings`, else the
version default: `fail` for version 1 and `report` for version 2.

Keep `report` where the tool owns its warning policy (`clippy -D warnings`,
`ruff`, `-W error`, the runner's own `RUSTFLAGS=-D warnings`) or where a
dependency's deprecation output is outside the project's control. Set
`"warnings": "fail"` where warning text is the only signal. The version 1 to 2
migration adds a top-level `"warnings": "fail"`, so migrated manifests keep
their behavior until a check opts into `report`.

Each check captures at most 4 MiB of combined stdout and stderr. Exceeding that
limit stops the check's own process group and reports `failed`, retaining the
bounded prefix with an explicit truncation diagnostic (`output_truncated` in
JSON). Partial output cannot prove success or turn an overflow into an
environment blocker. The remaining checks still run. The timeout also applies
when a check closes its output or a descendant keeps its output pipe open.
A timeout reports `failed` with `timed_out` in JSON: a slow check is a gate
problem (raise `timeout_seconds` or move it to `full`), not a missing tool.

Checks run with a per-run scratch `TMPDIR` (also `TMP` and `TEMP` on Windows).
`CARGO_TARGET_DIR` and
`RUFF_CACHE_DIR` live in a persistent per-project cache,
`$XDG_CACHE_HOME/project-check/<name>-<hash>/` (default `~/.cache`, or
`%LOCALAPPDATA%` on Windows), so Rust
checks reuse earlier builds without sharing the project's own `target/`, whose
different `RUSTFLAGS` would force rebuilds. Deleting the cache is always safe.
Without a writable user cache, such as inside a Nix build, the caches fall back
to the per-run scratch directory.

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
`blocked`, with instructions to install the declared toolchain. Nix projects can
enter `devenv shell` or `nix develop .`, or repair their declared development
shell. A failed command whose output names a missing toolchain or
download also reports `blocked`; the same text in a passing check does not.
Nonzero command exits include their exit status even when the command printed
nothing. Diagnostics from the runner do not echo argv arguments; the invoked
tools remain responsible for their own output.

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

## Runtime safety and resource contracts

The package contains native executables and an immutable adjacent tooling
manifest naming its pinned scanner, rules, CA bundle and renderer. Runtime
environment variables cannot replace those paths. Development Cargo binaries
use the source rule directory, a `semgrep` on PATH and a sibling renderer.
The scanner may itself depend on Python; the runner/renderer do not embed or
invoke a Python interpreter.

Rust prevents first-party unsafe code, not arbitrary project commands. The
runner is not a sandbox, credential boundary or network-policy enforcement
point. Dependencies and platform APIs contain separately maintained unsafe
internals. Repository metadata and paths must remain stable during inspection
and writes. Atomic migration output exclusively creates a new temporary file,
refuses symlink manifests and detects intervening byte changes before replace;
it does not provide a lock against an adversarial concurrent writer.

The Rust runner retains manifest versions 1/2 and report version 1. Deliberate
input hardening rejects duplicate JSON keys, nonstandard NaN/Infinity tokens,
NUL paths, manifests over 1 MiB, more than 128 checks or more than 4096 arguments
per check. Watch ignores permit at most 256 patterns of 4096 bytes each; compiled
fnmatch-compatible matchers are reused between edits, including literal unmatched
brackets and separator-crossing stars. JSON nesting uses the parser's bounded
recursion limit. Scanner reports are read with a 16 MiB bound; aggregate reports
have a 32 MiB budget with capacity reserved for every remaining check identity.
Exceeding a bound cannot establish success. These limits bound runner reads and
retention, not a child process's memory, disk writes or escaped descendants.

On Unix, SIGINT exits 130 and SIGTERM exits 143 after cleaning up the active owned
check process group. Leader state is observed without reaping until group
cleanup, avoiding a signal to a recycled leader PID. On Windows, Ctrl+C exits 130;
the runner assigns the suspended child to a Job Object before resuming it and
cleans up the whole job through stable handles. Abruptly killing the runner is
outside the orderly cleanup guarantee, as are Unix descendants that deliberately
escape their process group. Missing or malformed scanner reports never downgrade
an observed timeout/overflow failure to a blocker. File replacement flushes the
new file before replacement; directory fsync is additionally performed on Unix.
Windows does not claim equivalent directory crash-durability through safe std.

Watch polls and batches filesystem changes, not Git status. Unix additionally
tracks ctime/device metadata to detect ordinary equal-size restored-mtime edits.
Windows streams SHA-256 fingerprints, with a 64 MiB per-file and 256 MiB
per-snapshot read budget; exceeding either blocks with `watch_ignore` guidance.
Links and Windows reparse points are not followed. Logical ignore paths use `/`
on every platform. Trees above 200000 entries or depth 256 block. Polling does not
promise adversarial, race-free content monitoring.

The renderer parses Python literals with a Rust AST parser, never an interpreter.
Syntax support comes from pinned `rustpython-parser` 0.4.0, not the operator's
installed Python; newer Python grammar features are not implicitly supported.
Preflight limits Python sources to 100000 lexer tokens, 1024 per logical
statement, 256 active expression tokens, 128 delimiter/indentation levels and
128 total `elif` clauses per source (including unrelated chains).
Numeric tokens are limited to 4096 bytes. F-string bodies are limited to 4096
bytes and 128 structural punctuation marks/whitespace-separated words, including
literal text; ordinary unrelated f-strings remain accepted. Literal evaluation
also has a 128-level limit. Unsupported nonempty-set scalar formatting is
rejected explicitly; JSON cannot represent Python sets, bytes or complex values.
Plain scalar formatting retains Python boolean/null spelling. Quoted Python
containers use pinned Unicode 17 printability data, including nonprinting
separators, format characters, private-use and unassigned code points.

TOML is lexically checked before recursive deserialization. Its conservative
combined-depth estimate must satisfy `1 + 2H + K(C + 1) + C <= 128`, where `H`
is the maximum table-header component count, `K` the maximum dotted-key
component count and `C` the maximum value-container nesting. Global maxima
can reject large combinations on unrelated branches; string contents and
comments do not contribute fake structure. Exceeding this limit is a normal
error and does not replace generated targets.

Native startup, bounded buffering and shared parsing remove the Python runner
startup path. No measured end-to-end speedup is claimed: Semgrep, Nix, compilers
and project tests still run. Benchmark targets and workloads must be approved
and measured separately with identical coverage and cache states.

## Checks and local development

- `project-check fast` runs the installed baseline and the declared fast gate.
- `bash scripts/check fast` runs Nix formatting checks, all-system no-build flake
  evaluation and the native Rust gate without invoking the baseline itself.
- `nix build .#checks.x86_64-linux.rust-tests` builds both binaries and runs
  rustfmt, Clippy with denied warnings, Rust unit/integration tests and Ruff for
  the remaining portable-rule replay helper. It does not build Semgrep.
- `nix build .#project-check` builds the complete pinned runtime package.
- `nix build .#project-docs` builds the standalone native renderer.
- `nix build .#checks.x86_64-linux.quality-rules` replays the unchanged portable
  rules against their fixtures with real Semgrep (opt-in).

The gate uses Git-backed Nix sources so ignored build output and caches never
enter the source snapshot. Add new source files to Git (intent-to-add is enough)
before running the Nix gate; commits and publication are separate operations.

### Developer environment and recipes

`devenv.nix` and the flake's development shell share
`packages/development-tools.nix`: Rust, Cargo, Clippy, rustfmt, Ruff, Git, Nix,
nixfmt, `just` and the `devenv` CLI. Both use the same immutable nixpkgs
revision; `devenv.lock` records the standalone environment's resolved inputs.
No rustup download, additional Cachix trust or Git-hook installation is needed.

The framework is pinned to `v2.4.0` as well. That release's
`src/modules/latest-version` still reports `2.3.1`, so the CLI emits a
"newer than devenv input" notice. The lock matches the CLI package's framework
source hash; this notice does not mean that the input is unlocked.

With an existing `devenv` CLI:

```sh
devenv shell
```

Without a separately installed CLI, bootstrap it from the pinned flake:

```sh
nix develop --no-write-lock-file -c devenv shell
```

The existing `nix develop --no-write-lock-file` entrypoint also remains usable
directly. Shell entry is explicit; no direnv or native auto-entry approval is
created by this repository. Local `.devenv*` state and operator overrides are
ignored rather than committed.

Inside either environment:

```sh
just                 # List recipes.
just plan            # Read-only fast plan.
just fast            # Pinned project-check baseline and authoritative gate.
just check           # Alias for fast.
just fmt             # Format owned sources.
just fmt-check       # Check formatting without changing files.
just lint            # Clippy, all targets, denied warnings.
just test            # Native Rust unit and integration tests.
just build           # Both Nix-packaged binaries, no installation/result links.
```

`devenv --no-tui --max-jobs 1 --cores 2 test` runs the explicit
`project-check:environment-smoke` lifecycle task before `devenv:enterTest`.
It checks the environment's tools and recipe formatting, not the Rust suite:
it is not a substitute for `just fast` or `just test`.
The fast gate checks `justfile` formatting as well as the existing checks.

Tests include real synthetic child processes, byte-limit boundaries, closed and
inherited pipes, SIGINT/SIGTERM group cleanup, real watch recovery/batching,
schema migration symlink protection, exact warning policy, scanner report
classification, safe Git fixtures and native renderer behavior. The fast gate
executes the native platform only; evaluating ARM64 outputs is not ARM64 runtime
verification. The pre-rewrite source analysis and its historical references
remain in `docs/rust-rewrite-analysis.md`.
