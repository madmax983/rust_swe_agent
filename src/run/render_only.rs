//! Zero-cost prompt preview: render the initial prompt surface without any
//! model call or trajectory write (issue #172).
//!
//! Exposes the exact initial turn the agent would send — system message,
//! first user message, tool list, hook config — along with a token estimate
//! and an upper-bound cost projection. No outbound network connection is made.

use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};
use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::prompt_guard::{PromptGuard, UntrustedKind};
use crate::template::Renderer;
use crate::tool::ToolRegistry;

/// Bytes-per-token approximation (same as the agent loop).
const BYTES_PER_TOKEN: u64 = 4;

/// Per-step growth estimate in tokens (output + next-turn input overhead).
const PER_STEP_GROWTH_TOKENS: u64 = 2_000;

/// USD per million input tokens for an unknown/generic model.
const FALLBACK_INPUT_USD_PER_MTOK: f64 = 3.0;

// ── Public output types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedTool {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedHook {
    pub name: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedHooks {
    pub pre_tool_use: Vec<RenderedHook>,
    pub post_tool_use: Vec<RenderedHook>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpperBoundCost {
    pub usd: f64,
    pub caveat: String,
}

/// Schema-versioned JSON artifact produced by `--render-only`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderOnlyReport {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    pub model: String,
    pub mode: String,
    pub system_message: String,
    pub user_message: String,
    pub tools: Vec<RenderedTool>,
    pub hooks: RenderedHooks,
    /// Estimated token count for the initial prompt (byte-based approximation).
    pub initial_prompt_tokens: u64,
    /// Model's nominal context window in tokens (heuristic lookup by model name).
    pub context_window_tokens: u64,
    /// Percentage of context window consumed by the initial prompt (0–100).
    pub context_window_pct: f64,
    pub upper_bound_cost: UpperBoundCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_workdir: Option<String>,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub struct RenderOnlyArgs {
    pub task: String,
    pub extra_context: Option<String>,
    pub config: Config,
    pub local_workdir: Option<std::path::PathBuf>,
    pub read_only: bool,
}

/// Render the initial agent turn without any model call or trajectory write.
///
/// Returns a `RenderOnlyReport` on success, or an `Error` when template
/// rendering or config validation fails.
#[allow(clippy::too_many_lines)]
pub fn render(args: RenderOnlyArgs) -> Result<RenderOnlyReport, Error> {
    let RenderOnlyArgs {
        task,
        extra_context,
        config,
        local_workdir,
        read_only,
    } = args;

    let renderer = Renderer::new();

    // Mirror the exact rendering path from `DefaultAgentBuilder`.
    let tool_registry = ToolRegistry::from_config_and_providers(&config.root.agent.tools, vec![])?;
    let prompt_tools = tool_registry.prompt_tools();

    let wrapped_task = PromptGuard::wrap(UntrustedKind::TaskText, &task);
    let wrapped_extra_context = extra_context
        .as_deref()
        .map(|ctx| PromptGuard::wrap(UntrustedKind::ExtraContext, ctx));

    let system_message = renderer.render_str(
        &config.root.prompts.system,
        &serde_json::json!({
            "task": wrapped_task,
            "extra_context": wrapped_extra_context,
            "tools": &prompt_tools,
        }),
    )?;

    let user_message = renderer.render_str(
        &config.root.prompts.instance,
        &serde_json::json!({
            "task": wrapped_task,
            "extra_context": wrapped_extra_context,
            "tools": &prompt_tools,
        }),
    )?;

    // Build the tool list as the model would see it.
    let tools: Vec<RenderedTool> = prompt_tools
        .iter()
        .map(|t| RenderedTool {
            name: t.name.clone(),
            description: t.description.clone(),
        })
        .collect();

    // Resolve hook configuration from the merged config.
    let hooks = RenderedHooks {
        pre_tool_use: config
            .root
            .agent
            .hooks
            .pre_tool_use
            .iter()
            .map(|h| RenderedHook {
                name: h.name.clone(),
                command: h.command.clone(),
                timeout_secs: h.timeout_secs,
            })
            .collect(),
        post_tool_use: config
            .root
            .agent
            .hooks
            .post_tool_use
            .iter()
            .map(|h| RenderedHook {
                name: h.name.clone(),
                command: h.command.clone(),
                timeout_secs: h.timeout_secs,
            })
            .collect(),
    };

    // Token estimate: byte length / BYTES_PER_TOKEN (same approximation as the
    // history bounding subsystem).
    let total_bytes = (system_message.len() + user_message.len()) as u64;
    let initial_prompt_tokens = (total_bytes / BYTES_PER_TOKEN).max(1);

    let model_name = &config.root.model.name;
    let context_window_tokens = context_window_for_model(model_name);

    #[allow(clippy::cast_precision_loss)]
    let context_window_pct =
        (initial_prompt_tokens as f64 / context_window_tokens as f64 * 100.0).min(100.0);

    let step_limit = u64::from(config.root.agent.step_limit);
    // Cumulative cost: every turn pays for the full prompt. Turn k has
    // initial_prompt_tokens + k*PER_STEP_GROWTH_TOKENS input tokens, so the
    // total across N turns is N*initial + N*(N-1)/2 * growth.
    // Use saturating arithmetic: u32::MAX step_limit still produces a finite
    // (capped) estimate rather than panicking in debug or wrapping in release.
    let sum_growth_tokens = (step_limit.saturating_mul(step_limit.saturating_sub(1)) / 2)
        .saturating_mul(PER_STEP_GROWTH_TOKENS);
    let upper_bound_tokens = step_limit
        .saturating_mul(initial_prompt_tokens)
        .saturating_add(sum_growth_tokens);
    let input_rate = input_usd_per_mtok(model_name);
    #[allow(clippy::cast_precision_loss)]
    let upper_bound_usd = upper_bound_tokens as f64 / 1_000_000.0 * input_rate;

    let upper_bound_cost = UpperBoundCost {
        usd: upper_bound_usd,
        caveat: format!(
            "Upper bound: Estimated total input cost over {step_limit} steps \
             ({step_limit} turns × {initial_prompt_tokens} initial + \
             {sum_growth_tokens} cumulative growth tokens) \
             × ${input_rate:.2}/Mtok. \
             Actual cost depends on agent behavior and output tokens."
        ),
    };

    Ok(RenderOnlyReport {
        artifact_kind: ArtifactKind::RenderOnly,
        schema_version: ArtifactSchemaVersion::CURRENT,
        model: model_name.clone(),
        mode: if read_only { "read_only" } else { "default" }.into(),
        system_message,
        user_message,
        tools,
        hooks,
        initial_prompt_tokens,
        context_window_tokens,
        context_window_pct,
        upper_bound_cost,
        local_workdir: local_workdir.map(|p| p.display().to_string()),
    })
}

/// Flags that are incompatible with `--render-only`.
///
/// Each entry is `(flag_name, is_set)`. The function returns the first
/// conflict found, which is enough for a clear error message.
#[allow(clippy::struct_excessive_bools)]
pub struct IncompatibleFlags<'a> {
    pub per_task_budget_usd: Option<f64>,
    pub task_timeout_secs: Option<u64>,
    /// `--stream <addr>` has no meaning without an agent loop.
    pub stream: Option<&'a str>,
    /// `--verify NAME:CMD` checks run after execution; meaningless without one.
    pub has_verify_checks: bool,
    /// `--open-pr` / `--open-prs` publishes a patch that can only exist after a run.
    pub open_pr: bool,
    /// `--github-pr-dry-run` is a PR-publishing mode that requires a completed trajectory.
    pub pr_dry_run: bool,
    /// `--webhook-url` pushes streaming events during a run; meaningless without one.
    pub webhook_url: bool,
    /// `--webhook-header` configures headers for webhook push; meaningless without a run.
    pub webhook_headers: bool,
}

/// Validate that `--render-only` is not combined with execution-time flags
/// that have no meaning without an actual agent run.
pub fn reject_incompatible_flags(flags: &IncompatibleFlags<'_>) -> Result<(), Error> {
    let conflicts: &[(&str, bool)] = &[
        ("--per-task-budget-usd", flags.per_task_budget_usd.is_some()),
        ("--task-timeout-secs", flags.task_timeout_secs.is_some()),
        ("--stream", flags.stream.is_some()),
        ("--verify", flags.has_verify_checks),
        ("--open-pr / --open-prs", flags.open_pr),
        ("--github-pr-dry-run", flags.pr_dry_run),
        ("--webhook-url", flags.webhook_url),
        ("--webhook-header", flags.webhook_headers),
    ];
    for (name, set) in conflicts {
        if *set {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "--render-only is mutually exclusive with {name}; \
                 the flag has no meaning without an agent run"
            ))));
        }
    }
    Ok(())
}

// ── Formatting helpers ────────────────────────────────────────────────────────

/// Render the report in human-readable text form.
pub fn format_text(report: &RenderOnlyReport) -> String {
    let mut out = String::new();
    out.push_str("=== render-only preview (no model call made) ===\n\n");

    let mut overview_table = Table::new();
    overview_table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Property", "Value"])
        .add_row(vec!["Model", &report.model])
        .add_row(vec!["Mode", &report.mode]);
    if let Some(ref wd) = report.local_workdir {
        overview_table.add_row(vec!["Local workdir", wd]);
    }
    out.push_str(&overview_table.to_string());
    out.push_str("\n\n");

    out.push_str("--- System message ---\n");
    out.push_str(&report.system_message);
    out.push_str("\n\n--- User message (instance prompt) ---\n");
    out.push_str(&report.user_message);
    out.push_str("\n\n--- Registered tools ---\n");

    if report.tools.is_empty() {
        out.push_str("  (no tools registered)\n");
    } else {
        let mut tools_table = Table::new();
        tools_table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec!["Tool", "Description"]);
        for t in &report.tools {
            tools_table.add_row(vec![&t.name, &t.description]);
        }
        out.push_str(&tools_table.to_string());
        out.push('\n');
    }

    out.push_str("\n--- Hook configuration ---\n");
    if report.hooks.pre_tool_use.is_empty() && report.hooks.post_tool_use.is_empty() {
        out.push_str("  (no hooks configured)\n");
    } else {
        let mut hooks_table = Table::new();
        hooks_table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec!["Phase", "Name", "Command"]);
        for h in &report.hooks.pre_tool_use {
            hooks_table.add_row(vec!["PreToolUse", &h.name, &h.command]);
        }
        for h in &report.hooks.post_tool_use {
            hooks_table.add_row(vec!["PostToolUse", &h.name, &h.command]);
        }
        out.push_str(&hooks_table.to_string());
        out.push('\n');
    }

    out.push_str("\n--- Estimates ---\n");
    let mut estimates_table = Table::new();
    estimates_table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Metric", "Value"])
        .add_row(vec![
            "Initial Prompt Tokens",
            &format!(
                "{} ({:.2}% of {}-token window)",
                report.initial_prompt_tokens,
                report.context_window_pct,
                report.context_window_tokens
            ),
        ])
        .add_row(vec![
            "Upper-bound Cost (USD)",
            &format!(
                "${:.6}\nNote: {}",
                report.upper_bound_cost.usd, report.upper_bound_cost.caveat
            ),
        ]);
    out.push_str(&estimates_table.to_string());
    out.push('\n');

    out
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Heuristic context-window size by model name prefix.
fn context_window_for_model(model: &str) -> u64 {
    let name = model.rsplit('/').next().unwrap_or(model);
    if name.starts_with("claude-") {
        200_000
    } else if name.starts_with("gpt-4") {
        128_000
    } else if name.starts_with("gpt-3.5") {
        16_385
    } else if name.starts_with("gemini-") {
        1_000_000
    } else {
        128_000
    }
}

/// Heuristic input USD per 1M tokens for the named model.
fn input_usd_per_mtok(model: &str) -> f64 {
    let name = model.rsplit('/').next().unwrap_or(model);
    if name.starts_with("claude-opus") {
        15.0
    } else if name.starts_with("claude-sonnet") {
        3.0
    } else if name.starts_with("claude-haiku") {
        0.25
    } else if name.starts_with("claude-") {
        3.0
    } else if name.starts_with("gpt-4o-mini") {
        0.15
    } else if name.starts_with("gpt-4") {
        2.5
    } else {
        FALLBACK_INPUT_USD_PER_MTOK
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn default_args() -> RenderOnlyArgs {
        RenderOnlyArgs {
            task: "fix the bug".into(),
            extra_context: None,
            config: Config::defaults().unwrap(),
            local_workdir: None,
            read_only: false,
        }
    }

    #[test]
    fn render_returns_ok_for_valid_config() {
        let report = render(default_args()).unwrap();
        assert_eq!(report.artifact_kind, ArtifactKind::RenderOnly);
        assert_eq!(report.schema_version, ArtifactSchemaVersion::CURRENT);
    }

    #[test]
    fn render_tools_includes_bash() {
        let report = render(default_args()).unwrap();
        assert!(
            report.tools.iter().any(|t| t.name == "bash"),
            "tools must include bash; got: {:?}",
            report.tools
        );
    }

    #[test]
    fn render_initial_prompt_tokens_positive() {
        let report = render(default_args()).unwrap();
        assert!(report.initial_prompt_tokens > 0);
    }

    #[test]
    fn render_context_window_pct_in_valid_range() {
        let report = render(default_args()).unwrap();
        assert!(
            (0.0..=100.0).contains(&report.context_window_pct),
            "pct out of range: {}",
            report.context_window_pct
        );
    }

    #[test]
    fn render_user_message_contains_task() {
        let mut args = default_args();
        args.task = "a unique task string XYZ987".into();
        let report = render(args).unwrap();
        assert!(
            report.user_message.contains("XYZ987") || report.system_message.contains("XYZ987"),
            "task text must appear in rendered output"
        );
    }

    #[test]
    fn render_user_message_contains_extra_context() {
        let mut args = default_args();
        args.extra_context = Some("EXTRA_ABC_123".into());
        let report = render(args).unwrap();
        assert!(
            report.user_message.contains("EXTRA_ABC_123"),
            "extra context must appear in user_message;\nuser_message: {}",
            report.user_message
        );
    }

    fn clean_flags() -> IncompatibleFlags<'static> {
        IncompatibleFlags {
            per_task_budget_usd: None,
            task_timeout_secs: None,
            stream: None,
            has_verify_checks: false,
            open_pr: false,
            pr_dry_run: false,
            webhook_url: false,
            webhook_headers: false,
        }
    }

    #[test]
    fn reject_incompatible_flags_budget_err() {
        let flags = IncompatibleFlags {
            per_task_budget_usd: Some(1.0),
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_timeout_err() {
        let flags = IncompatibleFlags {
            task_timeout_secs: Some(60),
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_stream_err() {
        let flags = IncompatibleFlags {
            stream: Some("127.0.0.1:7878"),
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_verify_err() {
        let flags = IncompatibleFlags {
            has_verify_checks: true,
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_open_pr_err() {
        let flags = IncompatibleFlags {
            open_pr: true,
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_pr_dry_run_err() {
        let flags = IncompatibleFlags {
            pr_dry_run: true,
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_webhook_url_err() {
        let flags = IncompatibleFlags {
            webhook_url: true,
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_webhook_headers_err() {
        let flags = IncompatibleFlags {
            webhook_headers: true,
            ..clean_flags()
        };
        assert!(reject_incompatible_flags(&flags).is_err());
    }

    #[test]
    fn reject_incompatible_flags_all_clear_ok() {
        assert!(reject_incompatible_flags(&clean_flags()).is_ok());
    }

    #[test]
    fn context_window_claude_is_200k() {
        assert_eq!(context_window_for_model("claude-opus-4-7"), 200_000);
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 200_000);
    }

    #[test]
    fn context_window_gpt4_is_128k() {
        assert_eq!(context_window_for_model("gpt-4o"), 128_000);
    }

    #[test]
    fn broken_template_returns_err() {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.prompts.system = "{{ unclosed".into();
        let result = render(RenderOnlyArgs {
            task: "test".into(),
            extra_context: None,
            config: cfg,
            local_workdir: None,
            read_only: false,
        });
        assert!(result.is_err(), "broken template must return Err");
    }

    #[test]
    fn format_text_contains_key_sections() {
        let report = render(default_args()).unwrap();
        let text = format_text(&report);
        assert!(text.contains("System message"));
        assert!(text.contains("User message"));
        assert!(text.contains("Registered tools"));
        assert!(text.contains("Estimates"));
        assert!(text.contains("Upper-bound Cost"));
    }
}
