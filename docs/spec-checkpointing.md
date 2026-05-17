# 🔭 Vantage: Spec for Mid-Run Trajectory Checkpointing

## 👤 User Story
"As a Machine Learning Engineer running large-scale evaluation sweeps, I want the agent to incrementally checkpoint its progress mid-run, so that if the process is interrupted (e.g., node preemption or OOM), I can resume exactly from the last completed step without wasting API budget re-running successful turns."

## ❓ The "So What?" (Business Problem)
Currently, the `--resume` flag only skips tasks that have fully completed and have a valid trajectory and patch file. If a run crashes at step 49 out of 50, all of the API tokens and time spent on those 49 steps are lost because the trajectory is incomplete and invalid. In long-running sweeps where each instance can cost several dollars, this binary completion state wastes significant API budget and wall-clock time during transient failures or spot instance preemptions. Mid-run checkpointing solves this by saving the trajectory after every model turn, turning wasted work into resumable progress.

## 🎯 Metric Definition
Success = Mid-run resumption re-uses >90% of previously successful steps after an interruption, leading to an overall reduction in wasted token spend (cost savings) and faster recovery times for incomplete runs.

## ✅ Acceptance Criteria
- Must incrementally write or update a trajectory checkpoint file on disk at the end of every agent step, not just at termination.
- Must enhance the `--resume` functionality to detect incomplete trajectories, read the existing steps, and initialize the agent state to continue from the last successful step instead of starting from scratch.
- Must ensure that checkpoint files are atomic or fail-safe (e.g., writing to a temporary file and renaming) so that an interruption during the write does not corrupt the entire history.
- Must correctly append new steps to the resumed trajectory so that the final output file looks indistinguishable from a continuous run.

## 🚫 Out of Scope
- Branching or rewinding histories (e.g., resuming from step 5 instead of the latest step 49) – this is a Phase 2 analytical feature.
- Multi-agent coordination state – only standard single-agent trajectories are covered.
- Hot-reloading Docker containers with precise filesystem state (resuming the bash state is necessary but we assume standard environment re-initialization can reconstruct state from bash history).

## 🕳️ Gap Analysis
- **SWE-agent**: Supports resuming from trajectory checkpoints.
- **maxwells-daemon today**: Has a `--resume` flag, but it only looks for completed tasks (`resume-skip`). Incomplete runs are treated as absent, discarding all partial work and completely re-running the instance.
