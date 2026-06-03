# `agent injection-audit` — Post-hoc Prompt-Injection Audit (issue #343)

## Summary

`agent injection-audit --sweep <DIR>` scans every `*.traj.json` in a sweep
directory for known prompt-injection signatures, inspecting only the content of
untrusted XML envelopes. Operators can quantify injection exposure, locate
suspicious instances, and decide whether to discard or re-run affected results
before publishing a leaderboard number.

The command is **zero-cost**: read-only over trajectory files, no model calls, no
network access, no mutation of sweep artifacts.

## Motivation

`PromptGuard` wraps every piece of untrusted content in XML envelopes so the
model can distinguish operator instructions from data. However, wrapping is a
structural cue, not a detector — when a task description or tool output contains
injection bait ("ignore previous instructions", fake `<|system|>` tokens, exfil
URLs), the trajectory records the wrapped content but operators have no signal
that an attempt occurred. This command provides that signal via a fast,
deterministic, dependency-light signature scan over the stored envelopes.

## Related Commands

| Command | Purpose |
|---------|---------|
| `agent redact-audit` | Detects *secret leaks* in sweep artifacts (issue #342) |
| `agent injection-audit` | Detects *injection signals* in sweep trajectories (this spec) |

## Usage

```
max agent injection-audit --sweep <DIR> [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep <DIR>` | (required) | Sweep directory containing `*.traj.json` files |
| `--signatures <FILE>` | none | YAML or JSON file with custom signature entries to add |
| `--format <FMT>` | `text` | Output format: `text`, `json`, or `jsonl` |
| `--fail-on <SEV>` | `medium` | Minimum severity that triggers exit code 34 |
| `--output <FILE>` | stdout | Write report to this file in addition to stdout |

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | No hits at or above the `--fail-on` threshold |
| 34 (`injection_audit_hits`) | At least one hit at or above the threshold |
| 35 (`injection_audit_scan_error`) | Could not read the sweep directory or a trajectory file |
| 1 (`internal_error`) | Unexpected internal error |
| 2 (`usage_error`) | Invalid flag value or configuration error |

## Scanned Envelopes

The audit inspects **only** content wrapped in these XML envelopes:

| Tag | `UntrustedKind` | Typical source |
|-----|-----------------|----------------|
| `<untrusted_task_text>` | `TaskText` | SWE-bench problem statement |
| `<untrusted_extra_context>` | `ExtraContext` | Supplementary context |
| `<untrusted_tool_output>` | `ToolOutput` | Bash / MCP tool stdout/stderr |
| `<untrusted_hook_output>` | `HookOutput` | Pre/post-tool hook output |
| `<untrusted_repo_content>` | `RepoContent` | README, source files, etc. |

System messages (operator instructions) and assistant messages (model output)
are **never scanned**.

## Default Signature Pack

| Name | Pattern description | Severity |
|------|---------------------|----------|
| `instruction_override` | "ignore (all/the/previous/prior) instructions/prompt/rules" | high |
| `disregard_above` | "disregard the above" | high |
| `you_are_now` | "you are now a/an/the …" | medium |
| `act_as` | "act as a/an/the …" | medium |
| `from_now_on` | "from now on you are …" | high |
| `chatgpt_system_token` | `<\|system\|>` | high |
| `llama_sys_tag` | `<<SYS>>` | high |
| `llama_inst_tag` | `[INST]` | high |
| `chatml_im_start` | `<\|im_start\|>` | high |
| `chatml_im_end` | `<\|im_end\|>` | high |
| `developer_instruction_block` | Fenced `` ```developer instruction `` block | high |
| `developer_instruction_bracket` | `[DEVELOPER INSTRUCTION]` | high |
| `curl_pipe_sh` | `curl … \| sh` or `\| bash` | high |
| `base64_api_key_exfil` | `$AWS_*/ANTHROPIC_API_KEY/OPENAI_API_KEY … \| base64` | high |
| `webhook_host` | `requestbin.com`, `webhook.site`, `pipedream.net` | high |

## Custom Signatures

Operators can extend the default pack with a YAML or JSON file:

```yaml
# custom_sigs.yaml
- name: my_custom_signal
  pattern: "super secret injection pattern"
  kind: custom
  severity: high
```

```json
[{"name": "json_signal", "pattern": "inject_me", "kind": "custom", "severity": "medium"}]
```

```
max agent injection-audit --sweep ./runs --signatures custom_sigs.yaml
```

Custom signatures **supplement** the built-in pack; they do not replace it.

## Hit Record Schema

Each hit record (in `--format json` hits array or `--format jsonl` lines):

```json
{
  "instance_id": "django__django-11422",
  "trajectory_path": "django__django-11422.traj.json",
  "step_index": 1,
  "envelope_kind": "task_text",
  "signature_name": "instruction_override",
  "severity": "high",
  "byte_offset_start": 42,
  "byte_offset_end": 73,
  "context": "…redacted 80-char window around the match…"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `instance_id` | string | Trajectory filename stem (e.g. `django__django-11422`) |
| `trajectory_path` | string | Path relative to the sweep directory |
| `step_index` | integer | 0-based index of the message in `messages[]` |
| `envelope_kind` | string | One of `task_text`, `extra_context`, `tool_output`, `hook_output`, `repo_content` |
| `signature_name` | string | Matched signature name (e.g. `instruction_override`) |
| `severity` | string | `low`, `medium`, or `high` |
| `byte_offset_start` | integer | Start byte offset within the envelope content |
| `byte_offset_end` | integer | End byte offset (exclusive) within the envelope content |
| `context` | string | Redacted 80-char context window around the match |

## JSON Report Schema

```json
{
  "artifact_kind": "injection_audit",
  "schema_version": {"major": 1, "minor": 0},
  "sweep_dir": "./runs",
  "trajectories_scanned": 500,
  "total_hits": 3,
  "hit_counts_by_signature": {"instruction_override": 2, "curl_pipe_sh": 1},
  "hit_counts_by_envelope_kind": {"task_text": 2, "tool_output": 1},
  "hits": [...],
  "scan_errors": []
}
```

## `--fail-on` Severity Threshold

The `--fail-on <SEVERITY>` flag configures the minimum severity that triggers
exit code 34:

| `--fail-on` value | Exit 34 when … |
|-------------------|----------------|
| `high` | Any `high`-severity hit found |
| `medium` (default) | Any `medium` or `high` hit found |
| `low` | Any hit at any severity found |

## Output Formats

### `--format text` (default)

Human-readable rollup printed to stdout. Includes:
- Summary line with trajectory count and hit count
- Hits grouped by signature and envelope kind
- Per-hit details: instance, kind, step, signature, severity, context snippet

### `--format json`

Machine-readable JSON report (full schema above). Suitable for CI snapshot
diffing. The hits array contains every hit record.

### `--format jsonl`

One JSON hit record per line. Suitable for piping to `jq`, `grep`, or streaming
processors. No report header — only hit records are emitted.

## Performance Budget

The audit scans each trajectory file once, extracting envelope content with
a linear string scan and matching against compiled regexes. The expected
throughput is ≥ 100 trajectories/second, so a 500-instance sweep finishes in
≤ 30 seconds on a developer laptop.

## Security Properties

- **Read-only**: The audit never writes to sweep artifacts; all output goes to
  `--output` or stdout.
- **No model calls**: Pure Rust; no network, no subprocess.
- **Context window bounded**: The 80-character context window cannot carry large
  raw payloads out of the trajectory.
- **Operator content excluded**: Only envelope-wrapped content is scanned;
  system messages and model output are ignored by construction.

## Out of Scope

- Inline blocking or remediation at run time (post-hoc only)
- ML-based or LLM-judge classification
- Detecting injection inside operator-controlled inputs (task instructions,
  hook scripts)
- Redacting or modifying historical trajectories
- Cross-sweep trending (`bench injection-trend` would be a future addition)
