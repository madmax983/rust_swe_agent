# 🔭 Vantage: Spec for GitHub Integration

## 👤 User Story
"As a Lead Developer, I want the agent to post its proposed patch directly as a GitHub Pull Request (or PR comment), so that my team can review its work natively in our existing code review workflow without manually applying patches."

## 💡 "So What?" (Business Problem)
Currently, running `rust-swe-agent` requires a developer to execute it locally, find the resulting patch, and manually apply it. This manual intervention increases friction and reduces daily active usage.
By integrating natively with GitHub, the agent becomes an automated contributor. This reduces the time-to-value for the tool and significantly improves the developer experience. Complexity is a cost, utility is revenue: integrating where developers already work generates high utility.

## 📈 Success Metrics
- **Metric 1:** 80% reduction in "time-to-review" for agent-generated patches.
- **Metric 2:** 100% of successful agent trajectory patches can be mapped to GitHub PRs seamlessly.
- **Metric 3:** Zero manual terminal commands required to view an agent patch.

## ✅ Acceptance Criteria
- Must authenticate via GitHub App or Personal Access Token (PAT).
- Must open a Pull Request against the specified target branch containing the agent's patch.
- Must link to the trajectory logs or summary in the Pull Request description so developers can review the agent's thought process.
- Must handle API rate limits gracefully without panicking.

## 🚫 Out of Scope
- Automatic merging of PRs.
- Support for GitLab, Bitbucket, or other VCS platforms (Phase 2).
- Real-time streaming of the agent's internal reasoning as live PR comments.
