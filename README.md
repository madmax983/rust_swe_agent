[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/madmax983/rust_swe_agent)

# rust_swe_agent
A minimal rust port of mini-swe-agent.

## Specs

- [`bench tail`](docs/spec-tail.md): live aggregate progress, cost burn, ETA,
  and failure mix for running SWE-bench sweeps.
- [`bench evaluate`](docs/spec-evaluation.md): evaluator output, rerun metrics,
  pass@k, and compare regression gates.
- [`agent scriptability`](docs/spec-scriptability.md): config-defined
  `PreToolUse` and `PostToolUse` hooks for extending the tiny agent loop
  without new Rust tools.

## Nightly E2E smoke

`.github/workflows/swe-bench-nightly.yml` runs a single SWE-bench Lite
instance through the full `bench swebench` pipeline against an OpenRouter
free-tier model (`openrouter/deepseek/deepseek-chat-v3.1:free`). It exists
to catch harness regressions, not to track solve rate — the run passes
whenever the sweep reports `errored == 0` in `results.json`. Trajectories
and the input dataset are uploaded as artifacts on every run; scheduled
failures auto-open a `nightly-smoke` issue. Requires the
`OPENROUTER_API_KEY` repository secret.
