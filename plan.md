1. **Extract Limit Checks in `src/agent/default.rs`**
   - Execute the following python script to extract the limit checks from `step` into `check_limits`:
   ```bash
   cat << 'PYEOF' > script.py
with open("src/agent/default.rs", "r") as f:
    content = f.read()

old_block = """        // 1. Limit checks.
        if self.steps >= self.config.root.agent.step_limit {
            self.trajectory.info.exit_reason = Some("step_limit".into());
            self.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
            self.trajectory.info.steps = Some(self.steps);
            self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
            self.emit_run_ended("step_limit", Some(FailureCategory::StepLimit), None);
            return Ok(StepOutcome::Terminate(ExitReason::StepLimit {
                limit: self.config.root.agent.step_limit,
            }));
        }
        if let Some(limit) = self.config.root.agent.cost_limit_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("cost_limit".into());
                self.trajectory.info.failure_category = Some(FailureCategory::CostLimit);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                // Cost limit is also a resource limit; map to the same
                // coarse outcome as step limit per the three-value spec.
                self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
                self.emit_run_ended("cost_limit", Some(FailureCategory::CostLimit), None);
                return Ok(StepOutcome::Terminate(ExitReason::CostLimit {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                }));
            }
        }
        if let Some(limit) = self.config.root.agent.per_task_budget_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("budget_exhausted".into());
                self.trajectory.info.failure_category = Some(FailureCategory::BudgetExhausted);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                self.finalize_run_metadata(outcome::BUDGET_EXHAUSTED);
                self.emit_run_ended(
                    "budget_exhausted",
                    Some(FailureCategory::BudgetExhausted),
                    None,
                );
                return Ok(StepOutcome::Terminate(ExitReason::BudgetExhausted {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                }));
            }
        }"""

new_block = """        // 1. Limit checks.
        if let Some(outcome) = self.check_limits() {
            return Ok(outcome);
        }"""

check_limits_func = """
    fn check_limits(&mut self) -> Option<StepOutcome> {
        if self.steps >= self.config.root.agent.step_limit {
            self.trajectory.info.exit_reason = Some("step_limit".into());
            self.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
            self.trajectory.info.steps = Some(self.steps);
            self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
            self.emit_run_ended("step_limit", Some(FailureCategory::StepLimit), None);
            return Some(StepOutcome::Terminate(ExitReason::StepLimit {
                limit: self.config.root.agent.step_limit,
            }));
        }
        if let Some(limit) = self.config.root.agent.cost_limit_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("cost_limit".into());
                self.trajectory.info.failure_category = Some(FailureCategory::CostLimit);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                // Cost limit is also a resource limit; map to the same
                // coarse outcome as step limit per the three-value spec.
                self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
                self.emit_run_ended("cost_limit", Some(FailureCategory::CostLimit), None);
                return Some(StepOutcome::Terminate(ExitReason::CostLimit {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                }));
            }
        }
        if let Some(limit) = self.config.root.agent.per_task_budget_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("budget_exhausted".into());
                self.trajectory.info.failure_category = Some(FailureCategory::BudgetExhausted);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                self.finalize_run_metadata(outcome::BUDGET_EXHAUSTED);
                self.emit_run_ended(
                    "budget_exhausted",
                    Some(FailureCategory::BudgetExhausted),
                    None,
                );
                return Some(StepOutcome::Terminate(ExitReason::BudgetExhausted {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                }));
            }
        }
        None
    }"""

content = content.replace(old_block, new_block)

insert_marker = """    // The step body walks through 7 sequential phases (limit checks →
    // model query → action parse → bash → observation → trajectory
    // record → bump). Splitting it out would obscure the linear flow
    // for no real reuse benefit.
    #[allow(clippy::too_many_lines)]
    async fn step(&mut self) -> Result<StepOutcome, Error> {"""
content = content.replace(insert_marker, check_limits_func + "\n\n" + insert_marker)

with open("src/agent/default.rs", "w") as f:
    f.write(content)
PYEOF
   python3 script.py
   ```

2. **Verify Modification**
   - Run the following commands to verify:
   ```bash
   git diff src/agent/default.rs
   cargo check
   ```

3. **Verify with tests and lints**
   - Run the following commands:
   ```bash
   cargo fmt --all
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   ```

4. **Complete pre-commit steps**
   - Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.

5. **Submit the change**
   - Submit the change via `submit` tool with title "⚒️ Forge: Extract check_limits from step God Function" and description with 🚮 Smell, ✨ Solution, 🧼 Benefit, and 🛡️ Verification.
