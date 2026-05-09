[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/madmax983/rust_swe_agent)

# rust_swe_agent

A minimal harness for operators who want to own the SWE agent loop: start with
a small bash-only agent, produce inspectable trajectories, and measure each
change before adding more machinery.

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

### 2. Inspect The Trajectory

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

### 3. Optional Preflight Before SWE-bench

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
cargo run --quiet -- --log info bench forecast --dataset-path ./data/swebench.jsonl --output runs/forecast --limit 5 --calibration-n 2 --sweep-cost-limit-usd 1.00
```

## Live-Model Quickstart

Keep the paid path separate from the no-key smoke path. Set the credential for
the model family you choose, keep the task local and tiny, and set both a step
limit and a per-task budget. Local execution runs model-generated shell commands
from this checkout, so treat it as trusted-code execution. Use Docker isolation
when available for untrusted repositories or tasks; see
[`docs/spec-interactive-mode.md`](docs/spec-interactive-mode.md) for the local
execution safety rationale.

PowerShell:

```powershell
$env:ANTHROPIC_API_KEY = "<your Anthropic key>"
cargo run --quiet -- --log info mini --task "Create runs/live-task/hello.txt containing hello from rust_swe_agent." --model claude-opus-4-7 --env local --output runs/live-quickstart --trajectory-name live-hello --step-limit 8 --task-timeout-secs 300 --per-task-budget-usd 0.25
cargo run --quiet -- --log error bench inspect --sweep runs/live-quickstart --instance live-hello
```

macOS/Linux:

```bash
export ANTHROPIC_API_KEY="<your Anthropic key>"
cargo run --quiet -- --log info mini --task "Create runs/live-task/hello.txt containing hello from rust_swe_agent." --model claude-opus-4-7 --env local --output runs/live-quickstart --trajectory-name live-hello --step-limit 8 --task-timeout-secs 300 --per-task-budget-usd 0.25
cargo run --quiet -- --log error bench inspect --sweep runs/live-quickstart --instance live-hello
```

For a real SWE-bench sweep, run `bench doctor` first, then `bench forecast`
with a cost cap, then `bench swebench` only after the forecast clears your
budget. This avoids beginning with a multi-instance spendfest. Tiny mercy.

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

- [`configuration reference`](docs/config-reference.md): every config field,
  default value, valid values, precedence rules, copy-pasteable TOML examples,
  and secret handling guidance. Start here before tuning a sweep.
- [`bench tail`](docs/spec-tail.md): live aggregate progress, cost burn, ETA,
  and failure mix for running SWE-bench sweeps.
- [`bench evaluate`](docs/spec-evaluation.md): evaluator output, rerun metrics,
  pass@k, and compare regression gates.
- [`agent scriptability`](docs/spec-scriptability.md): config-defined
  `PreToolUse` and `PostToolUse` hooks for extending the tiny agent loop
  without new Rust tools.
- [`streaming`](docs/spec-streaming.md): SSE and webhook event surfaces for
  observing runs while they execute.
- [`secret redaction`](docs/spec-secret-redaction.md): redaction guarantees for
  trajectories, inspect output, streams, and patch artifacts.

## Nightly E2E smoke

`.github/workflows/swe-bench-nightly.yml` runs a single SWE-bench Lite
instance through the full `bench swebench` pipeline against an OpenRouter
free-tier model (`openrouter/deepseek/deepseek-chat-v3.1:free`). It exists
to catch harness regressions, not to track solve rate — the run passes
whenever the sweep reports `errored == 0` in `results.json`. Trajectories
and the input dataset are uploaded as artifacts on every run; scheduled
failures auto-open a `nightly-smoke` issue. Requires the
`OPENROUTER_API_KEY` repository secret.
