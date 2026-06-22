# `failure-signature` Spec

`bench failure-digest` summarizes a single instance's terminal failure. As of
schema `1.1` every digest also carries a stable, redaction-safe **failure
signature** so a downstream consumer (the nightly smoke, a `bench retry`, or a
rerun) can deterministically tell a *known recurring* failure apart from a *new*
regression without re-reading the trajectory.

The signature reuses the same `FailureSignature` primitive that powers
multi-instance clustering in [`bench triage`](spec-triage.md); this document
specifies how it is surfaced for the single-instance digest and the stability
guarantees that hold there.

## Surfaces

### JSON (`--format json`)

The digest gains a `failure_signature` object and, when a baseline is supplied,
a `recurrence` object:

```json
{
  "schema_version": "1.1",
  "instance_id": "error-1",
  "outcome": "error",
  "failure_category": "model_parse",
  "failure_signature": {
    "signature_id": "aabbccdd11223344",
    "failure_category": "model_parse",
    "assistant_tail": "i tried to parse the model response but failed ...",
    "bash_exit_code": 1,
    "stderr_line": "model parse error: unexpected token at position <num>",
    "summary": "assistant=\"...\" exit=1 stderr=\"...\""
  },
  "recurrence": {
    "baseline_signature_id": "aabbccdd11223344",
    "verdict": "recurring"
  }
}
```

`recurrence` is omitted entirely when `--baseline-signature` is not supplied.

### Markdown (default)

A stable, greppable line is rendered directly under the headline:

```text
failure_signature: aabbccdd11223344
recurrence: recurring
```

The `recurrence:` line is present only when a baseline was supplied. CI logs and
humans can match a failure with a plain `grep 'failure_signature: '`.

## Signature Inputs

`signature_id` is the first 16 lowercase hex characters of SHA-256 over a stable
key built from four fields:

1. `failure_category` — the digest's failure category, or `none` when the
   instance has no failure category. See
   [`docs/failure-categories.md`](failure-categories.md) for the vocabulary.
2. The last assistant message tail, capped at the final 500 Unicode scalar
   values.
3. The last bash exit code, or `none` when no bash result is present.
4. The last non-empty stderr line from the last bash result.

The text fields are normalized before hashing exactly as in `bench triage`
(lowercase; collapse whitespace; `<path>` for path-like tokens; `<num>` for
number runs). The `summary` is a short (≤160 char) human rendering of the
assistant tail, exit code, and stderr line. This is the identical
`FailureSignature` computation documented in
[`spec-triage.md`](spec-triage.md#signature-function); two failures collide only
when all four normalized fields match.

## Stability Guarantees

- **Determinism.** Two independent runs whose terminal failure matches on all
  four signature inputs produce an identical `signature_id`.
- **Discrimination.** A differing terminal failure — different category, exit
  code, or stderr tail — produces a different `signature_id`.
- **Redaction-safe.** The signature is computed *only* from the already-redacted
  terminal fields the digest emits (assistant message and tool stderr are passed
  through the `Redactor` before the signature is built). No raw secret material
  can appear in `signature_id` or `summary`.
- **Stable across record/replay.** Per-run redaction markers carry a run-specific
  salt (`[REDACTED:kind:size:salt]`). The signature inputs are passed through
  [`fingerprint::normalize_redaction_markers`](../src/fingerprint.rs) — which
  strips the salt suffix to `[REDACTED:kind:size]` — so a recording run and its
  replay of the same failure yield the same `signature_id`.

## Baseline Recurrence Verdict

`--baseline-signature <SIGNATURE_ID>` classifies the current failure against a
known baseline:

| Condition | `verdict` |
| --- | --- |
| Computed `signature_id` equals the baseline | `recurring` |
| Otherwise | `new` |

The verdict is **informational only**. It is reflected in both the JSON and
markdown output but **never changes the process exit code** — `bench
failure-digest` still exits `0` on a successful digest regardless of the verdict.

## Success Metric

Over a rolling 7-day window of nightly failures with an unchanged underlying
terminal failure, the number of **distinct** `signature_id` values equals **1**
(false-new rate < 1%). This lets a downstream consumer dedup N duplicate issues
into one recurrence-counted issue.

## Out of Scope

- The nightly workflow's issue-filing/dedup logic that *consumes* this signature.
- Cross-instance clustering or sweep-vs-sweep cluster deltas — served by
  [`bench triage`](spec-triage.md) and `bench triage-diff`.
- Multi-instance aggregation; the digest and its signature stay single-instance.
- Any persistent signature-history datastore.
