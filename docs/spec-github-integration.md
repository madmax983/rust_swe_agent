# 🔭 Vantage: Spec for GitHub App Integration

## 👤 User Story
"As a repository maintainer, I want to install rust_swe_agent as a GitHub App on my repository, so that I can automatically trigger agent runs by assigning issues or tagging the bot in PRs."

## ❓ The "So What?" (Business Problem)
Currently, users must run `rust_swe_agent` locally or in a self-hosted CI pipeline to analyze issues or review code. This creates friction, requiring infrastructure setup and terminal access. A GitHub App integration allows teams to interact with the agent natively within their existing workflow. By lowering the barrier to entry, we increase adoption and daily active usage, turning the agent from a developer tool into an embedded team member.

## 🎯 Metric Definition
Success =
1. Time from app installation to first successful agent comment on an issue is < 5 minutes.
2. The agent successfully processes >95% of tagged GitHub webhooks within 10 seconds of receipt.

## ✅ Acceptance Criteria
- Must provide a webhook endpoint to receive GitHub events (e.g., `issue_comment.created`, `issues.assigned`).
- Must authenticate securely using GitHub App private keys and installation tokens.
- Must be able to automatically read the contents of the issue/PR and clone the corresponding repository branch.
- Must post a comment back to the GitHub issue/PR with the agent's progress/results (or link to a web UI trace).
- Must queue incoming requests gracefully without dropping them during spikes in activity.

## 🚫 Out of Scope
- A hosted SaaS offering (Phase 2, this spec is for open-source users to run their own GitHub App).
- Integration with other forges (GitLab, Bitbucket).
- Creating new PRs proactively without human prompting.
