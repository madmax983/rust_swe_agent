//! `bench watch`: follow a single in-flight trajectory instance live.

use std::io::{IsTerminal as _, Write as StdWrite};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::Error;
use crate::redaction::Redactor;
use crate::run::inspect::{
    InspectStep, build_inspect_steps_with_max, redact_trajectory_for_inspect, render_step_text,
};
use crate::trajectory::Trajectory;

const DEFAULT_MAX_BYTES: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const NDJSON_SCHEMA_VERSION: &str = "watch-1.0";

pub struct WatchArgs {
    pub sweep: PathBuf,
    pub instance: String,
    pub run_index: u32,
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
    let mut last_file_mtime: Option<SystemTime> = None;

    loop {
        let traj_path = cached_path
            .clone()
            .or_else(|| resolve_watch_path(&args.sweep, &args.instance, args.run_index));

        let Some(traj_path) = traj_path else {
            if Instant::now() >= wait_deadline {
                return Err(Error::Trajectory(format!(
                    "watch: trajectory file for instance `{}` (run {}) not found after {} second(s)",
                    args.instance, args.run_index, args.wait_secs
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

        // Skip read when both size and mtime are unchanged (avoids O(N) read every poll).
        // When mtime is unavailable (rare filesystems) we always re-read for correctness.
        let (current_len, current_mtime) = match tokio::fs::metadata(&traj_path).await {
            Ok(m) => (m.len(), m.modified().ok()),
            Err(_) => (0, None),
        };
        let skippable = emitted_steps > 0
            && current_len == last_file_len
            && current_mtime.is_some()
            && current_mtime == last_file_mtime;
        if skippable {
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
        last_file_mtime = current_mtime;
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
            print_completion_summary(args, &traj, &mut stdout)?;
            return Ok(());
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Resolve the trajectory file path for a watched instance.
///
/// For run_index > 1, only the nested `<sweep>/<instance>/run-N.traj.json` path is
/// checked. For run_index == 1 the legacy flat layout is also accepted so that
/// hello-world and pre-rerun sweep outputs work without a `--run-index` flag.
fn resolve_watch_path(sweep: &Path, instance_id: &str, run_index: u32) -> Option<PathBuf> {
    let nested = sweep
        .join(instance_id)
        .join(format!("run-{run_index}.traj.json"));
    if nested.exists() {
        return Some(nested);
    }
    if run_index == 1 {
        let flat = sweep.join(format!("{instance_id}.traj.json"));
        if flat.exists() {
            return Some(flat);
        }
    }
    None
}

fn print_completion_summary(
    args: &WatchArgs,
    traj: &Trajectory,
    stdout: &mut std::io::Stdout,
) -> Result<(), Error> {
    if args.ndjson {
        return Ok(());
    }
    let outcome = traj.info.outcome.as_deref().unwrap_or("?");
    let cost = traj
        .info
        .total_cost_usd
        .map_or_else(|| "?".into(), |c| format!("{c:.4}"));
    let steps = traj.info.steps.unwrap_or(0);
    writeln!(
        stdout,
        "\n[watch] instance `{}` complete: outcome={} steps={} cost_usd={}",
        args.instance, outcome, steps, cost
    )?;
    stdout.flush()?;
    Ok(())
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

    #[test]
    fn resolve_watch_path_finds_nested_run1() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("my-instance");
        std::fs::create_dir_all(&instance_dir).unwrap();
        std::fs::write(instance_dir.join("run-1.traj.json"), "{}").unwrap();
        let p = resolve_watch_path(dir.path(), "my-instance", 1).unwrap();
        assert!(p.ends_with("run-1.traj.json"));
    }

    #[test]
    fn resolve_watch_path_finds_flat_for_run1() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my-instance.traj.json"), "{}").unwrap();
        let p = resolve_watch_path(dir.path(), "my-instance", 1).unwrap();
        assert!(p.ends_with("my-instance.traj.json"));
    }

    #[test]
    fn resolve_watch_path_finds_run2() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("my-instance");
        std::fs::create_dir_all(&instance_dir).unwrap();
        std::fs::write(instance_dir.join("run-2.traj.json"), "{}").unwrap();
        let p = resolve_watch_path(dir.path(), "my-instance", 2).unwrap();
        assert!(p.ends_with("run-2.traj.json"));
    }

    #[test]
    fn resolve_watch_path_ignores_flat_for_run2() {
        let dir = tempfile::tempdir().unwrap();
        // Only flat file exists — run_index=2 should not find it
        std::fs::write(dir.path().join("my-instance.traj.json"), "{}").unwrap();
        assert!(resolve_watch_path(dir.path(), "my-instance", 2).is_none());
    }
}
