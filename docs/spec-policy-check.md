# `agent policy-check` — Policy Config Preflight

**Issue:** #335  
**Status:** Implemented  
**Complexity tier:** S

## Problem

Maxwell's Daemon ships a pre-execution command policy engine with `safe` / `ask` / `yolo` profiles plus operator-supplied `extra_deny_patterns` and `extra_allow_patterns`. The only way an operator could previously verify whether their custom regex matched the commands they intended to block — or that it over-blocked routine commands — was to run a paid sweep. This command provides a deterministic, $0, no-model preflight.

## Usage

```
max agent policy-check [OPTIONS]
```

### Input sources (exactly one required)

| Flag | Description |
|------|-------------|
| `--commands-file <PATH>` | Read one command per line from a file |
| `--stdin` | Read commands from stdin (one per line) |
| `--command <CMD>` | Ad-hoc command (repeatable) |

Blank lines and lines beginning with `#` in a commands file (or stdin) are silently ignored.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--config <PATH>` | harness default | TOML config containing a `[policy]` block |
| `--format text\|json` | `text` | Output format |
| `--expect CMD:VERDICT` | — | Assert expected verdict (repeatable); mismatch → exit 2 |
| `--output <PATH>` | stdout | Write output to file instead of stdout |

### Output fields (per command)

| Field | Description |
|-------|-------------|
| `command` | The original command string |
| `verdict` | `allow`, `ask`, or `deny` |
| `matching_rule` | Label of the rule that produced this verdict (see below) |
| `profile` | Effective profile (`safe`, `ask`, or `yolo`) |

### `matching_rule` sentinels

| Value | Meaning |
|-------|---------|
| `default-allow` | No rule matched; safe/yolo profile default |
| `default-ask` | No rule matched; ask profile default |
| `yolo-bypass` | Yolo profile (all commands bypass rule evaluation) |
| `ask-non-interactive` | Ask decision resolved to deny (non-interactive) |
| `cfg-allow-N` | Matched `extra_allow_patterns[N]` |
| `cfg-deny-N` | Matched `extra_deny_patterns[N]` |
| `<built-in-label>` | Matched a built-in dangerous-command corpus rule |

## Examples

### Basic check against default safe policy

```bash
max agent policy-check --command "ls -la" --command "rm -rf /"
```

Output:
```
command   verdict   matching_rule             profile
--------  -------   ----------------          -------
ls -la    allow     default-allow             safe
rm -rf /  deny      catastrophic-delete-root  safe
```

### Check a commands file

```bash
max agent policy-check --commands-file my-commands.txt
```

`my-commands.txt`:
```
# safe commands
ls -la
git status

# potentially dangerous
rm -rf /tmp/work
```

### Use a custom config

```toml
# policy-config.toml
[policy]
profile = "safe"
extra_deny_patterns = ["curl\\b", "wget\\b"]
extra_allow_patterns = ["rm\\s+-rf\\s+/tmp/"]
```

```bash
max agent policy-check \
  --config policy-config.toml \
  --commands-file corpus.txt
```

### JSON output for CI snapshot diffing

```bash
max agent policy-check \
  --commands-file corpus.txt \
  --format json > policy-verdicts.json
```

JSON schema:
```json
{
  "artifact_kind": "policy_check",
  "schema_version": "1.0",
  "profile": "safe",
  "verdicts": [
    {
      "command": "ls",
      "verdict": "allow",
      "matching_rule": "default-allow",
      "profile": "safe"
    }
  ],
  "mismatches": []
}
```

### CI regression guard with `--expect`

```bash
max agent policy-check \
  --commands-file corpus.txt \
  --expect "rm -rf /:deny" \
  --expect "ls:allow" \
  --expect "curl https://example.com:deny"
```

Any mismatch causes exit code 2 (`outcome_class: usage_error`).

### Pipe from stdin

```bash
echo "rm -rf /" | max agent policy-check --stdin
```

## Ask profile behaviour

`agent policy-check` is non-interactive by design. Under the `ask` profile, every command that would normally prompt for approval resolves to `deny` with the label `ask-non-interactive`. This matches the documented `check_command_non_interactive` contract.

## Guarantees

- **Zero network calls** — no model API is contacted.
- **Zero model calls** — deterministic regex evaluation only.
- **Zero file writes** unless `--output <PATH>` is provided.

## Related commands

- [`bench policy-impact`](spec-policy-impact.md): post-hoc analysis of policy impact on a completed sweep.
- [`agent redact-check`](spec-secret-redaction.md): similar preflight for the secret-redaction config.
- [`agent env preview`](spec-env-preview.md): environment config preflight.
