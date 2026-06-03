# Seeding Runs from GitHub Issues

`mini` can seed a run's task prompt directly from a GitHub issue or a local issue snapshot. This closes the loop between issue discovery and agent execution, removing the need for manual copy-pasting and formatting.

---

## Why it exists

Ingesting tasks from GitHub issues is the most common entry point for bug-fixing workflows. Previously, operators had to manually copy titles, bodies, and comments, strip formatting, and supply the result to `--task` or `--task-file`. Now, Maxwell's Daemon handles the ingestion end-to-end, automatically fetch-wrapping the issue content securely and preserving tracking provenance.

---

## CLI Surface

```bash
max mini --from-issue owner/repo#123                     # Fetch online issue and comments
max mini --from-issue https://github.com/owner/repo/issues/123
max mini --from-issue-file tests/fixtures/issue-snapshot.json  # Load offline snapshot
```

### `--from-issue <ISSUE_REF>`

Ingests from a live GitHub issue. Accepts either:
- The standard reference format: `owner/repo#number` (e.g., `madmax983/rust_swe_agent#484`)
- The full GitHub URL: `https://github.com/owner/repo/issues/number`

This command retrieves the issue title, body, and all comments using the token from the configured `GITHUB_TOKEN` environment variable.

### `--from-issue-file <PATH>`

Ingests from a local JSON snapshot file containing the issue and discussion comments. Used for local reproducibility, testing, and offline operations.

---

## Mutual Exclusivity & Validation Rules

To prevent ambiguous task origins, `mini` enforces strict mutual exclusivity:
- `--from-issue` and `--from-issue-file` are mutually exclusive with each other.
- They are mutually exclusive with `--task`, `--task-file`, and `--resume` (`--resume-from`).
- Attempting to pass more than one of these task sources results in a synchronous validation error exiting with code `2` (`usage_error`).
- `--from-issue` and `--from-issue-file` cannot be combined with `--continue`. Doing so exits with code `2`.

---

## Secure Prompt Isolation (PromptGuard)

All issue-derived content (including the title, body, and discussion comments) is treated as **untrusted user content** to protect against prompt-injection attacks.

The ingested content is wrapped in `<untrusted_task_text>` XML tags using the `PromptGuard` subsystem:

```xml
<untrusted_task_text>
Issue Title: [Title]

Issue Body:
[Ingested Issue Body]

Discussion Comments:
[user1]: [Comment body]
[user2]: [Comment body]
</untrusted_task_text>
```

This envelope informs the down-stream system prompt that the enclosed text must be executed as data rather than instructions.

---

## Offline Snapshot Schema

The offline json file loaded by `--from-issue-file` matches the JSON schema returned by `gh issue view --json title,body,comments`:

```json
{
  "title": "Fix the compiler panic on division by zero",
  "body": "Running `cargo build` on a crate containing `/ 0` causes a compiler crash.",
  "comments": [
    {
      "author": {
        "login": "octocat"
      },
      "body": "panic at main.rs:10"
    },
    {
      "author": {
        "login": "torvalds"
      },
      "body": "Actually, this might be related to constant evaluation."
    }
  ]
}
```

---

## Provenance Tracking Manifest

When a task is seeded from a GitHub issue (or snapshot), tracking metadata is recorded in the trajectory's `MiniProvenanceManifest` to ensure auditability and identical reproduction:

```json
{
  "issue_repo": "owner/repo",
  "issue_number": 123,
  "issue_fetched_at_utc": "2026-06-01T03:00:00Z",
  "issue_body_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
}
```

- **`issue_repo`**: The owner/repository slug (only present when fetched online).
- **`issue_number`**: The numeric issue ID (only present when fetched online).
- **`issue_fetched_at_utc`**: ISO-8601 UTC timestamp of the API ingestion.
- **`issue_body_sha256`**: The SHA-256 hash of the exact issue body, guaranteeing content integrity and verification for offline identical replay.

---

## Failure Modes & Exit Codes

If issue ingestion fails before the agent loop starts, Maxwell's Daemon exits with one of the following stable exit codes:

| Condition | Exit Code | Outcome Class | Description |
| :--- | :---: | :--- | :--- |
| **Missing Github Token** | `34` | `github_issue_missing_token` | The `GITHUB_TOKEN` environment variable was empty or missing. |
| **Issue or Repo Not Found** | `35` | `github_issue_not_found` | The API returned `404 Not Found` (or a `403` indicating a private/unauthorized repository). |
| **Rate Limited** | `36` | `github_issue_rate_limited` | The GitHub API returned a `403 Rate Limit Exceeded` status. |
| **Validation Violations** | `2` | `usage_error` | Multiple task sources specified or empty issue content parsed. |
