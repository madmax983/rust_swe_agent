# Spec: Agent Run Budgets

## 👤 User Story
"As an Engineering Manager running large-scale SWE-bench sweeps, I want to set hard caps on API costs and run durations per agent, so that a runaway loop or unexpected LLM behavior doesn't drain our budget or hog compute resources."

## 💼 The "So What?" (Business Problem)
Currently, agents can enter infinite loops or output excessively large prompt contexts during evaluation sweeps. This leads to unpredictable API bills (especially with models like GPT-4) and wasted compute time. Adding hard budgets provides financial predictability and operational safety, allowing us to run sweeps at a larger scale with confidence. Complexity must be managed through simple, verifiable configuration limits rather than complex dynamic scaling.

## 🎯 Success Metrics
- **Cost Predictability:** 100% of agent runs terminate if they exceed the defined USD cost cap.
- **Compute Efficiency:** 100% of agent runs terminate if they exceed the defined total step limit or time duration.
- **Operational Clarity:** Clear exit reason (`BudgetExceeded`) recorded in the final trajectory/evaluation output, with no corrupted state.

## 🔍 Gap Analysis
- **Current State:** The agent relies on internal loop detection or external execution environment timeouts. It lacks native awareness of accumulated LLM API costs or token usage limits.
- **Standard Tooling:** Other frameworks (e.g., Langchain, Autogen) often provide callback handlers to track token usage and abort, but we need a deterministic check in our core agent loop.

## ✅ Acceptance Criteria
- Must introduce configuration options for: `max_cost_usd`, `max_steps`, and `max_duration_seconds`.
- Must track accumulated API cost based on token usage reported by the model provider.
- Must halt the agent loop gracefully if any budget threshold is exceeded before the next model invocation.
- Must include the termination reason (e.g., `BudgetExceeded(Cost)`) in the final output file (trajectory/evaluation result).
- Must not panic or crash when a limit is reached; it must cleanly exit and log the event.

## 🚫 Out of Scope
- Dynamic budget reallocation between different agent runs.
- Integration with external billing APIs (e.g., Stripe or AWS Billing).
- Complex heuristic-based cost estimation; we will rely strictly on token usage multiplied by fixed per-token cost rates.
