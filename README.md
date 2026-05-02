# rust_swe_agent

A minimal Rust port of mini-SWE-agent with local trajectories, SWE-bench sweep
helpers, evaluation summaries, inspection tools, and cost controls.

## Getting Started

### Prerequisites

- Rust 1.85 or newer with Cargo. The crate uses Rust edition 2024.
- Git, because live benchmark tasks and patch capture operate on checkouts.
- Docker is optional. Local runs work without it; Docker execution requires a
  binary built with the `docker` feature and a usable Docker daemon.
- Model credentials are needed only for live-model runs, not for the smoke path.
  For the default model, provide `ANTHROPIC_API_KEY` in the environment before
  running `mini` or a real `bench swebench` sweep.

### 1. Run the no-key smoke path

This command costs $0 and performs no network model call. It uses the scripted
deterministic model, runs one local `echo hello` action, submits `ok`, and writes
a valid trajectory artifact.

Windows PowerShell:

```powershell
cargo run -- hello-world --output .\runs\quickstart
Test-Path .\runs\quickstart\hello-world.traj.json
```

macOS/Linux:

```sh
cargo run -- hello-world --output ./runs/quickstart
test -f ./runs/quickstart/hello-world.traj.json
```

Expected success output:

```text
hello-world smoke succeeded
trajectory: ./runs/quickstart/hello-world.traj.json
final_output: ok
total_cost_usd: 0.0000
```

Expected artifact path: `runs/quickstart/hello-world.traj.json`.

### 2. Inspect the trajectory

Use `bench inspect` next. This is the core loop: run one task, inspect the
trajectory, then decide whether a larger sweep is worth running. Tiny ritual,
fewer cursed surprises.

Windows PowerShell:

```powershell
cargo run -- bench inspect --sweep .\runs\quickstart --instance hello-world
```

macOS/Linux:

```sh
cargo run -- bench inspect --sweep ./runs/quickstart --instance hello-world
```

The inspect view should show `outcome: submitted`, `final_output: ok`, and
`total_cost_usd: 0.0000`.

### 3. Optional preflight

Before a SWE-bench sweep, run `doctor` against the dataset path you plan to use.
Use `forecast` before spending real model budget.

```sh
cargo run -- bench doctor --dataset-path ./path/to/swebench.jsonl --output ./runs/doctor --limit 1 --skip-model-probe
cargo run -- bench forecast --dataset-path ./path/to/swebench.jsonl --output ./runs/forecast --sample 5 --seed 42 --sweep-cost-limit-usd 5.00 --skip-model-probe
```

On Windows PowerShell, use the same arguments and PowerShell-style paths if you
prefer, for example `.\path\to\swebench.jsonl`.

### Live-model local run

After the no-key path works, try one small local task with an API key. Keep
`--step-limit` and `--task-timeout-secs` tight while proving the loop, then use
`bench forecast` or `bench swebench --forecast-first --sweep-cost-limit-usd <USD>`
before any paid sweep.

Windows PowerShell:

```powershell
$env:ANTHROPIC_API_KEY = "<your key>"
cargo run -- mini --task "Create a short NOTE.txt saying hello from rust_swe_agent" --model claude-opus-4-7 --env local --step-limit 3 --task-timeout-secs 120 --output .\runs\live-local --trajectory-name live-local-smoke
cargo run -- bench inspect --sweep .\runs\live-local --instance live-local-smoke
```

macOS/Linux:

```sh
export ANTHROPIC_API_KEY="<your key>"
cargo run -- mini --task "Create a short NOTE.txt saying hello from rust_swe_agent" --model claude-opus-4-7 --env local --step-limit 3 --task-timeout-secs 120 --output ./runs/live-local --trajectory-name live-local-smoke
cargo run -- bench inspect --sweep ./runs/live-local --instance live-local-smoke
```

Local execution can modify files visible from the current working directory.
Use Docker execution for stronger isolation when that feature is available:
`--env docker --docker-image <image>`. If Docker is unavailable, stay local but
run in a disposable checkout.

### Troubleshooting

| Symptom | What to check |
| --- | --- |
| Missing Rust | Install Rust 1.85+ with `rustup`, then rerun `cargo --version`. |
| Missing Git | Install Git and confirm `git --version` works in the same shell. |
| Missing model credential | The no-key smoke path does not need a key; live runs need `ANTHROPIC_API_KEY` for the default model. |
| Docker unavailable | Omit `--env docker`, or start Docker and rebuild with `--features docker`. |
| Unwritable output directory | Choose an output path you can create, for example `./runs/quickstart`. |
| Malformed dataset path | Pass a readable JSONL file to `--dataset-path`; use quotes around paths with spaces. |

## Advanced Specs

Start with the first-run path above. Once that works, these specs document the
larger benchmark and inspection surfaces:

- [`bench tail`](docs/spec-tail.md): live aggregate progress, cost burn, ETA,
  and failure mix for running SWE-bench sweeps.
- [`bench evaluate`](docs/spec-evaluation.md): evaluator output, rerun metrics,
  pass@k, and compare regression gates.
- [`bench inspect`](docs/spec-inspect.md): trajectory inspection and diff views.
- [`streaming`](docs/spec-streaming.md): SSE and webhook-style run events.
- [`GitHub integration`](docs/spec-github-integration.md): posting and tracking
  agent patches through GitHub workflows.
