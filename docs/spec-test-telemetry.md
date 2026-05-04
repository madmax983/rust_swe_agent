# Test Command Telemetry

`rust-swe-agent` records recognized test commands from assistant action text in
trajectory `info.test_invocations`. Detection uses the assistant's issued bash
command, not the observation text, because observations can mention test
commands in logs, scripts, or error output without proving that the agent chose
to run them. Detection is anchored to shell command starts and shell segments
after separators such as `&&`, `||`, `|`, `;`, and newlines. This intentionally
detects commands like `cd repo && pytest -q` and `echo "data" | pytest -q` while
ignoring quoted mentions such as `echo "run pytest"`.

Each invocation records:

- `step_index`: agent step index that issued the command.
- `command`: full action command text.
- `exit_code`: command exit code.
- `matched_pattern`: built-in or configured pattern that matched.

Trajectory `info` also carries:

- `tests_run_before_submit`: true when a recognized test command ran before a
  submit action.
- `last_tests_passed`: exit status of the most recent recognized pre-submit
  test, or null when none exists.

## Default Patterns

The built-in closed pattern list is:

- `pytest`
- `python -m pytest`
- `python -m unittest`
- `tox`
- `nox`
- `make test`
- `make check`
- `cargo test`
- `go test`
- `npm test`
- `npm run test`
- `yarn test`
- `mvn test`
- `mvn -Dtest=`
- `gradle test`
- `./gradlew test`

## Configuration

Add project-specific command regexes with:

```toml
[agent]
test_command_patterns = ["project-(check|test)"]
```

To replace the built-in list entirely:

```toml
[agent]
test_command_patterns_replace = true
test_command_patterns = ["project-(check|test)"]
```

Configured regexes extend the built-in list by default. They are validated when
configuration is loaded and compiled once during agent initialization. They are
still evaluated only at a command segment start, so a regex that matches inside
quoted text or in the middle of another command does not count.

## Behavioral Metrics

`bench evaluate` writes these metrics to `evaluation.json.behavioral`:

- `tests_run_before_submit_rate`: submitted instances with
  `tests_run_before_submit = true` divided by all submitted instances.
- `resolved_rate_when_tests_run`: resolved submitted instances among submitted
  instances that ran a recognized test command before submit.
- `resolved_rate_when_tests_skipped`: resolved submitted instances among
  submitted instances that did not run a recognized test command before submit.

The default pattern list is a compatibility contract. Adding a default pattern is
a minor schema bump; removing one is a breaking schema change.
