# `bench stagnation-report` Specification

## Purpose

Post-hoc, zero-cost aggregation of in-loop stagnation halts across a completed
sweep. Groups halted instances by canonical loop fingerprint so operators can
see which tool/prompt patterns produce recurring loops and verify that prompt
changes actually reduce them.

**Zero-cost**: reads only on-disk trajectory and results artifacts. No model
calls, no network access.

## Usage

```
max bench stagnation-report --sweep <SWEEP_DIR> [--format text|json]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep` | *(required)* | Completed sweep directory produced by `bench swebench`. |
| `--format` | `text` | Output format: `text` (default table) or `json` (schema-versioned). |

## Output

### Text format (default)

Two tables are printed:

**Per-instance table** — ranked by `budget_burned_usd` descending:

| Column | Description |
|--------|-------------|
| `instance_id` | Instance identifier. |
| `fingerprint` | First 8 hex chars of the canonical SHA-256 loop fingerprint. |
| `canonical_action` | Canonicalized action text, truncated to 80 chars + "…" when longer. |
| `K` | Hit count — number of occurrences that triggered the halt. |
| `halt_step` | Step number at which the halt fired. |
| `burned_usd` | USD cost burned before the halt. |
| `saved_est_usd` | Conservative USD-saved estimate (see formula below). |

**Cluster view** — ranked by `instance_count` descending:

| Column | Description |
|--------|-------------|
| `fingerprint` | First 8 hex chars of the canonical fingerprint. |
| `exemplar_action` | Canonical action text from the first instance in the cluster. |
| `instances` | Number of instances in the cluster. |
| `total_burned_usd` | Total USD burned across all instances in the cluster. |
| `exemplar_ids` | Up to 5 exemplar instance IDs. |

### JSON format (`--format json`)

Emits a stable, schema-versioned artifact suitable for CI snapshot diffing:

```json
{
  "artifact_kind": "stagnation_report",
  "schema_version": { "major": 1, "minor": 10 },
  "sweep_path": "/path/to/sweep",
  "instances": [
    {
      "instance_id": "django__django-1234",
      "fingerprint": "a37c5b26",
      "canonical_action": "find . -name '*.py'",
      "hit_count": 4,
      "halt_step": 8,
      "budget_burned_usd": 0.0160,
      "usd_saved_estimate": 0.0840
    }
  ],
  "clusters": [
    {
      "fingerprint": "a37c5b26",
      "exemplar_action": "find . -name '*.py'",
      "instance_count": 3,
      "total_usd_burned": 0.0480,
      "exemplar_instance_ids": ["django__django-1234", "django__django-5678", "sympy__sympy-99"]
    }
  ],
  "totals": {
    "halted_count": 3,
    "total_usd_burned_before_halt": 0.0480,
    "total_usd_saved_estimate": 0.2520
  }
}
```

## USD-saved Estimate Formula

```
usd_saved_estimate = (step_limit − halt_step) × mean_per_step_usd
```

where:

- `halt_step` — step number when the stagnation halt fired (from the trajectory)
- `step_limit` — configured step cap (parsed from the sweep manifest's resolved config)
- `mean_per_step_usd = budget_burned_usd / halt_step`

**Conservative by design**: assumes constant per-step cost equal to the observed
mean across completed steps. The estimate is `null` when `step_limit` cannot be
determined from the sweep manifest (e.g., sweeps without a `results.json`
manifest).

## Canonical Fingerprint

The fingerprint is derived from the in-loop stagnation detector (see
[`spec-stagnation.md`](spec-stagnation.md)) and is already stored in each
trajectory's `info.other["stagnation"].action_hash` field. The command reads
this stored value directly — no re-hashing of trajectory data.

The canonicalization rule (applied by the detector at run time):
1. Trim leading/trailing ASCII whitespace.
2. Collapse internal whitespace runs to a single space.
3. Strip a single trailing semicolon (if present).

The canonical form is SHA-256 hashed; the first 16 bytes (32 hex chars) form
the full fingerprint. The report displays the first 8 hex chars for readability.

## Redaction

Secret-redaction guarantees from `spec-secret-redaction.md` apply to the
canonical action text in both table and JSON outputs. Structured secrets and
operator-configured literal patterns are replaced before display.

## Exit Codes

| Code | Condition |
|------|-----------|
| 0 | Success (any number of stagnation halts, including zero). |
| 1 | I/O or internal error (unreadable sweep directory, malformed artifact). |
| 2 | Usage error (e.g., `--format` with unknown value; missing `--sweep` is caught by the CLI parser). |

Exit codes follow the project's `outcome_class` convention (see
`docs/exit-codes.md`). **No new exit code is introduced.**

## Behavior Details

- Trajectories without a stagnation halt are silently skipped.
- Sweeps with zero stagnation halts produce an empty but well-formed report and
  exit 0.
- Instances appear in the per-instance table only if the trajectory file can be
  located and parsed. Unresolvable trajectories are silently skipped (the
  instance is still counted in `results.json` totals but not in the report).
- The cluster table shows at most 5 exemplar instance IDs per cluster.
- Instances in a cluster are ordered by `budget_burned_usd` descending, so
  exemplar IDs represent the costliest instances.

## Examples

**Identify the top-3 loop signatures in a 300-instance sweep:**

```bash
max bench stagnation-report --sweep /sweeps/2026-05-20 | head -60
```

**Compare before/after a prompt change in CI:**

```bash
max bench stagnation-report --sweep /sweeps/before --format json > before.json
max bench stagnation-report --sweep /sweeps/after  --format json > after.json
diff before.json after.json
```

**Show only the cluster summary (pipe-friendly):**

```bash
max bench stagnation-report --sweep /sweeps/run-42 --format json \
  | jq '.clusters[] | {fingerprint, instance_count, exemplar_action}'
```

## Relationship to Other Commands

| Command | Relationship |
|---------|-------------|
| `bench triage` | Clusters all failure categories; stagnation-report is stagnation-specific with cost/savings accounting. |
| `bench ladder` | Shows resolved-rate trend; stagnation-report surfaces the loop-pattern cause of halts. |
| `bench retry` (#170) | Re-runs halted instances; stagnation-report identifies *which* loops to fix first. |
| `bench grep` | Searches trajectory content by regex; stagnation-report aggregates the pre-computed fingerprints. |

## Out of Scope

- Modifying the stagnation detector (K, W, fingerprint algorithm) — this
  command is read-only over existing artifacts.
- Cross-sweep trend analysis (compose with `bench ladder` #270).
- Auto-suggesting prompt fixes from loop signatures.
- Live in-flight halt reporting (covered by `bench tail` / `bench watch`).
- Re-running halted instances (covered by `bench retry` #170 and `bench fork`
  #279).
