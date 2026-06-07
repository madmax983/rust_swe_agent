# Session-Scoped Auto-Approve Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement session-scoped auto-approve rules for interactive mode commands sharing a scope (e.g. program token or tool name).

**Architecture:** Extend `ConfirmDecision` with an `AutoApprove(String)` variant. Store active rules in `DefaultAgent`, check them after policy/hooks gates but before prompting. Propagate rule creation via a new `StreamEvent::AutoApproveRuleCreated` to decouple agent state from the `RatatuiDashboard`.

**Tech Stack:** Rust, Tokio, Crossterm, Ratatui

---

### Task 1: Extend `ConfirmDecision` & Add Scope Extraction

**Files:**
- Modify: `src/agent/confirm.rs`
- Test: `src/agent/confirm.rs`

**Step 1: Write the failing test**

Add a test that verifies `ConfirmContext::derive_scope` returns the correct scope for bash and non-bash tools, and that `ConfirmDecision::AutoApprove` is a valid variant with a correct label.

```rust
#[test]
fn confirm_context_derives_correct_scope() {
    let ctx_bash = ConfirmContext {
        tool_name: "bash".into(),
        command: "cargo test --all".into(),
        step: 0,
        step_limit: 10,
        cost_usd: 0.0,
        cache_marker: "cache:auto-or-none",
    };
    assert_eq!(ctx_bash.derive_scope(), "cargo");

    let ctx_tool = ConfirmContext {
        tool_name: "read_file".into(),
        command: "src/lib.rs".into(),
        step: 0,
        step_limit: 10,
        cost_usd: 0.0,
        cache_marker: "cache:auto-or-none",
    };
    assert_eq!(ctx_tool.derive_scope(), "read_file");
}

#[test]
fn auto_approve_decision_has_label() {
    let d = ConfirmDecision::AutoApprove("cargo".to_string());
    assert_eq!(d.label(), "auto-approve");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib agent::confirm::tests`
Expected: FAIL due to compilation error (no `AutoApprove` variant, no `derive_scope` method).

**Step 3: Write minimal implementation**

Modify `ConfirmDecision` enum in `src/agent/confirm.rs`:
```rust
pub enum ConfirmDecision {
    Approve,
    Reject(Option<String>),
    Abort,
    Edit(String),
    AutoApprove(String),
}
```
Update `ConfirmDecision::label` implementation:
```rust
impl ConfirmDecision {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject(_) => "reject",
            Self::Abort => "abort",
            Self::Edit(_) => "edit",
            Self::AutoApprove(_) => "auto-approve",
        }
    }
}
```
And add `derive_scope` to `ConfirmContext` in `src/agent/confirm.rs`:
```rust
impl ConfirmContext {
    #[must_use]
    pub fn derive_scope(&self) -> String {
        if self.tool_name == "bash" {
            self.command
                .split_whitespace()
                .next()
                .unwrap_or(&self.tool_name)
                .to_string()
        } else {
            self.tool_name.clone()
        }
    }
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test --lib agent::confirm::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add src/agent/confirm.rs
git commit -m "feat: add ConfirmDecision::AutoApprove and scope derivation"
```

---

### Task 2: Implement CLI Prompt Keys and Line Parsing

**Files:**
- Modify: `src/agent/confirm_cli.rs`
- Test: `src/agent/confirm_cli.rs`

**Step 1: Write the failing test**

Add tests to verify keystroke mapping and line parsing for `AutoApprove`:

```rust
#[test]
fn parse_line_decision_matches_auto_approve() {
    assert_eq!(parse_line_decision("A"), ConfirmDecision::AutoApprove("".to_string()));
    assert_eq!(parse_line_decision("auto"), ConfirmDecision::AutoApprove("".to_string()));
}

#[test]
fn key_event_to_decision_maps_a_to_auto_approve() {
    let key = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
    assert!(matches!(key_event_to_decision(key), Some(ConfirmDecision::AutoApprove(_))));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib agent::confirm_cli::tests`
Expected: FAIL

**Step 3: Write minimal implementation**

In `src/agent/confirm_cli.rs`, update:
1. `render_banner` to show the derived scope in the options banner:
   ```rust
   let scope = ctx.derive_scope();
   format!(
       "\n[interactive] step {}/{}  cost ${:.4}  {}\n\
        [interactive] tool: {}\n\
        [interactive] command:\n{}\n\
        [interactive] (y)approve / (n)reject / (e)edit / (a)abort / (A)auto-approve {}? ",
       ctx.step,
       ctx.step_limit,
       ctx.cost_usd,
       ctx.cache_marker,
       ctx.tool_name,
       indent_command(&ctx.command),
       scope,
   )
   ```
2. Update `key_event_to_decision`:
   ```rust
   match key.code {
       KeyCode::Char('y' | 'Y') => Some(ConfirmDecision::Approve),
       KeyCode::Char('n' | 'N') => Some(ConfirmDecision::Reject(None)),
       KeyCode::Char('a') | KeyCode::Esc => Some(ConfirmDecision::Abort),
       KeyCode::Char('A') => {
           // We'll pass an empty scope placeholder or the derived scope.
           // Since `key_event_to_decision` doesn't have `ctx`, we can return
           // `ConfirmDecision::AutoApprove(String::new())` and populate it in `read_single_keystroke`.
           Some(ConfirmDecision::AutoApprove(String::new()))
       }
       _ => None,
   }
   ```
3. Update `read_single_keystroke` to inject the correct scope when `ConfirmDecision::AutoApprove` is returned:
   ```rust
   if let Some(ConfirmDecision::AutoApprove(_)) = d {
       break ConfirmDecision::AutoApprove(ctx.derive_scope());
   }
   ```
4. Update `parse_line_decision`:
   ```rust
   pub fn parse_line_decision(input: &str) -> ConfirmDecision {
       let trimmed = input.trim().to_ascii_lowercase();
       match trimmed.as_str() {
           "y" | "yes" => ConfirmDecision::Approve,
           "n" | "no" => ConfirmDecision::Reject(None),
           "a" | "abort" => ConfirmDecision::Abort,
           // Note: Since `parse_line_decision` does not have access to the context,
           // returning AutoApprove("") is sufficient. In production we'd populate
           // the scope in the caller prompt_blocking.
           "a_upper" | "a" | "auto" if input.trim().chars().next().is_some_and(|c| c.is_uppercase()) || input.trim() == "auto" => {
               ConfirmDecision::AutoApprove(String::new())
           }
           _ => ConfirmDecision::Abort,
       }
   }
   ```
   Wait, let's implement `parse_line_decision` cleanly:
   ```rust
   pub fn parse_line_decision(input: &str) -> ConfirmDecision {
       let trimmed = input.trim().to_ascii_lowercase();
       match trimmed.as_str() {
           "y" | "yes" => ConfirmDecision::Approve,
           "n" | "no" => ConfirmDecision::Reject(None),
           "a" | "abort" => ConfirmDecision::Abort,
           "auto" => ConfirmDecision::AutoApprove(String::new()),
           _ => {
               // If it's a single 'A' or 'a' but input was uppercase 'A'
               if input.trim() == "A" {
                   ConfirmDecision::AutoApprove(String::new())
               } else {
                   ConfirmDecision::Abort
               }
           }
       }
   }
   ```
   And in `prompt_blocking`, if `decision` is `ConfirmDecision::AutoApprove(ref mut s)` and `s.is_empty()`, we set it to `ctx.derive_scope()`.

**Step 4: Run test to verify it passes**

Run: `cargo test --lib agent::confirm_cli::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add src/agent/confirm_cli.rs
git commit -m "feat: implement CLI keys and parsing for auto-approve"
```

---

### Task 3: Implement `StreamEvent` and `DefaultAgent` Integration

**Files:**
- Modify: `src/stream/mod.rs`, `src/agent/default.rs`
- Test: `src/agent/default.rs`

**Step 1: Write the failing test**

Add tests in `src/agent/default.rs` that verify:
1. `DefaultAgent` bypasses the confirmation callback when a command/tool matching an active auto-approve rule is run.
2. Trajectory records the `interactive_decision` as `"auto-approve"`.
3. Auto-approve rules are session-scoped (only in memory).

```rust
// We will write these tests in src/agent/default.rs tests module.
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib agent::default::tests`
Expected: FAIL

**Step 3: Write minimal implementation**

1. In `src/stream/mod.rs`, add `AutoApproveRuleCreated` to `StreamEvent`:
   ```rust
   pub enum StreamEvent {
       ...
       #[serde(rename = "auto_approve_rule_created")]
       AutoApproveRuleCreated {
           scope: String,
       },
   }
   ```
   Update `event_name()`:
   ```rust
   Self::AutoApproveRuleCreated { .. } => "auto_approve_rule_created",
   ```
2. In `DefaultAgent` struct (`src/agent/default.rs`), add `auto_approve_rules: std::sync::Mutex<std::collections::HashSet<String>>`. Initialize it in `DefaultAgent::new` / builders.
3. In `DefaultAgent::confirm_operator_action`:
   - If the current command matches a rule in `auto_approve_rules`, return `Some(ConfirmDecision::AutoApprove(scope))`.
4. In `DefaultAgent::run_step` (around line 1262):
   - Handle the returned `decision`:
     - If it is `ConfirmDecision::AutoApprove(scope)`:
       - If the scope was *not* already in `auto_approve_rules`, insert it, and emit `StreamEvent::AutoApproveRuleCreated { scope }`.
       - Record this in trajectory/metadata as `interactive_decision: "auto-approve"`.
       - Execute the command immediately.

**Step 4: Run test to verify it passes**

Run: `cargo test --lib agent::default::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add src/stream/mod.rs src/agent/default.rs
git commit -m "feat: integrate auto-approve rule checks and stream events in DefaultAgent"
```

---

### Task 4: Implement TUI Dashboard Rule Display and Modal interaction

**Files:**
- Modify: `src/agent/confirm_tui.rs`
- Test: `src/agent/confirm_tui.rs`

**Step 1: Write the failing test**

Write a test verifying that `RatatuiDashboard` displays the modal keys for auto-approve, accepts `A` to trigger auto-approve, updates its list of active rules upon receiving `StreamEvent::AutoApproveRuleCreated`, and displays the active rules in the dashboard.

**Step 2: Run test to verify it fails**

Run: `cargo test --lib agent::confirm_tui::tests`
Expected: FAIL

**Step 3: Write minimal implementation**

1. In `confirm_tui.rs`, add `active_rules: Vec<String>` to `DashboardState` and `DashboardSnapshot`.
2. Update `StreamSink::emit` for `RatatuiDashboard`:
   - On `StreamEvent::AutoApproveRuleCreated { scope }`, add it to `active_rules` in `DashboardState`.
3. In `handle_key_normal`, add mapping for `KeyCode::Char('A')` / `A` keypress:
   - When pressed, send `ConfirmDecision::AutoApprove(pending.ctx.derive_scope())` to the responder.
4. Update `draw_modal` to show `(A) auto-approve <scope>` as an option in the prompt modal.
5. Display active rules list in the dashboard (e.g. in `header_paragraph` or `footer_paragraph`). Let's render it in a clean styling.

**Step 4: Run test to verify it passes**

Run: `cargo test --lib agent::confirm_tui::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add src/agent/confirm_tui.rs
git commit -m "feat: implement auto-approve in TUI dashboard"
```

---

### Task 5: End-to-End Verification and PR/Doc Verification

**Files:**
- Test: `tests/interactive_auto_approve.rs` (new integration test)

**Step 1: Write the failing test**
Create a new integration test exercising `DefaultAgent` with interactive mode, simulating user input of `A` for `cargo` command, and verifying that the next `cargo` commands are executed automatically without prompts while other commands (e.g., `git` or custom tools) still prompt.

**Step 2: Run test to verify it fails**
Run: `cargo test --test interactive_auto_approve`
Expected: FAIL or Not Found

**Step 3: Write minimal implementation / tests**
Implement the integration test.

**Step 4: Run test to verify it passes**
Run: `cargo test --test interactive_auto_approve`
Expected: PASS

**Step 5: Commit**
```bash
git add tests/interactive_auto_approve.rs
git commit -m "test: add integration test for interactive mode auto-approve"
```
