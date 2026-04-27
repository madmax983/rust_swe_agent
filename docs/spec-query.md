# 🔭 Vantage: Spec for Trajectory Query Language

## 👤 User Story
"As a Developer, I want to query my sweep results using a simple query language, so that I can easily find instances with specific failures, token counts, or command patterns without writing custom Python scripts."

## ❓ The "So What?" ask
"What business problem does this solve?"
It reduces the time spent analyzing sweeps. Developers currently manually parse JSON or use basic text filtering. A query language turns the output into an actionable database, significantly speeding up the debugging and iteration loop.

## 🎯 Metric Definition
Success = Complex queries (e.g., 'cost > 0.5 AND outcome = error') execute across 500 instances in under 500ms.

## ✅ Acceptance Criteria
- Must support logical operators (AND, OR).
- Must support querying by metadata (cost, outcome, token count, failure_category).
- Must output a list of matching instance IDs or integrate with the existing `bench inspect` filter logic.

## 🚫 Out of Scope
- Full SQL support (Phase 2).
- Cross-sweep joins.

## 🕳️ Gap Analysis
Standard tools require `jq` wizardry or custom scripts. Native querying lowers the barrier to entry and integrates seamlessly with our Rust toolchain.
