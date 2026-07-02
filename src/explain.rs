//! Offline `explain` registry: a compiled-in read surface over the two stable
//! taxonomies operators react to — the [`crate::exit_code::ExitCode`] contract
//! (documented in `docs/exit-codes.md`) and the
//! [`crate::trajectory::FailureCategory`] enum (documented in
//! `docs/failure-categories.md`).
//!
//! `max explain <selector>` turns an exit code (`7`), an outcome-class name
//! (`verification_failure`), or a failure-category value (`step_limit` /
//! `StepLimit`) into its documented meaning plus a remediation hint, with **no**
//! network or model call. This mirrors `rustc --explain E0382`.
//!
//! This module is the single source of truth for the explanation text. The
//! `tests/explain_drift.rs` suite cross-checks it against `ExitCode`,
//! `docs/exit-codes.md`, and `FailureCategory` in both directions, so a new code
//! or category cannot be added without a matching explanation — contract drift
//! fails the build rather than shipping silently.

/// Stable schema version for the `--format json` output of `max explain`.
pub const EXPLAIN_SCHEMA_VERSION: &str = "1.0";

/// Which taxonomy (or taxonomies) a selector name belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// A process exit-code outcome class from `docs/exit-codes.md`.
    ExitCode,
    /// A trajectory `failure_category` value from `docs/failure-categories.md`.
    FailureCategory,
}

impl Family {
    /// Stable lowercase wire string for JSON output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExitCode => "exit_code",
            Self::FailureCategory => "failure_category",
        }
    }
}

/// One addressable explanation: a code/class/category, its meaning, a
/// remediation hint, and a documentation reference.
#[derive(Debug, Clone, Copy)]
pub struct ExplainEntry {
    /// Process exit code, present only when this entry is an exit-code class.
    pub code: Option<i32>,
    /// Canonical snake_case selector name (outcome class and/or failure-category
    /// wire name).
    pub outcome_class: &'static str,
    /// Taxonomy(ies) this name belongs to.
    pub families: &'static [Family],
    /// One-line documented meaning.
    pub meaning: &'static str,
    /// What to do about it.
    pub remediation: &'static str,
    /// Documentation reference (path, optionally with an anchor).
    pub docs_ref: &'static str,
}

const EXIT_ONLY: &[Family] = &[Family::ExitCode];
const CATEGORY_ONLY: &[Family] = &[Family::FailureCategory];
const BOTH: &[Family] = &[Family::ExitCode, Family::FailureCategory];

const EXIT_DOC: &str = "docs/exit-codes.md";

/// The full registry: every `ExitCode` outcome class and every
/// `FailureCategory` variant. The single cross-taxonomy collision —
/// `agent_stagnation`, which is both exit code 12 and a failure category — is a
/// single merged entry tagged with both families.
#[allow(clippy::too_many_lines)]
pub fn entries() -> &'static [ExplainEntry] {
    &[
        // ── Exit-code outcome classes (docs/exit-codes.md) ──────────────────
        ExplainEntry {
            code: Some(0),
            outcome_class: "success",
            families: EXIT_ONLY,
            meaning: "Command completed with no errors; all checks passed (also covers no-op success).",
            remediation: "No action needed.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(1),
            outcome_class: "internal_error",
            families: EXIT_ONLY,
            meaning: "Unexpected failure: I/O error, JSON parse failure, or unclassified panic. Treat as infrastructure broken, not a domain result.",
            remediation: "Re-run; if it persists capture the stderr message and file a bug. Do not treat as a task or gate result.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(2),
            outcome_class: "usage_error",
            families: EXIT_ONLY,
            meaning: "Bad flag, missing required argument, unknown enum value, or invalid config file.",
            remediation: "Fix the invocation or config and retry; see `max <command> --help`.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(3),
            outcome_class: "preflight_failure",
            families: EXIT_ONLY,
            meaning: "A dependency was unavailable at sweep start: Docker not installed, daemon unreachable, container failed to start, or model endpoint probe failed.",
            remediation: "Verify Docker is running and the model endpoint is reachable, then retry. `max agent doctor` checks host readiness.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(4),
            outcome_class: "task_unsuccessful",
            families: EXIT_ONLY,
            meaning: "The agent ran but produced no usable result: step limit reached, environment command failed, wallclock timeout, or repeated model API errors.",
            remediation: "Inspect the trajectory's `failure_category` for the precise cause, then raise the relevant limit or fix the environment.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(5),
            outcome_class: "budget_halt",
            families: EXIT_ONLY,
            meaning: "Cost ceiling triggered: a forecast projected an over-cap run, or the sweep stopped because `--sweep-cost-limit-usd` was reached.",
            remediation: "Increase the cost cap or reduce the dataset/scope, then retry.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(6),
            outcome_class: "regression_gate_failure",
            families: EXIT_ONLY,
            meaning: "`bench compare` exceeded `--max-regressions` or `--max-patch-size-regression`.",
            remediation: "Review the regressions in `bench compare` output; fix the candidate or adjust the threshold deliberately.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(7),
            outcome_class: "verification_failure",
            families: EXIT_ONLY,
            meaning: "One or more `--verify NAME:COMMAND` checks did not pass after a `mini` run, or `bench bundle` detected a redaction/archive mismatch.",
            remediation: "Read the failing check output and fix the change so the verify command passes.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(8),
            outcome_class: "calibration_optimistic",
            families: EXIT_ONLY,
            meaning: "`bench calibrate --fail-on-optimistic` found actual sweep metrics above the forecast interval.",
            remediation: "Recalibrate the forecast or widen the budget before the next run.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(9),
            outcome_class: "replay_prompt_drift",
            families: EXIT_ONLY,
            meaning: "`bench replay` detected at least one input fingerprint that does not match the cassette.",
            remediation: "Re-record the cassette, or align the inputs with the recorded trajectory.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(10),
            outcome_class: "replay_response_exhausted",
            families: EXIT_ONLY,
            meaning: "`bench replay` ran out of scripted responses before the agent finished (structural drift).",
            remediation: "Re-record the cassette so it covers the full run.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(11),
            outcome_class: "systemic_halt",
            families: EXIT_ONLY,
            meaning: "The mid-sweep circuit breaker tripped: at least N completed instances share the same operator-actionable failure category at or above the share threshold.",
            remediation: "Fix the systemic cause (often `env_setup` or `model_api`) — e.g. check credentials and Docker — then resume the sweep.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(12),
            outcome_class: "agent_stagnation",
            families: BOTH,
            meaning: "The agent repeated the same action at least K times within a trailing window of W steps without progress, so the harness halted it. Surfaces both as process exit 12 and as the `agent_stagnation` failure_category on the trajectory.",
            remediation: "Inspect the repeated action; adjust the task, the available tools, or the K/W stagnation thresholds.",
            docs_ref: "docs/exit-codes.md (see also docs/failure-categories.md#agent_stagnation)",
        },
        ExplainEntry {
            code: Some(13),
            outcome_class: "env_preview_warning",
            families: EXIT_ONLY,
            meaning: "`agent env preview` found at least one risky finding (wide host path, sensitive env var forwarded, MCP server outside workdir, etc.).",
            remediation: "Review the printed findings and tighten the environment before running a sweep.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(14),
            outcome_class: "skills_preview_warning",
            families: EXIT_ONLY,
            meaning: "`agent skills-preview` found a warning: a task hit `max_active`, an activated manifest has no `version`, or `auto_load` matched a skill implicitly.",
            remediation: "Review the preview warnings and adjust the skills configuration.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(15),
            outcome_class: "resume_already_terminal",
            families: EXIT_ONLY,
            meaning: "`mini --resume` target trajectory already has a terminal outcome; the run already completed, was cancelled, or hit a cap.",
            remediation: "Start a fresh run; `--resume` only continues an in-progress run.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(16),
            outcome_class: "resume_manifest_missing",
            families: EXIT_ONLY,
            meaning: "`mini --resume` target is missing required `task`/`model_name` fields (it pre-dates the run-manifest schema).",
            remediation: "Create a fresh run instead of resuming.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(17),
            outcome_class: "resume_invalid_prefix",
            families: EXIT_ONLY,
            meaning: "`mini --resume` target is structurally invalid: the message sequence is empty, too short, or ends in a partial assistant turn.",
            remediation: "Create a fresh run; the trajectory cannot be resumed.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(18),
            outcome_class: "bisect_budget_exhausted",
            families: EXIT_ONLY,
            meaning: "`bench bisect` exhausted its budget before identifying the regressing commit.",
            remediation: "Increase the bisect budget or narrow the commit range.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(19),
            outcome_class: "bisect_schema_break",
            families: EXIT_ONLY,
            meaning: "`bench bisect` found only trajectory-schema breaks in the remaining search space.",
            remediation: "Re-record the affected trajectories or exclude schema-broken commits from the range.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(20),
            outcome_class: "audit_failure",
            families: EXIT_ONLY,
            meaning: "`bench audit` detected a divergence exceeding tolerance or a bijection/evaluator contradiction.",
            remediation: "Investigate the divergence reported by `bench audit` and reconcile the evaluator.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(21),
            outcome_class: "eval_gaming_gate_failure",
            families: EXIT_ONLY,
            meaning: "`bench compare --max-test-only-resolved-rate` was exceeded by the candidate sweep.",
            remediation: "Investigate test-only resolutions (possible eval gaming) before accepting the candidate.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(22),
            outcome_class: "artifact_integrity_violation",
            families: EXIT_ONLY,
            meaning: "`bench export-ci` found JUnit XML aggregate attributes that disagree with the counts in `results.json`.",
            remediation: "Re-run the export on a complete sweep; the artifact is corrupt or incomplete.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(23),
            outcome_class: "scriptability_check_failure",
            families: EXIT_ONLY,
            meaning: "`bench scriptability-check` found at least one misconfigured MCP server or hook. Zero model calls were made; this is a wiring preflight.",
            remediation: "Fix the MCP/hook wiring; distinct from `preflight_failure` so CI can route it separately.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(24),
            outcome_class: "feature_unavailable",
            families: EXIT_ONLY,
            meaning: "The subcommand requires a Cargo feature that was not compiled in.",
            remediation: "Rebuild with the named feature enabled (see the docs link in the error message).",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(25),
            outcome_class: "redact_check_stale_literals",
            families: EXIT_ONLY,
            meaning: "`agent redact-check` found a `secret_literals` entry that produced zero matches against the sample input — likely stale.",
            remediation: "Update or remove the stale literal so redaction keeps protecting something.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(26),
            outcome_class: "redact_check_strict_fail",
            families: EXIT_ONLY,
            meaning: "`agent redact-check --strict` found a `custom_patterns` regex that compiled but produced zero matches.",
            remediation: "Fix the regex typo or update it for the renamed token format.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(27),
            outcome_class: "slo_rule_failure",
            families: EXIT_ONLY,
            meaning: "`bench assert` evaluated all rules and at least one rule (or a missing artifact in fail-closed mode) did not pass.",
            remediation: "Review `assertions.json`; the sweep did not meet the declared SLO. Distinct from `usage_error`.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(28),
            outcome_class: "continue_non_terminal",
            families: EXIT_ONLY,
            meaning: "`mini --continue` target trajectory is non-terminal (still partial/in-progress).",
            remediation: "Use `--resume` to continue an in-progress run; `--continue` only accepts a terminal trajectory.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(29),
            outcome_class: "apply_check_failed",
            families: EXIT_ONLY,
            meaning: "`agent apply` ran `git apply --check` and the patch cannot apply cleanly to the current tree. The working tree is unchanged.",
            remediation: "Rebase the patch onto the current tree or regenerate it.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(30),
            outcome_class: "apply_redacted_refused",
            families: EXIT_ONLY,
            meaning: "`agent apply` detected `[REDACTED:…]` markers in the patch, or the source trajectory recorded patch-submission redaction.",
            remediation: "Use an unredacted patch, or pass `--allow-redacted` to override.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(31),
            outcome_class: "apply_dirty_tree_refused",
            families: EXIT_ONLY,
            meaning: "`agent apply` found uncommitted changes in the target working tree.",
            remediation: "Commit or stash your changes, or pass `--allow-dirty` to override.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(32),
            outcome_class: "redact_audit_findings",
            families: EXIT_ONLY,
            meaning: "`agent redact-audit` found at least one new finding at `medium` or higher severity in the scanned artifacts.",
            remediation: "Treat as a leak: scrub the artifact before publishing.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(33),
            outcome_class: "redact_audit_scan_error",
            families: EXIT_ONLY,
            meaning: "`agent redact-audit` could not read or extract one or more artifacts, so the scan is incomplete.",
            remediation: "Fix the unreadable/corrupt artifact and re-run; a clean verdict cannot be trusted yet.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(34),
            outcome_class: "github_issue_missing_token",
            families: EXIT_ONLY,
            meaning: "The `GITHUB_TOKEN` environment variable was empty or missing during issue ingestion.",
            remediation: "Export a valid `GITHUB_TOKEN` and retry.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(35),
            outcome_class: "github_issue_not_found",
            families: EXIT_ONLY,
            meaning: "The GitHub API returned 404 Not Found (or a 403 for a private/unauthorized repository).",
            remediation: "Check the issue/repo reference and the token's scope.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(36),
            outcome_class: "github_issue_rate_limited",
            families: EXIT_ONLY,
            meaning: "The GitHub API returned 403 Rate Limit Exceeded during issue ingestion.",
            remediation: "Wait for the rate-limit window to reset, or use a token with a higher limit.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(37),
            outcome_class: "injection_audit_hits",
            families: EXIT_ONLY,
            meaning: "`agent injection-audit` found at least one hit at or above the configured `--fail-on` severity threshold.",
            remediation: "Review the flagged content for prompt injection before publishing.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(38),
            outcome_class: "injection_audit_scan_error",
            families: EXIT_ONLY,
            meaning: "`agent injection-audit` could not read the sweep directory or a trajectory file, so the scan is incomplete.",
            remediation: "Fix the unreadable path and re-run; a clean verdict cannot be trusted yet.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(39),
            outcome_class: "stability_gate_failure",
            families: EXIT_ONLY,
            meaning: "`agent stability --fail-under <F>` found `pass_at_k < F`. All runs completed; the measured pass rate missed the threshold.",
            remediation: "Improve run stability, or lower the threshold deliberately.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(40),
            outcome_class: "dataset_verify_mismatch",
            families: EXIT_ONLY,
            meaning: "`bench dataset-verify` detected a mismatch between the candidate dataset and the canonical reference.",
            remediation: "Re-sync the dataset to the canonical reference.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(41),
            outcome_class: "best_of_all_failed",
            families: EXIT_ONLY,
            meaning: "`agent best-of` completed all runs and none passed every verify check. The best-scoring run was still selected and its patch emitted.",
            remediation: "Improve the task or model, or pass `--allow-no-pass` to accept the best run while keeping `all_failed: true`.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(42),
            outcome_class: "config_override_warning",
            families: EXIT_ONLY,
            meaning: "`agent config resolve` found a clap-default override hazard: a `--config` field (`model.name` or `agent.step_limit`) will be silently overwritten unless its flag is also passed.",
            remediation: "Pass the corresponding flag explicitly, or accept the resolved config (exit 0 means no hazards).",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(43),
            outcome_class: "eval_parity_gate_failure",
            families: EXIT_ONLY,
            meaning: "`bench eval-parity --min-agreement <F>` measured an offline-vs-canonical agreement rate below the declared threshold.",
            remediation: "Investigate evaluator divergence; distinct from `slo_rule_failure`.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(44),
            outcome_class: "utilization_gate_failure",
            families: EXIT_ONLY,
            meaning: "`bench utilization --min-utilization <PCT>` measured a concurrency utilization below the declared floor.",
            remediation: "Increase concurrency or investigate under-utilization; distinct from `slo_rule_failure`.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(45),
            outcome_class: "fs_audit_findings",
            families: EXIT_ONLY,
            meaning: "`agent fs-audit` found at least one bash command that accessed a path outside the configured workdir.",
            remediation: "Review the filesystem-boundary violation before publishing.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(46),
            outcome_class: "fs_audit_scan_error",
            families: EXIT_ONLY,
            meaning: "`agent fs-audit` could not read or parse one or more trajectory files, so the scan is incomplete.",
            remediation: "Fix the unreadable/invalid trajectory and re-run; a clean verdict cannot be trusted yet.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(47),
            outcome_class: "artifact_check_failure",
            families: EXIT_ONLY,
            meaning: "`agent artifact-check` found an `invalid` or `unsupported_major` artifact (with `--strict`, also legacy/with-warnings). Zero model calls; a structural conformance gate.",
            remediation: "Regenerate the artifact so it conforms to the contract.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(48),
            outcome_class: "host_not_ready",
            families: EXIT_ONLY,
            meaning: "`agent doctor` found a failing host-readiness check: git missing, provider credential absent, Docker daemon unreachable, runs/output dir not writable, or toolchain below the crate `rust-version`.",
            remediation: "Fix the failing host check reported by `agent doctor`, then retry.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(49),
            outcome_class: "ledger_budget_exceeded",
            families: EXIT_ONLY,
            meaning: "`bench ledger --budget-usd <N>` found that the grand total actual spend across discovered trajectories meets or exceeds N. The report is printed before exit.",
            remediation: "Review the per-model/dataset/day breakdown in the ledger report and reduce spend, or raise `--budget-usd`.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(50),
            outcome_class: "disk_usage_prune_blocked",
            families: EXIT_ONLY,
            meaning: "`bench du --prune --apply` matched at least one sweep against the given selectors that could not be proven idle (a partial checkpoint was touched within `--in-progress-window`). That sweep was skipped and reported `protected`; every other matching sweep was still deleted.",
            remediation: "Re-run once the live sweep finishes, or narrow the selectors to exclude it. The report lists exactly which sweep was protected.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(130),
            outcome_class: "interrupted",
            families: EXIT_ONLY,
            meaning: "Graceful SIGINT / Ctrl-C cancellation (POSIX convention: 128 + SIGINT(2)).",
            remediation: "Re-run if the interruption was unintended; partial results may be on disk.",
            docs_ref: EXIT_DOC,
        },
        ExplainEntry {
            code: Some(137),
            outcome_class: "killed",
            families: EXIT_ONLY,
            meaning: "SIGKILL escalation after the graceful-cancel deadline expired (128 + SIGKILL(9)).",
            remediation: "Re-run; if cancellation routinely escalates to a kill, increase the graceful-cancel deadline.",
            docs_ref: EXIT_DOC,
        },
        // ── Failure categories (docs/failure-categories.md) ─────────────────
        // (agent_stagnation is the merged exit/category entry above.)
        ExplainEntry {
            code: None,
            outcome_class: "env_setup",
            families: CATEGORY_ONLY,
            meaning: "An environment or infrastructure failure prevented a successful run: setup failed before any agent turns (Docker down, image pull/clone failed) or patch capture failed after the agent ran.",
            remediation: "Check Docker, image availability, and the workspace. Operator-actionable: can trip the systemic-halt circuit breaker.",
            docs_ref: "docs/failure-categories.md#env_setup",
        },
        ExplainEntry {
            code: None,
            outcome_class: "model_api",
            families: CATEGORY_ONLY,
            meaning: "Repeated errors calling the model API (invalid key, exhausted quota, or persistent provider errors).",
            remediation: "Verify the API key, quota, and provider status. Operator-actionable: can trip the systemic-halt circuit breaker.",
            docs_ref: "docs/failure-categories.md#model_api",
        },
        ExplainEntry {
            code: None,
            outcome_class: "model_parse",
            families: CATEGORY_ONLY,
            meaning: "Repeated failures to parse the model's response format.",
            remediation: "Check the prompt/format contract and the model's compatibility with the expected output shape.",
            docs_ref: "docs/failure-categories.md#model_parse",
        },
        ExplainEntry {
            code: None,
            outcome_class: "step_limit",
            families: CATEGORY_ONLY,
            meaning: "The agent exhausted its maximum step budget before submitting.",
            remediation: "Raise `--step-limit` if the task genuinely needs more steps, or investigate why the agent looped.",
            docs_ref: "docs/failure-categories.md#step_limit",
        },
        ExplainEntry {
            code: None,
            outcome_class: "cost_limit",
            families: CATEGORY_ONLY,
            meaning: "The per-task USD cost ceiling was reached.",
            remediation: "Raise the per-task cost limit or reduce task scope.",
            docs_ref: "docs/failure-categories.md#cost_limit",
        },
        ExplainEntry {
            code: None,
            outcome_class: "budget_exhausted",
            families: CATEGORY_ONLY,
            meaning: "The per-task USD budget was reached mid-loop; any patch accumulated before the cap fired is preserved.",
            remediation: "Raise the budget or reduce scope; inspect the preserved partial patch.",
            docs_ref: "docs/failure-categories.md#budget_exhausted",
        },
        ExplainEntry {
            code: None,
            outcome_class: "wallclock_timeout",
            families: CATEGORY_ONLY,
            meaning: "The maximum real-time execution duration was reached.",
            remediation: "Raise the wallclock limit or reduce task scope.",
            docs_ref: "docs/failure-categories.md#wallclock_timeout",
        },
        ExplainEntry {
            code: None,
            outcome_class: "agent_internal",
            families: CATEGORY_ONLY,
            meaning: "An internal logic error within the agent harness.",
            remediation: "Capture the trajectory and file a bug; this is not an expected per-task outcome.",
            docs_ref: "docs/failure-categories.md#agent_internal",
        },
        ExplainEntry {
            code: None,
            outcome_class: "patch_apply_invalid",
            families: CATEGORY_ONLY,
            meaning: "A patch was captured but `git apply --check` rejected it at capture time.",
            remediation: "Inspect the diff; the agent produced a malformed or conflicting patch.",
            docs_ref: "docs/failure-categories.md#patch_apply_invalid",
        },
        ExplainEntry {
            code: None,
            outcome_class: "patch_empty",
            families: CATEGORY_ONLY,
            meaning: "The agent submitted but the captured diff was empty (zero bytes).",
            remediation: "Check whether the agent actually edited files; the submission produced no change.",
            docs_ref: "docs/failure-categories.md#patch_empty",
        },
        ExplainEntry {
            code: None,
            outcome_class: "secret_leak_detected",
            families: CATEGORY_ONLY,
            meaning: "A configured secret literal was found in a submission artifact.",
            remediation: "Rotate the exposed secret, scrub the artifact, and review the redaction config.",
            docs_ref: "docs/failure-categories.md#secret_leak_detected",
        },
        ExplainEntry {
            code: None,
            outcome_class: "history_compaction_failed",
            families: CATEGORY_ONLY,
            meaning: "Trajectory history could not be compacted within `history_max_input_tokens` even after eliding all older observations.",
            remediation: "Reduce context usage or raise `history_max_input_tokens`. Operator-actionable: can trip the systemic-halt circuit breaker.",
            docs_ref: "docs/failure-categories.md#history_compaction_failed",
        },
        ExplainEntry {
            code: None,
            outcome_class: "read_only_violation",
            families: CATEGORY_ONLY,
            meaning: "Read-only mode blocked a tool invocation that attempted a write.",
            remediation: "Remove `--read-only` for write tasks, or constrain the agent to read-only actions.",
            docs_ref: "docs/failure-categories.md#read_only_violation",
        },
        ExplainEntry {
            code: None,
            outcome_class: "unknown",
            families: CATEGORY_ONLY,
            meaning: "An unknown or unclassified failure, or a value produced by a newer harness version this reader does not recognise. Reserved for forward-compatible parsing.",
            remediation: "Update to a matching harness version, or inspect the raw trajectory for details.",
            docs_ref: "docs/failure-categories.md#unknown",
        },
    ]
}

/// Normalize a selector for case-insensitive matching across snake_case and the
/// enum's PascalCase display form: lowercase and drop underscores so
/// `step_limit`, `StepLimit`, and `STEP_LIMIT` all collapse to `steplimit`.
fn normalize(selector: &str) -> String {
    selector
        .chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolve a selector — an exit-code integer, an outcome-class name, or a
/// failure-category value — to its explanation. Returns `None` if unknown.
#[must_use]
pub fn resolve(selector: &str) -> Option<&'static ExplainEntry> {
    let trimmed = selector.trim();
    if let Ok(code) = trimmed.parse::<i32>() {
        return entries().iter().find(|e| e.code == Some(code));
    }
    let needle = normalize(trimmed);
    entries()
        .iter()
        .find(|e| normalize(e.outcome_class) == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolves_by_integer_code() {
        let e = resolve("7").unwrap();
        assert_eq!(e.outcome_class, "verification_failure");
        assert_eq!(e.code, Some(7));
    }

    #[test]
    fn resolves_outcome_class_case_insensitively() {
        assert_eq!(resolve("verification_failure").unwrap().code, Some(7));
        assert_eq!(resolve("VERIFICATION_FAILURE").unwrap().code, Some(7));
        assert_eq!(resolve("Verification_Failure").unwrap().code, Some(7));
    }

    #[test]
    fn resolves_failure_category_snake_and_pascal() {
        let snake = resolve("step_limit").unwrap();
        let pascal = resolve("StepLimit").unwrap();
        assert_eq!(snake.outcome_class, "step_limit");
        assert_eq!(snake.outcome_class, pascal.outcome_class);
        assert!(snake.code.is_none());
    }

    #[test]
    fn unknown_selector_resolves_to_none() {
        assert!(resolve("definitely_not_real").is_none());
        assert!(resolve("9999").is_none());
    }

    #[test]
    fn agent_stagnation_carries_both_families() {
        let e = resolve("agent_stagnation").unwrap();
        assert_eq!(e.code, Some(12));
        assert!(e.families.contains(&Family::ExitCode));
        assert!(e.families.contains(&Family::FailureCategory));
    }

    #[test]
    fn no_duplicate_outcome_classes() {
        let mut seen = std::collections::HashSet::new();
        for e in entries() {
            assert!(seen.insert(e.outcome_class), "dup: {}", e.outcome_class);
        }
    }
}
