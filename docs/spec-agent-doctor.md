# Spec: `agent doctor` (Issue #526)

`agent doctor` is a **zero-cost host-readiness preflight**. It answers the
simplest go/no-go question a new operator needs before their first *live* run:
*can this machine actually run a task right now, and if not, what do I fix?*

It makes **no model call and no provider network probe — it costs $0** — and
moves first-run setup errors from cryptic mid-run failures to a pre-run gate.
It complements rather than duplicates:

- `bench doctor` — validates *sweep inputs* (datasets, config).
- `agent env preview` — inspects a task's env *config* and flags policy/secret risk.

`agent doctor` answers neither of those; it checks the *host*.

---

## Invocation

```
max agent doctor \
    [--env <local|docker>] \
    [--model <name>] \
    [--config <path-to-config.toml>] \
    [--format <text|json>] \
    [--output <dir>] \
    [--docker-image <ref>]
```

### Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--env` | config kind (default `local`) | Environment type to validate. When omitted, the environment kind from the resolved config (`[environment] kind`) is used, so a docker-backed config is validated as docker rather than silently skipped. The Docker daemon check only runs when `docker` is in effect; for `local` it is reported as `skip`. |
| `--model` | resolved config model | Override the model name used to pick the expected provider credential env var. |
| `--config` | — | Path to a TOML config file (overlays defaults). Used to resolve the model and environment. |
| `--format` | `text` | `text` (human checklist) or `json` (machine-readable). |
| `--output` | `./runs` | Runs/output directory whose write access is checked. |
| `--docker-image` | `environment.docker_image` | Override the Docker image to preflight (requires `--env docker`). Mirrors `max mini --docker-image …` so doctor validates the same image the live run would use. |

---

## Checks

Each check produces one checklist row with a stable `check` id and a
`pass` / `fail` / `skip` status.

| `check` | What it verifies | `skip` when | Remediation on `fail` |
|---------|------------------|-------------|------------------------|
| `git` | `git` is resolvable on `PATH` (by directory inspection — the program is **not** executed). | never | "install git and ensure it is on your PATH" |
| `credential` | The provider credential env var expected for the resolved model is **present** (presence only). `claude*` → `ANTHROPIC_API_KEY`; a `provider/model` prefix → `PROVIDER_API_KEY`; anything else → `OPENAI_API_KEY`. | model is `deterministic` (no credential needed) | "export `<VAR>` before a live run" |
| `docker` | The host can actually run a docker environment — mirroring the run path's `build_docker_env` preflight: (1) the binary was built with the `docker` feature, (2) a docker image is resolved (the `--docker-image` CLI override wins, else `environment.docker_image`), and (3) the daemon is reachable (a `docker version` subprocess — a local probe, not a provider call — bounded by a 5s timeout so a wedged daemon cannot hang the gate). | environment is `local` | "rebuild with --features docker, or use --env local"; "environment.kind=docker requires environment.docker_image…"; "start or install Docker"; or "docker probe timed out after 5s…" |
| `output_dir` | The runs/output directory is writable (creates it if needed, then writes and removes a probe file). | never | "check permissions or pass --output `<dir>`" |
| `toolchain` | The active `rustc` meets the crate `rust-version` (currently 1.85). | `rustc` is not found or its version cannot be parsed (unknowable) | "run rustup update" |

`ready` is `true` iff **no** check failed. **Skipped checks never block
readiness.**

### Credential check is presence-only (redaction)

The credential check reads only whether the env var is set and non-empty
(`std::env::var_os(..).is_some_and(non-empty)`). The value is never bound to a
named variable, never placed in `detail`, and never printed or logged —
consistent with the redaction policy. A regression test asserts that a secret
value passed through the environment never appears in stdout or stderr.

---

## Output

### Text (`--format text`)

One line per check, prefixed with `[pass]` / `[fail]` / `[skip]`, followed by a
final verdict line. Failing rows carry the remediation hint in their detail.

### JSON (`--format json`)

A schema-versioned object suitable for CI gating:

```json
{
  "schema_version": 1,
  "ready": false,
  "checks": [
    { "check": "git",        "status": "pass", "detail": "git found at /usr/bin/git" },
    { "check": "credential", "status": "fail", "detail": "ANTHROPIC_API_KEY is not set; export ANTHROPIC_API_KEY before a live run" },
    { "check": "docker",     "status": "skip", "detail": "docker not required for local environment" },
    { "check": "output_dir", "status": "pass", "detail": "./runs is writable" },
    { "check": "toolchain",  "status": "pass", "detail": "active rustc 1.85.0 meets required rust-version 1.85" }
  ]
}
```

Each `checks[]` entry is exactly `{ check, status, detail }`. `status`
serializes to `pass` / `fail` / `skip`.

---

## Exit codes

| Code | Outcome class | Meaning |
|-----:|---------------|---------|
| 0 | `success` | Every check passed (skips are allowed). |
| 48 | `host_not_ready` | At least one check failed. The full checklist was printed first; the non-zero exit is the go/no-go gate. Distinct from `preflight_failure` (3) so CI can route "host not ready before any run" separately from sweep-time dependency failures. |
| 2 | `usage_error` | Invalid flag or config file. |
| 1 | `internal_error` | Unexpected I/O or serialization failure. |

See `docs/exit-codes.md` for the full contract.

---

## Guarantees

- **$0, no model call, no provider network probe.** The only subprocesses are
  `docker version` (only when `--env docker`) and `rustc --version`; neither is
  a provider call. `git` is resolved by inspecting `PATH`, not by execution.
- **Presence-only credential check.** No secret value is ever read into a
  variable, printed, or logged.
- **Skips never fail the gate.** A check that cannot be determined (e.g. unknown
  toolchain) is reported as `skip` and does not set `host_not_ready`.

---

## CI example

```bash
max agent doctor --env docker --format json > doctor.json || {
  echo "Host not ready:"
  jq -r '.checks[] | select(.status=="fail") | "  - \(.check): \(.detail)"' doctor.json
  exit 1
}
```
