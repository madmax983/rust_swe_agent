//! Runners: thin glue that wires (config + model + env + agent) together
//! and writes trajectories to disk.

pub mod bundle;
pub mod calibrate;
pub mod compare;
pub mod dataset;
pub mod evaluate;
pub mod forecast;
pub mod frontier;
pub mod github_pr;
pub mod hello_world;
pub mod inspect;
pub mod matrix;
pub mod mini;
pub mod patch_stats;
pub mod rate_limit;
pub mod replay;
pub mod reproduce;
pub mod swebench;
pub mod tail;
pub mod trajectory_diff;
pub mod triage;
