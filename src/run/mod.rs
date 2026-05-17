//! Runners: thin glue that wires (config + model + env + agent) together
//! and writes trajectories to disk.

pub mod behavior;
pub mod bundle;
pub mod calibrate;
pub mod command_stats;
pub mod compare;
pub mod dataset;
pub mod evaluate;
pub mod evaluator_selftest;
pub mod forecast;
pub mod frontier;
pub mod github_pr;
pub mod grep;
pub mod hello_world;
pub mod inspect;
pub mod matrix;
pub mod mini;
pub mod patch_stats;
pub mod rate_limit;
pub mod render_only;
pub mod replay;
pub mod report;
pub mod reproduce;
pub mod retry;
pub mod swebench;
pub mod tail;
pub mod trajectory_diff;
pub mod tool_coverage;
pub mod triage;
pub mod watch;
