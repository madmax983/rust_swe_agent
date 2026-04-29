# 🔭 Vantage: Spec for Patch Apply Failure Telemetry

## 👤 User Story
"As a Software Engineer debugging SWE-bench runs, I want to see detailed error output when a patch fails to apply (`patch_apply_failed`), so that I can figure out why the diff produced by the model was rejected."

## ❓ The "So What?" (Business Problem)
Currently, when `rust-swe-agent bench evaluate` encounters a `patch_apply_failed` exit reason (which we already parse from SWE-bench evaluators), it drops the specific diff or patch conflict context. Without this error output, operators are blind as to whether the failure was due to malformed Git diffs, unexpected file state, or missing context. Storing and exposing this telemetry saves hours of manual repro efforts.

## 🎯 Metric Definition
Success = Time to diagnose a `patch_apply_failed` error drops from >5 minutes (running git apply manually) to <10 seconds (reading the error log directly in `bench inspect`).

## ✅ Acceptance Criteria
- Extend the `EvaluationResults` schema in `evaluation.json` (and internal models) to capture a `patch_error_log` string if present when `eval_exit_reason` is `patch_apply_failed`.
- The `bench evaluate` command must extract this error string from the `sb-cli` backend output when the patch fails.
- The `bench inspect` command must surface this patch application error context if it exists.

## 🚫 Out of Scope
- Fixing the actual model prompt to avoid patch errors (this is an analysis tool feature).
- Re-running the patch application logic directly within Rust (we still rely on the backend/sb-cli for the evaluation step).

## 🕳️ Gap Analysis
- **SWE-bench**: Returns `patch_apply_failed` and logs out standard error during the evaluation container step.
- **rust_swe_agent today**: Identifies the exit reason but does not keep the standard error output related to that specific reason, creating a debugging dead end.
