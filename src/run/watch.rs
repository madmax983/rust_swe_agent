//! `bench watch`: follow a single in-flight trajectory instance live.

use std::io::{IsTerminal as _, Write as StdWrite};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::Error;
use crate::redaction::Redactor;
use crate::run::inspect::{
    build_inspect_steps_with_max, redact_trajectory_for_inspect, render_step_text,
    resolve_trajectory_path,
};
use crate::trajectory::{Trajectory, exit_reason};

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
    if !args.sweep.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "watch: sweep directory does not exist: {}",
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
    let mut last_new_turn_at: Option<Instant> = None;
    let mut stall_warned = false;
    let mut last_wait_progress_at = Instant::now();
    let mut file_found = false;

    loop {
        let traj_path = resolve_trajectory_path(&args.sweep, &args.instance);

        if traj_path.is_none() && !file_found {
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
        }

        let Some(traj_path) = traj_path else {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        };

        file_found = true;

        let text = match std::fs::read_to_string(&traj_path) {
            Ok(t) => t,
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        let mut traj: Trajectory = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        redact_trajectory_for_inspect(&mut traj, &redactor);

        let steps = build_inspect_steps_with_max(&traj, args.full, max_bytes);

        if steps.len() > emitted_steps {
            let new_steps = &steps[emitted_steps..];
            for step in new_steps {
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
                    stdout.flush()?;
                } else {
                    let rendered = render_step_text(step, color);
                    write!(stdout, "{rendered}")?;
                    stdout.flush()?;
                }
            }
            emitted_steps = steps.len();
            last_new_turn_at = Some(Instant::now());
            stall_warned = false;
        } else if let Some(last_turn) = last_new_turn_at {
            if !stall_warned && last_turn.elapsed() >= stall_duration {
                let secs = last_turn.elapsed().as_secs();
                eprintln!("[stalled: no new turns in {secs}s]");
                stall_warned = true;
            }
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

fn is_terminal_outcome(traj: &Trajectory) -> bool {
    traj.info.outcome.is_some()
        || traj.info.exit_reason.as_deref() == Some(exit_reason::CANCELLED)
        || traj.info.exit_reason.as_deref() == Some(exit_reason::WALLCLOCK_TIMEOUT)
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
    fn not_terminal_when_no_outcome() {
        let traj = Trajectory::new();
        assert!(!is_terminal_outcome(&traj));
    }

    #[test]
    fn terminal_when_exit_reason_cancelled() {
        let mut traj = Trajectory::new();
        traj.info.exit_reason = Some(exit_reason::CANCELLED.into());
        assert!(is_terminal_outcome(&traj));
    }

    #[test]
    fn terminal_when_exit_reason_wallclock_timeout() {
        let mut traj = Trajectory::new();
        traj.info.exit_reason = Some(exit_reason::WALLCLOCK_TIMEOUT.into());
        assert!(is_terminal_outcome(&traj));
    }
}
