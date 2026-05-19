# CLI Task File Input Specification

This document defines the specification for the `--task-file` feature added to the `mini` subcommand. The goal is to allow passing multi-line task descriptions containing quotes, backticks, shell variables, and Unicode characters to the agent loop without shell escaping complexities.

## Overview

Historically, specifying a long or complex task prompt via the `--task` CLI flag required complex shell escaping (e.g., nesting quotes or escapes in Bash or PowerShell). This issue is resolved by introducing `--task-file`, which reads the task text directly from a file or standard input.

## Command-Line Arguments

The `mini` subcommand accepts the following mutually exclusive task flags:

- `--task <string>`: The task prompt text supplied directly via the command line.
- `--task-file <path>`: The path to a file containing the task prompt text, or `-` to read from standard input (`stdin`).

### Mutual Exclusion

The `--task` and `--task-file` flags are strictly mutually exclusive.
- If **both** `--task` and `--task-file` are provided, the CLI must immediately exit with status code `2` (Config/Usage error) and output a clear error message naming both flags and explaining that they are mutually exclusive.
- If **neither** is provided, the CLI must immediately exit with status code `2` naming that at least one of these task sources is required.

## Input Handling and Validation

When `--task-file <path>` is specified:

1. **Path Existence**:
   - If the specified file does not exist, is unreadable, or is a directory, the CLI must fail and exit with status code `2` naming the nonexistent or unreadable path.

2. **Standard Input Sentinel (`-`)**:
   - If the path is `-`, the CLI reads from `stdin` until EOF.
   - Reading must be done sequentially, and any read error must exit with status code `2`.

3. **Empty Source Validation**:
   - If the loaded task content (from either the file or `stdin`) is empty or consists entirely of whitespace, the CLI must fail and exit with status code `2` naming the empty task source.

4. **Unicode and Byte-Identity**:
   - The file/stdin stream must be decoded as UTF-8.
   - If a UTF-8 Byte Order Mark (BOM, `\u{FEFF}`) exists at the very beginning of the loaded text, it must be cleanly stripped.
   - Beyond BOM stripping, all characters (including single/double quotes, backticks, newlines, `$VAR` references, and emoji/Unicode code points) must be preserved in their raw form. The content is passed down to the agent loop byte-identically with no other trimming, escaping, or alteration.

5. **Trajectory Recording**:
   - The recorded trajectory's task field must be byte-identical to the resolved task (matching SHA-256 of the raw source).

## Zero-Cost Render Mode

`--task-file` integrates seamlessly with the `--render-only` dry-run preview.
- Running `mini --render-only --task-file <path>` loads, validates, and renders the system message, first user message, tool list, and hook config at $0 model/network cost.
