//! `bench watch`: follow a single in-flight trajectory instance live.

use std::io::{IsTerminal as _, Write as StdWrite};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::Error;
use crate::redaction::Redactor;
use crate::run::inspect::{
    InspectStep, build_inspect_steps_with_max, redact_trajectory_for_inspect, render_step_text,
    resolve_trajectory_path,
};
use crate::trajectory::Trajectory;

const DEFAULT_MAX_BYTES: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const NDJSON_SCHEMA_VERSION: &str = "watch-1.0";

pub struct WatchArgs {
    pub sweep: PathBuf,
    pub instance: String,
    pub wait_secs: u64,
    pub stall_secs: u64,
    pub full: bool,
    pub max_bytes: Option<usize>,
    pub ndjson: bool,
}

#[derive(Serialize)]
struct WatchTurnEvent<'a> {
    schema_version: &'static str,
    instance_id: &'a str,
    turn_index: usize,
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<&'a str>,
}

pub async fn run(args: &WatchArgs) -> Result<(), Error> {
    if !args.sweep.is_dir() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "watch: sweep path is not a directory: {}",
            args.sweep.display()
        ))));
    }

    let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let wait_deadline = Instant::now() + Duration::from_secs(args.wait_secs);
    let stall_duration = Duration::from_secs(args.stall_secs);
    let color = !args.ndjson && std::io::stdout().is_terminal();
    let redactor = Redactor::default_enabled();

    let mut stdout = std::io::stdout();
    let mut emitted_steps: usize = 0;
    let mut last_activity_at: Option<Instant> = None;
    let mut stall_warned = false;
    let mut last_wait_progress_at = Instant::now();
    let mut cached_path: Option<PathBuf> = None;
    let mut last_file_len: u64 = 0;

    loop {
        let traj_path = cached_path
            .clone()
            .or_else(|| resolve_trajectory_path(&args.sweep, &args.instance));

        let Some(traj_path) = traj_path else {
            if Instant::now() >= wait_deadline {
                return Err(Error::Trajectory(format!(
                    "watch: trajectory file for instance `{}` not found after {} second(s)",
                    args.instance, args.wait_secs
                )));
            }
            if last_wait_progress_at.elapsed() >= WAIT_PROGRESS_INTERVAL {
                eprintln!(
                    "[watch] waiting for trajectory file for `{}`...",
                    args.instance
                );
                last_wait_progress_at = Instant::now();
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        };

        // Initialize activity timer and cache path on first discovery.
        if cached_path.is_none() {
            cached_path = Some(traj_path.clone());
            last_activity_at = Some(Instant::now());
        }

        // Skip read if file size is unchanged (avoids O(N) read every poll).
        let current_len = tokio::fs::metadata(&traj_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if current_len == last_file_len && emitted_steps > 0 {
            maybe_warn_stall(last_activity_at.as_ref(), &mut stall_warned, stall_duration);
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let Ok(text) = tokio::fs::read_to_string(&traj_path).await else {
            maybe_warn_stall(last_activity_at.as_ref(), &mut stall_warned, stall_duration);
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        };

        let Ok(mut traj) = serde_json::from_str::<Trajectory>(&text) else {
            maybe_warn_stall(last_activity_at.as_ref(), &mut stall_warned, stall_duration);
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        };

        last_file_len = current_len;
        redact_trajectory_for_inspect(&mut traj, &redactor);
        let steps = build_inspect_steps_with_max(&traj, args.full, max_bytes);

        // Handle the case where the file was rewritten with fewer steps (worker restart).
        if steps.len() < emitted_steps {
            emitted_steps = 0;
        }

        if steps.len() > emitted_steps {
            emit_new_steps(&steps[emitted_steps..], args, &mut stdout, color)?;
            emitted_steps = steps.len();
            last_activity_at = Some(Instant::now());
            stall_warned = false;
        } else {
            maybe_warn_stall(last_activity_at.as_ref(), &mut stall_warned, stall_duration);
        }

        if is_terminal_outcome(&traj) {
            if !args.ndjson {
                let traj_outcome = traj.info.outcome.as_deref().unwrap_or("?");
                let cost = traj
                    .info
                    .total_cost_usd
                    .map_or_else(|| "?".into(), |c| format!("{c:.4}"));
                let steps_count = traj.info.steps.unwrap_or(0);
                writeln!(
                    stdout,
                    "\n[watch] instance `{}` complete: outcome={} steps={} cost_usd={}",
                    args.instance, traj_outcome, steps_count, cost
                )?;
                stdout.flush()?;
            }
            return Ok(());
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn emit_new_steps(
    steps: &[InspectStep],
    args: &WatchArgs,
    stdout: &mut std::io::Stdout,
    color: bool,
) -> Result<(), Error> {
    for step in steps {
        if args.ndjson {
            let event = WatchTurnEvent {
                schema_version: NDJSON_SCHEMA_VERSION,
                instance_id: &args.instance,
                turn_index: step.index,
                role: &step.role,
                message: step.message.as_deref(),
                bash: step.bash.as_deref(),
                exit_code: step.exit_code,
                stdout: step.stdout.as_deref(),
                stderr: step.stderr.as_deref(),
            };
            let line = serde_json::to_string(&event)?;
            writeln!(stdout, "{line}")?;
        } else {
            write!(stdout, "{}", render_step_text(step, color))?;
        }
        stdout.flush()?;
    }
    Ok(())
}

fn maybe_warn_stall(last_activity: Option<&Instant>, stall_warned: &mut bool, duration: Duration) {
    let Some(last) = last_activity else { return };
    if !*stall_warned && last.elapsed() >= duration {
        eprintln!("[stalled: no new turns in {}s]", last.elapsed().as_secs());
        *stall_warned = true;
    }
}

fn is_terminal_outcome(traj: &Trajectory) -> bool {
    traj.info.outcome.is_some() || traj.info.exit_reason.is_some()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::trajectory::outcome;

    #[test]
    fn terminal_when_outcome_submitted() {
        let mut traj = Trajectory::new();
        traj.info.outcome = Some(outcome::SUBMITTED.into());
        assert!(is_terminal_outcome(&traj));
    }

    #[test]
    fn terminal_when_outcome_error() {
        let mut traj = Trajectory::new();
        traj.info.outcome = Some(outcome::ERROR.into());
        assert!(is_terminal_outcome(&traj));
    }

    #[test]
    fn not_terminal_when_no_outcome_and_no_exit_reason() {
        let traj = Trajectory::new();
        assert!(!is_terminal_outcome(&traj));
    }

    #[test]
    fn terminal_when_any_exit_reason_set() {
        let mut traj = Trajectory::new();
        traj.info.exit_reason = Some("any_reason".into());
        assert!(is_terminal_outcome(&traj));
    }

    #[test]
    fn terminal_when_outcome_and_exit_reason_both_set() {
        let mut traj = Trajectory::new();
        traj.info.outcome = Some(outcome::SUBMITTED.into());
        traj.info.exit_reason = Some("cancelled".into());
        assert!(is_terminal_outcome(&traj));
    }
}
