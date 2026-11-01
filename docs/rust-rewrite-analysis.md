# Rust rewrite: behavior, risks and acceptance requirements

## Scope and evidence

This records the pre-rewrite analysis, not the current implementation or an
implementation schedule. The intended successor is faster, memory-safe and
dependable for both human operators and agents. Compatibility and explicit
failure reporting are release requirements, not optional tradeoffs for speed.

Historical source baseline: commit `8aafbb522294b507a003d94a8a3f946d8e52380b`.
References below identify files, symbols and line ranges at that revision,
including Python files subsequently replaced by Rust. Verification results and
"current" behavior in this document describe that analysis snapshot. See the
README and native tests for the implemented successor. The analysis itself
changed only this document; installation, publication and activation were excluded.

Evidence labels:

- **Source:** behavior traced in the current implementation and existing tests.
- **Reproduced:** a bounded synthetic probe exercised the behavior; details and
  limitations are included below. These are not performance measurements.
- **Requirement:** proposed acceptance behavior for the later rewrite, not a
  claim that an implementation or test harness already exists.
- **Hypothesis:** a performance opportunity that still needs measurement.

At analysis time, no Rust toolchain, crate versions, performance target or
benchmark budget had been selected. No benchmark or external consumer audit was
run. There was no `.tabula/ENTRY.md`; orientation came from source, repository
guidance and the README rather than an assumed current orientation card.

## Executive assessment

1. The current program is more than a subprocess wrapper. It owns schema
   evolution, warning policy, bounded capture and cleanup, scanner coverage,
   Git-safe advisory plans, watch recovery, caches and documentation generation.
   Replacing only command spawning would lose important contracts.
2. Rust is a reasonable fit for a small native orchestrator. It can provide
   memory safety in first-party safe Rust, typed states and lower interpreter
   startup overhead. This does not prove a speedup or eliminate filesystem
   races, subprocess escapes, bad status classification or unsafe dependencies.
   The existing Python code is not evidence of an unmanaged-memory defect.
3. Three concrete defects were reproduced: an unsafe migration temporary path,
   an uncaught NUL-path error and a baseline failure downgrade. These should be
   explicit corrections, not behaviors to preserve in a differential rewrite.
4. Much of a real check can be external work: Semgrep, Nix evaluation/builds,
   compilers and tests. A faster orchestrator cannot remove that work without
   changing coverage. Existing persistent compiler caches already address some
   repeated-build cost.
5. The rewrite must preserve the `project-docs` companion contract. Rewriting
   only `project-check` does not remove the Python renderer or Semgrep's runtime
   dependencies from the packaged closure.

## Source map

| Concern | Primary evidence |
| --- | --- |
| CLI and top-level error handling | `runtime/project_check.py:1020-1124`, `report_blocked`, `main` |
| Schema, defaults and validation | `runtime/project_check.py:22-38,92-269`, `read_manifest`, `validate` |
| Execution, capture, warnings and cleanup | `runtime/project_check.py:272-455`, `stop`, `collect_output`, `run_check` |
| Baseline and aggregate status | `runtime/project_check.py:458-573,624-669`, `baseline`, `run` |
| Cache and documentation insertion | `runtime/project_check.py:576-621` |
| Git and advisory planning | `runtime/project_check.py:672-901` |
| Migration and watch | `runtime/project_check.py:904-1017,1037-1065` |
| Companion renderer | `runtime/project_docs.py:27-265` |
| Packaging and supported systems | `packages/project-check.nix:6-30`, `flake.nix:9-67` |
| Fast gate | `.project-checks.json:1-12`, `scripts/check:8-13`, `flake.nix:38-56` |
| Portable scanner rules | `.semgrep/portable/python-process.yml`, `python-http.yml`, `python-sql.yml` |
| Existing behavioral tests | `tests/test_project_check.py`, `tests/test_project_baseline.py`, `tests/test_project_docs.py` |

## Current externally visible contracts

### CLI, root selection and status

The resolved current working directory is the project root. There is no upward
manifest search, implicit check discovery, `--root` option or `--version` option
in the current parser (`main`, lines 1068-1120).

| Invocation | Contract |
| --- | --- |
| `project-check baseline` | Runs the portable baseline without loading the project manifest. |
| `project-check fast` | Baseline, optional documentation refresh, then checks declaring `fast`. |
| `project-check full` | Same sequence, selecting `full`; **not** an implicit superset of `fast`. |
| `project-check watch` | Initial fast run, then batched fast reruns; temporarily invalid manifests are reported and can recover. |
| `project-check migrate` | Read-only schema upgrade preview; runs no checks. |
| `project-check migrate --write` | Applies a needed schema upgrade, not an unconditional upgrade to the latest version. |
| `project-check {baseline,fast,full} --plan` | Inspects prerequisites; does not run the scanner, project commands or documentation renderer. |
| `--plan --changed` | Additionally invokes bounded Git metadata queries; annotations never skip checks. |
| `--json` | Structured JSON; watch emits one JSON object per report line. |

Exit codes: `0` for a passed run, ready plan, current manifest or successful
migration; `1` for a failed/blocked run, blocked plan, invalid contract or
outdated migration preview; argparse usage errors exit `2`; caught keyboard
interrupts return `130`. Usage errors are argparse text, even with `--json`.
`--plan` rejects watch/migrate, `--changed` requires `--plan`, and `--write`
requires migrate. Do not silently replace these with a new CLI shape.

Normal run JSON has `version: 1`, `profile`, `status`, and an ordered `checks`
array. Manifest schema version 2 is a different version domain. Plan JSON adds
`mode: "plan"`, and manifest/schema versions where applicable. Migration JSON
has `mode: "migrate"`, status and from/to versions. A top-level blocked report
instead has `version`, `status`, `diagnostics` and an empty `checks` array; it
currently omits profile/mode. Per-check fields are conditional: early blockers
lack durations, timeout return codes are null, and baseline records add coverage
and findings. Preserve these distinctions or explicitly version a change.

The aggregate ignores `not applicable` entries, prioritizes a failure over a
blocker, and passes only when at least one applicable result exists and all
applicable results pass. Crucially, a `not applicable` baseline plus a passing
project check **can** produce overall `passed`; baseline alone with no supported
files does not pass. Thus overall success is not evidence of Python coverage
(`run:652-660`; reproduced with a mocked empty-coverage baseline).

### Manifest and warning policy

`read_manifest` accepts integer versions 1 and 2, rejects booleans as versions,
and distinguishes a newer manifest from an unknown or too-new field. Top-level
fields are `version`, `checks`, `watch_ignore`, and version-2 `warnings`.

| Field | Current validation/default |
| --- | --- |
| `checks` | Nonempty list; check objects reject unknown fields. |
| `name` | Unique `[a-z][a-z0-9-]*` within manifest checks. |
| `argv` | Nonempty array of NUL-free strings; program nonempty, later empty arguments allowed. |
| `requires` | Required nonempty array of nonempty NUL-free program names/paths. Generated internal checks can use an empty list. |
| `profiles` | Nonempty string array containing only `fast` and/or `full`. |
| `timeout_seconds` | Required finite number, not boolean, strictly greater than 0 and at most 3600. Fractional values accepted. |
| `cwd` | Defaults to `.`; relative, without parent traversal, and resolved beneath root. Existence is checked in planning/execution. NUL handling is defective; see F2. |
| `input_paths` | Version 2; exact normalized relative files or trailing-slash directories, not glob patterns or parent traversal. Missing/null means unknown scope. |
| `watch_ignore` | Optional array of glob strings; defaults to empty. |
| `warnings` | Version 2, top-level or per-check: `fail` or `report`. |
| `expected_warnings` | Version 2; at most 32 distinct exact warning messages, each with exactly message/reason/max_count. |

Effective warning policy is: explicit check setting; otherwise `fail` for a
nonempty expected-warning list; otherwise top-level setting; otherwise `fail`
for version 1 and `report` for version 2. `report` plus a nonempty exception list
is invalid. Empty exception lists do not force strictness.

Messages and reasons must be nonempty single lines, at most 4096 characters,
without NUL. A message needs a recognized warning marker; max_count is an
integer from 1 to 100, not boolean. Counts use complete output lines with exact
whitespace, not regex matching; zero occurrences are allowed. Recognized warning
text includes `warning:`, `...Warning:` and nonzero warning counts. Four complete
Nix environment notice patterns are retained but excluded from warning failure.
No warning exception can turn a nonzero exit into success. Source:
`WARNING`, `NIX_ENVIRONMENT_NOTICE`, `validate`, `run_check:404-453`;
regressions at `tests/test_project_check.py:498-617,743-903`.

Migration preserves a valid older manifest unchanged. A version-1 document
using version-2 fields is upgraded only if the upgraded result validates. The
migration explicitly adds top-level `warnings: "fail"` to retain version-1
semantics, tries to preserve formatter-owned layout, otherwise writes two-space
JSON. Preview does not write. The current final replace is atomic, but its
fixed temporary-file creation is unsafe (F1). Tests:
`test_migrate_upgrades_only_manifests_their_version_cannot_express`.

### Execution, cleanup and environment

Execution is sequential: baseline first, then the auto-generated documentation
check if opted in, then declared checks in manifest order. Failures do not stop
later checks. Run reports include nonselected checks as `not applicable`; plans
omit them. A shell is not implicitly involved, but a manifest can explicitly
name a shell or any other executable. On an explicitly authorized run, entries
can execute arbitrary programs with the caller's UID. A manifest is not an
authorization boundary, and argv-only execution is not a sandbox.

Each check receives null stdin and merged stdout/stderr, with a new process
session/group. Capture stores at most 4 MiB in a temporary file. UTF-8 decoding
replaces invalid bytes. Timeout and output overflow fail, mark `timed_out` or
`output_truncated`, and kill/reap the owned process group. Timeout remains
applicable after output closes; inherited pipes do not allow unbounded waiting.
A group is signaled only once after cleanup. These guarantees cover that process
group, not an adversarial descendant that deliberately creates a new session.

Missing requirements, unavailable/escaping cwd, spawn errors and exits 126/127
are blockers. Certain environment-failure strings reclassify a failed command
as blocked, but not a successful command or an output-limit/timeout failure in
`run_check`. Other nonzero exits fail and retain their exit code in diagnostics.
Runner-generated startup diagnostics omit argument values; child output is not
redacted (`run_check:327-455`).

The child mostly inherits the environment. Overrides provide per-run `TMPDIR`,
`PYTHONDONTWRITEBYTECODE=1`, cache locations, and append ` -D warnings` to
`RUSTFLAGS`. Packaged executions set pinned CA paths. Compiler/Ruff caches use
`$XDG_CACHE_HOME` or `$HOME/.cache`, a root-name/root-hash namespace, and fall
back to scratch when unavailable. Cache reuse is not proof of check success and
must never become a reason to skip a check (`compiler_cache`, cache regression
at `tests/test_project_check.py:707-741`).

### Baseline and coverage

The packaged runner embeds immutable paths for rules, scanner, CA bundle and
renderer. The source runner has local development fallbacks. Semgrep runs with
one job, strict mode, metrics/version checking disabled, a 180-second timeout,
and exclusions for the two intentional rule-fixture directories. Baseline
settings/log files live in scratch; environment overrides are restored after
the scan. The rules cover **Python only**. Process-shell and disabled-TLS
findings block; only the exact formatted-SQL rule at WARNING severity is treated
as a nonblocking advisory. Unknown finding IDs/severities remain blocking.

Coverage requires a structurally valid JSON report with scanned paths, results
and errors. Findings/parser errors fail; empty coverage on an otherwise passing
scan is `not applicable`; an invalid/missing report normally blocks. F3 records
an important failure-precedence defect. The report file itself is read without
an explicit byte bound, unlike subprocess output (`baseline:528-572`).

### Changed plans and watch are different mechanisms

`changed_paths` strips inherited `GIT_*` variables, disables global/system Git
configuration, optional locks, replace objects, terminal prompting, fsmonitor
and hooks, and neutralizes declared clean/smudge/process filters. Filter names
and counts have additional bounds. Each Git query has a 15-second deadline and
4 MiB output cap. Porcelain-v1 NUL records retain both rename sides and scope
paths to the selected project. Non-UTF-8 paths block. Ignored paths and dirty
submodule contents are excluded; changed submodule commits are included.
Repository metadata must still be trusted and stable. These settings are not a
filesystem isolation boundary (`changed_paths`; real Git tests at
`tests/test_project_check.py:1051-1243`).

Baseline relevance is always `required`; changes to the manifest, flake or lock
mark every other planned check affected. Missing input mappings are `unknown`.
Exact-file/directory-prefix matches determine other annotations, with bounded
examples. None of these annotations change execution.

Watch does **not** use Git's change view or `.gitignore`. It walks nonignored
filesystem entries every 0.2 seconds, excludes symlinks and built-in/generated
subtrees, applies `watch_ignore`, and records mtime-nanoseconds, size and inode.
It waits 0.5 seconds after the latest detected change before rerunning fast.
Keeping the pre-run snapshot schedules a subsequent run for edits during a
check. Ordinary manifest files bypass custom ignore patterns; symlinked files
are still excluded. Existing batching/recovery tests mock clocks and snapshots;
they are not an end-to-end OS signal/watch stress test. Same-size edits with
restored mtime can be missed (F4).

### Companion renderer and Nix package boundary

Presence of `.dependency-docs.json` inserts `project-docs --write` before project
checks in fast/full, including watch's fast runs. Baseline/plan do not render.
The generated name appends `-auto` until it avoids manifest check-name collisions.
Documentation generation is therefore an existing intentional source mutation
on explicit runs, not merely a reporting feature.

The renderer supports JSON, TOML, Python AST literal sources without module
execution, and an explicitly declared offline/no-lock-write Nix attribute.
It bounds source reads to 1 MB, allows 1-32 sources/templates, rejects runtime
state paths, symlinks and input/output overlap, validates all rendering before
writes, and atomically replaces each changed Markdown file. This is **not** a
transaction across all target files: later I/O failure can leave earlier files
updated. Nix evaluation still executes the selected repository's Nix logic;
its temporary output files are not bounded while the subprocess writes. Path
validation followed by use is not race-proof against concurrent mutation.
Source: `runtime/project_docs.py:27-143,179-265`; tests in
`tests/test_project_docs.py:31-141`.

Both `x86_64-linux` and `aarch64-linux` outputs exist. The package exports
`project-check` and `project-docs`, with isolated Python shebangs today; it also
exports quality rules separately. A native runner must not silently remove the
renderer, change immutable rule semantics or claim non-Linux support. This
repository's fast check selects the native system; evaluation of another
system's outputs is not execution on that system.

## Findings and threat boundaries

### Reproduced findings

All probes used the unchanged source and the approved Python toolbox
(Python 3.15.0). Writes were confined to an owned temporary fixture directory;
no real external file, credential or user configuration was targeted. These
probes supplement the existing tests; no regression tests were added to source.

| ID | Evidence and reproduction | Impact and required correction |
| --- | --- | --- |
| F1 | `migrate:964-969` writes the predictable `..project-checks.json.migrate` path before `os.replace`. In a disposable project, a version-1 manifest using `input_paths` plus a pre-existing symlink at that temporary name to a synthetic sibling file caused that sibling to be overwritten; the final manifest became a symlink. Actual migration code executed. | Explicit migration in a prepared checkout can write outside the project with operator permissions. Use exclusively created, non-following temporary output and define safe symlink/concurrent-write handling; verify the sibling remains unchanged. Atomic rename alone is insufficient. |
| F2 | `validate:193-202` checks cwd traversal but not NUL; `main:1116-1118` does not catch general ValueError. A valid version-2 envelope with `cwd: "bad\u0000cwd"`, passed to `main(["fast", "--plan", "--json"])` in a temporary project, raised uncaught ValueError and emitted no JSON. | Malformed input breaks the agent-facing error contract. Reject NUL paths as a structured blocked contract error before filesystem resolution; exit 1 without a traceback or child execution. |
| F3 | `baseline:565-572` preserves timeout failure but not output-overflow failure when coverage parsing fails. A real synthetic scanner child exceeded a reduced 256-byte capture cap and wrote no report: the result retained `output_truncated: true` but changed to `blocked`. | Failure precedence is inconsistent with `run_check` and the documented output-limit contract. Both states exit nonzero, so this is not false success; it is misleading classification/remediation. Preserve real timeout/overflow failures when coverage is missing or malformed. |
| F4 | `snapshot:1013-1014` compares metadata, not bytes. An in-place equal-length edit followed by restoration of the original mtime produced an unchanged snapshot despite changed content. | A concrete detection limitation, not a claim of general editor failure or compromise. The rewrite must explicitly choose/document whether metadata-preserving edits are supported; if complete content-change detection is promised, this fixture must trigger a rerun. |

A fifth probe confirmed the empty-baseline aggregation behavior described above:
with only the baseline mocked to `not applicable`, a real successful project
check produced overall `passed`. This is a coverage-contract clarification, not
an asserted vulnerability or a proposed silent behavior change.

### Source-backed risks not demonstrated as exploits

- **Resource bounds are incomplete.** Manifest text, check count, argument sizes
  and nesting are not explicitly bounded (`read_manifest`, `validate`). Each
  check output is bounded, but `run` retains all result diagnostics, so aggregate
  memory can scale with check count. Semgrep's JSON file and logs have no explicit
  writer-side quota here. The renderer bounds its Nix result read, not the
  temporary output written during the subprocess. Rust allocation safety does
  not prevent resource exhaustion.
- **Filesystem checks are not race-proof.** Cwd resolution/rechecks, renderer
  symlink checks, cache creation and migration validation occur before later
  operations. Stable trusted checkout/cache state is an assumption. The concrete
  fixed-temp exploit is F1; broader races were not stress-tested.
- **Process groups are cleanup, not containment.** Neither a cgroup/job boundary
  nor cross-process locking is implemented in this source. Detached descendants,
  external concurrent runs and raw SIGTERM cleanup need explicit contracts and
  real-process tests; the source does not justify promising universal cleanup.
- **Output is untrusted.** Warning/blocker classification is text-based. Child
  stdout can contain secrets, control sequences or misleading instructions.
  Agent integrations must treat it as data, not authority. Omitting argv from
  runner diagnostics is useful but not a general secrecy guarantee.
- **Report identity can collide.** Manifest names are unique only within the
  manifest; `baseline` is not reserved (`validate:165-172`, `run:631-646`). Agents
  cannot safely assume names are globally unique across internal and declared
  checks. A future reserved-name or typed-identity policy needs an explicit
  compatibility decision, not an accidental new restriction.

The trusted enforcement layer for operator permissions, network egress and
workload ceilings remains the host. The runner executes authorized commands;
it is not a credential boundary, sandbox, approval system or security proof.

## Performance analysis: hypotheses, not measurements

A useful decomposition is total time = runner startup/validation + filesystem
and Git inspection + scanner + documentation refresh + declared commands +
reporting. A language rewrite primarily changes the first, inspection and
reporting components. This source does not establish which dominates real
consumer projects.

| Candidate | Source evidence | Constraint on an optimization |
| --- | --- | --- |
| Native startup and typed parsing | Python imports/regexes and JSON validation precede execution; `main`, `read_manifest`. | Measure small read-only plans separately; do not infer full-gate speedup from startup. |
| Watch traversal | Full nonignored tree walk every 0.2 seconds; `snapshot`, `watch`. | Event-driven or incremental detection must retain batching, recovery and edits-during-run behavior, handle lost events, and resolve F4's promise. |
| External scanner work | Every run invokes Semgrep; fixed one-job baseline. | Do not skip baseline, loosen rules or report unsupported languages as covered to obtain a faster number. |
| Sequential command duration | `run` executes one check at a time. | Parallelism is a behavioral change: dependencies, shared caches, output order and resource budgets matter. Retain sequential semantics unless separately approved. |
| Persistent cache reuse | Root-scoped Cargo/Ruff cache already exists. | Compare equally warm or equally cold caches; cache deletion is not a fair Rust-vs-Python comparison. |
| Buffering and retained diagnostics | Disk capture, decode, line counting and aggregate result retention. | Maintain exact byte limits, warning semantics and failure precedence; bound total retained data without silently dropping evidence. |
| Changed-plan overhead | Three bounded Git queries plus executable/cwd checks. | Preserve configuration hardening and exact rename/subproject handling. A new Git library is not automatically behavior-compatible. |

**Future performance acceptance proposal, not an approved numeric target:**
use identical fixture trees, child executables, output volumes, pinned tools,
resource ceilings and cache states. Compare released binaries, not Rust debug
builds. Separately measure read-only plan startup, changed-plan scaling, bounded
synthetic orchestration, representative end-to-end gates and idle watch cost.
Define samples, warmups, order, wall-time statistics, CPU/RSS and numeric budgets
before running the experiment. Acceptance requires the exact same selected
checks/outcomes, a pre-agreed measurable improvement in the targeted workload,
and no regression beyond pre-agreed tolerances in the other workloads. Without
those recorded thresholds/results, the claim "faster" is **unverified**, even if
all correctness tests pass. Actual numeric thresholds need owner approval in
the later implementation/measurement task, not an invented promise here.

## Rust acceptance requirements

These are observable acceptance cases, not a build sequence. Proposed future
Cargo commands below are requirements for a future project; they do not exist
as runnable repository checks today. Known defect cases F1-F3 are deliberate
corrections to the Python oracle, not parity exceptions to hide in snapshots.

| ID | Required test/action | Binary acceptance signal |
| --- | --- | --- |
| R1: CLI and reports | Run every profile/flag combination above against synthetic projects, including invalid combinations, invalid manifests and interrupt. Validate JSON and exit status together. | Expected 0/1/2/130 outcomes; no child stdout corrupts JSON; one complete object per watch report; ready is never reported as passed. |
| R2: Schema parity | Replay all current validation/default/migration fixtures; include booleans, fractions, NaN/infinity, unknown fields, NULs, duplicate names, empty args and warning-policy precedence. | Existing valid v1/v2 behavior preserved, invalid data rejected deterministically; F2 yields one blocked JSON object, exit 1, no panic/traceback. Specify duplicate JSON-key/depth handling rather than depending accidentally on a parser. |
| R3: Argv/cwd | Real child prints received arguments and cwd; include spaces, empty args and literal shell metacharacters. Test missing executables, nonexecutable files, traversal and symlink escapes. | Exact arguments; no implicit shell expansion; valid relative requirements resolve from cwd; escapes never start a child. |
| R4: Status precedence | Cross-product of child exits, warning policies, expected counts, missing tools, blocker-like text, timeout and overflow. Include scanner errors/missing report. | Nonzero never becomes success; failure dominates blocker; timeout and overflow remain failed at both run-check and baseline layers, including F3. |
| R5: Capture bounds | Real children emit exactly 4 MiB and 4 MiB + 1, mixed stdout/stderr, invalid UTF-8, or close output and keep running. | Boundary output retained; overflow fails with truncation flag; replacement decoding stable; deadline still enforced; following check runs. |
| R6: Lifecycle | Real child/grandchild fixtures for timeout, pipe inheritance, leader exit, SIGINT and SIGTERM; use bounded cleanup even if an assertion fails. | No surviving owned in-group descendants, no double signaling of an old group, no concurrent watch reruns; escape limitations explicit. Define SIGTERM's public exit/report contract before implementation. |
| R7: Coverage | Real scanner replay plus malformed/missing/empty report fixtures, known SQL advisory, unknown rule, parser error and non-Python project. | No false baseline coverage/pass; exact finding policy; aggregate behavior explicitly preserved or versioned. Do not call a Python-only rule pack Rust coverage. |
| R8: Git safety | Real temporary Git repos with hostile fsmonitor/filters, inherited overrides, nested paths/newlines, renames, ignored files, submodules and linked worktrees; malformed records and non-UTF-8 names. | No fixture hook/filter marker created; bounded queries; correct project-relative annotations; unsupported data blocks; no check is skipped. |
| R9: Watch | Real CLI fixture plus deterministic clock tests: edit bursts, edits during execution, initial/between-run invalid manifests, ignored/generated files and F4. | Required reruns occur once per settled batch, invalid manifest recovers, runs never overlap; metadata-preserving-edit promise and test expectation explicit. |
| R10: Safe migration | Preview, unchanged valid v1, required v1-to-v2 upgrade, malformed inputs, read-only directory, pre-existing temp symlink, interrupted/concurrent write. | Preview changes nothing; strict-warning semantics retained; F1 sibling unchanged; failure leaves a valid original manifest and no unsafe leftover; successful output validates. |
| R11: Documentation | Execute existing renderer boundary fixtures, including read-only plans, collision names, changed dependency, invalid late template, symlink/overlap and missing Nix. | Existing generated Markdown and blocked/failure statuses preserved; plan/baseline write nothing; no unnoticed removal of project-docs. Multi-file failure behavior explicitly documented. |
| R12: Bounded resources | Add published limits for manifest bytes/depth, check count, scanner report size and aggregate diagnostics; fixtures at and above each limit. | At-limit accepted inputs work, above-limit inputs return a structured non-success without panic or unbounded allocation. Limits must be reviewed as compatibility changes. |
| R13: First-party memory safety | Require `#![forbid(unsafe_code)]` in every first-party crate target and review the source/dependency inventory. Run future `cargo test --locked`, `cargo fmt --all -- --check`, and `cargo clippy --locked --all-targets -- -D warnings` in the approved pinned environment. | All commands exit 0; no first-party unsafe allowance. If a low-level operation cannot meet this policy, stop for an explicit exception rather than silently weaken it. Dependency/FFI unsafe internals are separately inventoried and reviewed, not covered by the crate attribute. |
| R14: Packaging and consumers | Build/run both Linux targets in authorized environments, preserve package/executable exports and immutable scanner/rule paths, and replay representative consumer manifests with the existing command name. | Native executable smoke and contract suites pass on each claimed platform; no removed renderer/export or implicit installer/network bootstrap. Evaluation alone cannot satisfy runtime portability. |
| R15: Speed | Run the separately authorized comparison described above using recorded numeric thresholds. | Identical coverage/results plus threshold success for targeted speed and allowed regressions. No measurement means this criterion remains unmet, not self-certified. |

Pin and review dependencies, keep the runtime surface small, and prefer typed
status transitions and explicit resource ownership. These are design constraints,
not a mandate for a particular async runtime, parser crate, event watcher or Git
library. Any dependency selection must use its version-matched API contract.
Memory safety must be stated at the correct scope: first-party safe Rust does
not establish memory safety of external check programs or eliminate unsafe code
inside the standard library and transitive dependencies.

For compatibility evaluation, compare parsed report structure, check ordering,
status, return codes, warning counts, coverage and filesystem/process effects.
Normalize only explicitly unstable fields such as duration, temporary paths and
pinned store identities. Do not normalize away a changed status, omitted check,
warning or coverage error. Existing Python unit tests import implementation
functions and frequently mock the baseline; they cannot alone prove that a new
Rust executable implements the same public contract.

### Decisions deliberately left for the next authorized task

These do not block this analysis, but they block claims of a completed rewrite:

- Numeric performance targets, representative consumer corpus and measurement
  budget; no target was approved in this task.
- Whether the companion renderer stays as a named Python consumer or is also
  replaced; no promise of a Python-free closure is justified yet.
- Compatibility policy for resource limits, duplicate JSON keys, reserved names,
  raw-signal behavior and complete-vs-metadata watch detection.
- Rust/MSRV/crate pins and any separately justified low-level unsafe exception;
  no new dependency is selected here.
- Whether non-Linux platforms, parallel checks, result caching or a new report
  schema are wanted at all. None is necessary to preserve today's contract.

## Verification record and limitations

Completed analysis evidence:

- `project-check --help`: exit 0; installed CLI exposes the documented options.
- `project-check fast --plan --json`: exit 0, `ready`, selecting baseline and
  `project-check-fast`. This confirms prerequisites, not passing checks or exact
  identity between installed runner and checkout source.
- `agent-security-scan --offline --timeout 120 source .`: exit 0, complete scan
  report, zero findings; engine summary reports 7 rules across 10 source targets
  and 5 files skipped by ignore patterns. This is a scoped pattern scan, not a
  whole-repository security proof. It did not detect F1-F3. An initial PTY launch
  failed with command-not-found (127); the normal noninteractive tool environment
  resolved and ran the same approved command without installation.
- A temporary `agent-toolbox python3 -B` probe script: exit 0; F1-F4 reproduced and
  the aggregation clarification confirmed. Real temporary filesystem operations
  exercised F1/F2/F4; F3 used a real synthetic scanner child and a reduced output
  cap; the aggregation probe mocked only the baseline result. Source and existing
  tests were not modified. Fixtures are disposable, not shipped tests.
- Complete runtime and test-source review: existing tests cover real subprocess
  execution, output limits and real Git fixtures; scanner parsing and much of
  watch scheduling are mocked. Tests for the newly identified defects are not
  present in the reviewed suite.

- `agent-toolbox project-check fast`: exit 0, `passed`, run in the foreground
  through the installed bounded toolbox. The installed Python baseline reported
  zero blocking findings, zero SQL advisories and zero parser errors. The gate
  checked Nix formatting, evaluated both Linux output sets without building the
  optional rule replay, and built the native Python unit-test derivation.
  Its build log confirmed Ruff lint/format success and **56 passing tests**.
  No production source, test or dependency changed after that verification.
- `nix log` for the exact native unit-check derivation produced by the gate:
  exit 0; used to verify the test count and lint/format outcomes rather than
  infer them from the aggregate runner status.

The outer `project-check fast` also executes the installed portable baseline.
This differs from invoking `bash scripts/check fast` alone, which does not run
a real baseline scanner. Documentation-only wording corrections do not alter
the verified runtime inputs; there is no separate Markdown gate declared here.

Not performed: Rust implementation/build, performance benchmarks, ARM64 runtime
execution, optional full portable-rule fixture replay, cross-repository consumer
audit, adversarial process escape/race stress testing, installation or host
activation. No claim of zero defects, complete security or a measured speedup is
made. The output is an evidence-backed contract and acceptance document for the
next, separately authorized implementation task.
