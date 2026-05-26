[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/madmax983/maxwells-daemon)

# Maxwell's Daemon

Maxwell's Daemon is a minimal harness for operators who want to own the SWE
agent loop: start with a small bash-first agent, swap MCP toolsets at invocation
time, produce inspectable trajectories, and measure each change before adding
more machinery.

## Getting Started

### Prerequisites

- Rust toolchain 1.85 or newer, matching the crate `rust-version`.
- Git on `PATH`.
- Optional: Docker, only when running isolated environments with a binary built
  with the `docker` feature.
- Optional for live-model runs only: the provider credential expected by
  LiteLLM-style routing, such as `ANTHROPIC_API_KEY` for `claude*` models or
  `OPENAI_API_KEY` for OpenAI-routed models.

### 1. Run The No-Key Smoke Path

This path costs $0 and performs no network model call. The `hello-world`
command uses a scripted deterministic model, runs one local shell command, then
writes a canonical trajectory and final-output artifact.

PowerShell:

<!-- quickstart-smoke-command -->
```powershell
cargo run --quiet -- --log error hello-world --output runs/quickstart
```

macOS/Linux:

<!-- quickstart-smoke-command -->
```bash
cargo run --quiet -- --log error hello-world --output runs/quickstart
```

Expected stdout:

<!-- quickstart-smoke-output -->
```text
hello-world smoke complete
trajectory: runs/quickstart/hello-world.traj.json
output: runs/quickstart/hello-world.output.txt
```

The trajectory at `runs/quickstart/hello-world.traj.json` should parse as
`mini-swe-agent-1.1`, have `outcome: "submitted"`, and record
`total_cost_usd: 0.0`.

### 2. Preview Your Agent Environment (Zero Cost)

Use `agent env preview` to inspect the runtime environment that would be used
for a task — filesystem paths, hooks, MCP servers, sensitive env vars
(redacted), and policy rules — **without running the agent or calling a
model**. Exit 13 (`env_preview_warning`) signals risky findings; exit 0 means
a clean preview.

```bash
cargo run --quiet -- --log error agent env preview --env docker --task "fix the bug in src/lib.rs"
```

> **Note:** `--env local` always exits 13 because `LocalEnvironment` does not
> confine bash commands to the configured workdir (full host filesystem access
> is always a risky finding). Use `--env docker` for a CI gate that can exit 0.

For JSON output suitable for CI snapshot diffing or automated gates:

```bash
cargo run --quiet -- --log error agent env preview \
  --env docker --task "fix the bug" --format json
```

Gate CI on a clean preview before launching a docker sweep (the config must
set `environment.kind = "docker"` and supply a `docker_image` so the preview
exits 0 instead of 13):

```toml
# config.toml — minimal docker config for a clean env preview
[environment]
kind   = "docker"
workdir = "/workspace"
docker_image = "ubuntu:22.04"
```

```bash
cargo run --quiet --features docker -- --log error agent env preview \
  --env docker --task "fix the bug" --config config.toml
preview_exit=$?
if [ $preview_exit -eq 13 ]; then
  echo "WARNING: risky env findings — review output before proceeding"
  exit 1
fi
```

See [`docs/spec-env-preview.md`](docs/spec-env-preview.md) for the full JSON
schema, field descriptions, and risky-finding trigger table.

### 4. Preview Your Prompt Before Any Paid Call

Use `--render-only` to see the exact system message, user message, registered
tools, and an estimated token count — at $0 with zero network calls. This is
the recommended first step when iterating on prompts, configs, or hooks:

```bash
cargo run --quiet -- --log error mini --render-only --task "fix the bug in src/lib.rs" --model claude-opus-4-7
```

Add `--format json` for a stable, schema-versioned object suitable for CI
snapshot diffing:

```bash
cargo run --quiet -- --log error mini --render-only --task "fix the bug" --model claude-opus-4-7 --format json
```

Use `--read-only` to force analysis-only behavior during real runs. In this
mode, tool execution (including built-in `bash`) is blocked and any attempted
tool action terminates the run with `failure_category: read_only_violation`.
`--read-only` is incompatible with PR-publish flags and invocation-time MCP
servers unless `--allow-mcp-in-read-only` is explicitly set.

To avoid complex shell escaping when passing multi-line prompt markdown or special characters, you can load the task from a file or standard input using `--task-file`:

```bash
# Load from a file
cargo run --quiet -- --log error mini --render-only --task-file prompts/my-complex-task.md --model claude-opus-4-7

# Read from stdin
echo "Fix the bug in src/lib.rs
Make sure all quotes like \"this\" and backticks like \`this\` are preserved." | cargo run --quiet -- --log error mini --render-only --task-file - --model claude-opus-4-7
```

### 5. Inspect The Trajectory

PowerShell:

```powershell
cargo run --quiet -- --log error bench inspect --sweep runs/quickstart --instance hello-world
```

macOS/Linux:

```bash
cargo run --quiet -- --log error bench inspect --sweep runs/quickstart --instance hello-world
```

This is the core operator loop before any sweep: run one task, inspect the
trajectory, then decide whether the model, prompt, budget, and environment are
ready for a broader run.

Export the same trajectory as shareable Markdown in one command:

```bash
cargo run --quiet -- --log error bench inspect --sweep runs/quickstart --instance hello-world --format markdown --output traj.md
```

### 6. Optional Preflight Before SWE-bench

Use `bench dataset-stats` to preview the composition, token distributions (computed completely offline using `litellm-rs`'s `TokenCounter`), expected tests count, languages present, representative slice skewness (warns if unique repos < 50% or median token length difference > 25% compared to the full dataset), and historical resolved rates matching current dataset hash before running a sweep:

```powershell
cargo run --quiet -- --log error bench dataset-stats --dataset lite --split test --sample 10 --seed 42
```

Use `doctor` on a local SWE-bench JSONL dataset before launching work. This
checks the dataset and environment setup; `--skip-model-probe` keeps this
preflight from touching a model provider.

PowerShell:

```powershell
cargo run --quiet -- --log error bench doctor --dataset-path .\data\swebench.jsonl --output runs\doctor --limit 1 --skip-model-probe
```

macOS/Linux:

```bash
cargo run --quiet -- --log error bench doctor --dataset-path ./data/swebench.jsonl --output runs/doctor --limit 1 --skip-model-probe
```

After credentials are set and you are ready to spend a small calibration
budget, run `forecast` before a full sweep:

```bash
cargo run --quiet -- --log info bench forecast --dataset-path ./data/swebench.jsonl --output runs/forecast --limit 5 --calibration-n 2 --sweep-cost-limit-usd 1.00 --format json > runs/forecast.json
```

### 7. Close The Calibration Loop

For paid sweeps, treat the operator loop as `doctor` -> `forecast` ->
`swebench` -> `calibrate`. The forecast keeps the first spend bounded; the
calibration report tells you whether that forecast was trustworthy after the
real sweep completes.

```bash
cargo run --quiet -- --log error bench doctor --dataset-path ./data/swebench.jsonl --output runs/doctor --limit 5 --skip-model-probe
cargo run --quiet -- --log info bench forecast --dataset-path ./data/swebench.jsonl --output runs/forecast --limit 5 --calibration-n 2 --sweep-cost-limit-usd 1.00 --format json > runs/forecast.json
cargo run --quiet -- --log info bench swebench --dataset-path ./data/swebench.jsonl --output runs/sweep --limit 5 --sweep-cost-limit-usd 1.00
cargo run --quiet -- --log error bench calibrate --forecast runs/forecast.json --results runs/sweep/results.json --output runs/sweep/calibration.json --fail-on-optimistic
```

`bench calibrate` prints a compact summary, writes a versioned
`calibration_report`, classifies each metric as `within_interval`,
`over_upper`, or `under_lower`, and exits with `calibration_optimistic` when
`--fail-on-optimistic` is set and actuals overshot the forecast. The budget
seance gets a receipt.

## Live-Model Quickstart

Keep the paid path separate from the no-key smoke path. Set the credential for
the model family you choose, keep the task local and tiny, and set both a step
limit and a per-task budget. Local execution runs model-generated shell commands
from this checkout, so treat it as trusted-code execution. **For any `--env
local` run, pass `--interactive` (issue #312):** the agent pauses before every
bash action and asks the operator to approve, reject, or abort — closing the
"one hallucinated `rm -rf` away from a wiped checkout" gap that Docker isolation
otherwise covers. See [`docs/spec-interactive-mode.md`](docs/spec-interactive-mode.md)
for the full contract; use `--interactive --ui ratatui` for a full-screen
dashboard, or `--yolo` to run unattended with a per-step status line.

PowerShell:

```powershell
$env:ANTHROPIC_API_KEY = "<your Anthropic key>"
cargo run --quiet -- --log info mini --interactive --task "Create runs/live-task/hello.txt containing hello from maxwells-daemon." --model claude-opus-4-7 --env local --output runs/live-quickstart --trajectory-name live-hello --step-limit 8 --task-timeout-secs 300 --per-task-budget-usd 0.25
cargo run --quiet -- --log error bench inspect --sweep runs/live-quickstart --instance live-hello
```

macOS/Linux:

```bash
export ANTHROPIC_API_KEY="<your Anthropic key>"
cargo run --quiet -- --log info mini --interactive --task "Create runs/live-task/hello.txt containing hello from maxwells-daemon." --model claude-opus-4-7 --env local --output runs/live-quickstart --trajectory-name live-hello --step-limit 8 --task-timeout-secs 300 --per-task-budget-usd 0.25
cargo run --quiet -- --log error bench inspect --sweep runs/live-quickstart --instance live-hello
```

If a sweep is interrupted (node preemption, OOM, Ctrl-C), re-run the same
`bench swebench` command with `--resume` added. Trajectories already marked
complete on disk are skipped entirely. Trajectories that were mid-run at the
time of interruption are persisted as partial checkpoints (`partial: true` in
the trajectory `info` block) and will continue from the last completed turn
rather than restarting from step 0, saving both API budget and wall-clock time.
`bench tail` shows a `Partial:` line when stale partial checkpoints are present
on disk, and the final `results.json` records a `partial` count for accounting.
See [`docs/spec-checkpointing.md`](docs/spec-checkpointing.md) for the full
specification and implementation details.

At each bash action the prompt prints the proposed command, the current step,
cumulative cost, and the cache marker, then reads one keystroke: `y` approves,
`n` rejects (the model receives a synthetic `Exit code: 1` observation and may
revise), `a` (or Esc/Ctrl-C) aborts the run cleanly. Rejections and aborts are
recorded on the trajectory as structured events so `bench inspect` can show
exactly which commands the operator vetoed.

### Live Event Streaming

Two transports let you observe per-step events without waiting for the trajectory file:

- **SSE (`--stream <host:port>`)** — the agent binds an HTTP server; clients dial in.
  Good for an interactive local session where you can `curl` or open a browser.
- **Webhook (`--webhook-url <url>`)** — the agent POSTs each event to your listener.
  Good for headless CI, Docker, or any environment that cannot expose an inbound port.
- **Event log (`--event-log <path>`)** — append one redacted JSON event per line.
  Good for `tail -f | jq` workflows and CI artifact collection.

Both can be active at once:

```bash
max mini --task "…" \
  --stream 127.0.0.1:7878 \
  --webhook-url https://hooks.example.com/agent-events \
  --webhook-header "Authorization: Bearer $TOKEN"
```

Each webhook POST body is a versioned JSON envelope
(`{ "schema_version": {"major":1,"minor":0}, "run_id": "…", "event": {…}, "emitted_at": "…" }`).
Secrets are redacted before POST. HTTP failures are logged and counted; they never
block or abort the run. See [`docs/spec-streaming.md`](docs/spec-streaming.md) for the
full transport comparison and envelope schema.

`--event-log` example:

```bash
max mini --task "…" --event-log runs/events.jsonl
tail -f runs/events.jsonl | jq -c '{event_type, instance_id}'
```

For a real SWE-bench sweep, run `bench doctor` first, then `bench forecast`
with a cost cap, then `bench swebench` only after the forecast clears your
budget, and finally `bench calibrate` against the completed `results.json`.
This avoids beginning with a multi-instance spendfest and leaves a durable
calibration record. The built-in
[systemic-failure circuit breaker](docs/spec-systemic-halt.md) halts the sweep
early if the first N instances all fail with the same operator-actionable cause
(bad API key, broken Docker daemon), so a misconfigured run costs cents to abort
instead of dollars to ride out. Tiny mercy.

## Troubleshooting

| Symptom | Likely Cause | Fix |
| --- | --- | --- |
| `cargo` is not recognized or `rustc` is too old | Missing Rust or a toolchain older than 1.85 | Install/update Rust with rustup, then run `rustc --version` |
| `git` is not recognized | Git is missing from `PATH` | Install Git and open a new shell |
| Live run fails with missing credentials | Provider API key is not set | Set `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or the provider-specific key before `mini`, `forecast`, or `swebench` |
| Docker run fails before the agent starts | Docker is unavailable or the binary lacks the `docker` feature | Start Docker, or use `--env local`; build with the Docker feature before selecting `--env docker` |
| Smoke run cannot write artifacts | Output directory is unwritable | Choose a writable `--output` path, for example `runs/quickstart` inside the repo |
| `bench doctor` reports dataset read/parse errors | The `--dataset-path` value is missing, points at a directory, or is not JSONL | Pass a readable SWE-bench JSONL file and rerun `bench doctor --skip-model-probe` |

## Advanced Specs

Start with the first-run path above, then use these deeper specs once you have
a valid trajectory in hand:

- [`mini --resume`](docs/spec-mini-resume.md): continue an interrupted
  single-task run from its persisted checkpoint — no token replay, prefix
  trusted verbatim, resume history recorded in the trajectory manifest.
- [`configuration reference`](docs/config-reference.md): every config field,
  default value, valid values, precedence rules, copy-pasteable TOML examples,
  and secret handling guidance. Start here before tuning a sweep.
- [`cli task file input`](docs/spec-mini-task-file.md): specification for `--task-file` flag semantics, mutual exclusion, exit codes, and standard input streaming.
- [`bench tail`](docs/spec-tail.md): live aggregate progress, cost burn, ETA,
  and failure mix for running SWE-bench sweeps.
- [`bench watch`](docs/spec-watch.md): attach to a single in-flight instance
  and stream its turns live; redaction-safe, NDJSON-pipeable.
- [`bench evaluator-selftest`](docs/spec-evaluator-selftest.md): zero-cost
  preflight that verifies the evaluator pipeline against gold patches before
  launching a paid sweep. Run after switching dataset, evaluator image, or machine.
- [`bench evaluate`](docs/spec-evaluation.md): evaluator output, rerun metrics,
  pass@k, and compare regression gates.
- [`bench triage`](docs/spec-triage.md): deterministic unresolved-failure
  clustering, ranked stdout tables, and the `triage.json` schema.
- [`bench triage-diff`](docs/spec-triage-diff.md): diff failure-cluster composition between two sweeps.
- [`bench command-stats`](docs/spec-command-stats.md): shell-command frequency
  and cost aggregated by outcome bucket, delta view for resolved-vs-unresolved
  comparison, and the `command-stats.json` schema.
- [`bench policy-impact`](docs/spec-policy-impact.md): measure security policy impact on sweep outcomes.
- [`bench grep`](docs/spec-grep.md): regex search across all trajectory messages
  in a sweep — filter by role, instance, or outcome; redaction-safe; zero-cost
  (reads only on-disk artifacts).
- [`bench matrix`](docs/spec-matrix.md): multi-arm experiment runner — compare
  models or configs against the same instance set, shared budget enforcement,
  `matrix.json` state, ranked `matrix-summary.json`, and `--resume` support.
- [`agent scriptability`](docs/spec-scriptability.md): invocation-time MCP
  servers plus `PreToolUse` and `PostToolUse` hooks for A/B testing agent
  toolsets without rebuilding Rust.
- [`streaming`](docs/spec-streaming.md): SSE and webhook event surfaces for
  observing runs while they execute.
- [`secret redaction`](docs/spec-secret-redaction.md): redaction guarantees for
  trajectories, inspect output, streams, and patch artifacts.
- [`bench bundle`](docs/spec-bundle.md): deterministic, redaction-strict
  `tar.gz` sweep archives with hash verification.
- [`bench reproduce`](docs/spec-reproduce.md): replay a saved sweep from its
  manifest, detect environment drift, and write a `reproducibility.json`
  comparison artifact.
- [`systemic-failure circuit breaker`](docs/spec-systemic-halt.md): halt
  sweeps early when all instances fail with the same operator-actionable cause;
  exit code 11, `halt-report.json` artifact, actionable-category whitelist, and
  `bench reproduce` drift handling.
- [`bench report`](docs/spec-report.md): produce a self-contained markdown or
  HTML sweep summary — provenance, top-line metrics, failure mix, and top failed
  instances — ready to drop into a PR, Slack thread, or paper appendix.
- [`bench ladder`](docs/spec-ladder.md): resolved-rate and cost trend across
  sweeps in a root directory — the single command for answering "am I making
  progress?" across a week of prompt iteration. Includes `--baseline`, `--last`,
  `--dataset`, and three output formats (text, json, markdown).
- [`agent skills-preview`](docs/spec-skills-preview.md): zero-cost pre-run
  enumeration of which skill manifests will activate for one or more tasks —
  task hash, activation reason (`explicit_mention` vs `auto_match`), content
  hash, byte cost, cap-hit status, and a per-corpus summary. Run before a
  paid sweep to audit prompt injection, detect stale manifests, and spot
  budget surprises.
- [`bench power`](docs/spec-power.md): offline statistical power, required sample
  size per arm, or Minimum Detectable Effect (MDE delta) calculations for
  two-proportion z-tests.
- [`bench dataset-stats`](docs/spec-dataset-stats.md): zero-cost preflight to
  preview and analyze dataset composition offline before running a sweep.
- [`bench eval-flake`](docs/spec-eval-flake.md): quantify evaluator-side verdict
  noise by replaying the evaluator N times per patch on a completed sweep.
  Produces `eval-flake.json`; pair with `bench compare --flake-report` to
  exclude flaky instances from significance testing and get honest published deltas.
- [`bench stagnation-report`](docs/spec-stagnation-report.md): post-hoc
  cross-sweep aggregation of in-loop stagnation halts — clustered by canonical
  loop fingerprint, ranked by cost burned, with a conservative USD-saved estimate.
  Zero-cost (reads only on-disk artifacts); JSON output suitable for CI snapshot
  diffing to prove prompt changes reduce recurring loops.

## Nightly E2E smoke

`.github/workflows/swe-bench-nightly.yml` runs a single SWE-bench Lite
instance through the full `bench swebench` pipeline against an OpenRouter
free-tier model (`openrouter/deepseek/deepseek-chat-v3.1:free`). It exists
to catch harness regressions, not to track solve rate — the run passes
whenever the sweep reports `errored == 0` in `results.json`. Trajectories
and the input dataset are uploaded as artifacts on every run; scheduled
failures auto-open a `nightly-smoke` issue. Requires the
`OPENROUTER_API_KEY` repository secret.
