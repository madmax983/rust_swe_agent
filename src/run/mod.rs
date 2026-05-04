//! Runners: thin glue that wires (config + model + env + agent) together
//! and writes trajectories to disk.

pub mod compare;
pub mod evaluate;
pub mod forecast;
pub mod github_pr;
pub mod hello_world;
pub mod inspect;
pub mod mini;
pub mod rate_limit;
pub mod replay;
pub mod swebench;
pub mod tail;
pub mod trajectory_diff;
