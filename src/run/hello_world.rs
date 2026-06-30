//! Sanity-check the pipeline with a scripted model. Runs a two-turn agent that
//! echoes hello, then submits "ok".

use std::path::{Path, PathBuf};

use super::mini::{InteractiveMode, MiniArgs, run};
use crate::config::Config;
use crate::error::Error;

pub async fn main(output_dir: PathBuf, config_path: Option<&Path>) -> Result<(), Error> {
    let cfg = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    let traj_path = output_dir.join("hello-world.traj.json");
    let out_path = output_dir.join("hello-world.output.txt");
    let args = MiniArgs {
        task: "Say hello".to_owned(),
        extra_context: None,
        config: cfg,
        driver: super::mini::RunDriver::Builtin,
        driver_append_system_prompt: false,
        driver_isolated: false,
        output_dir: output_dir.clone(),
        trajectory_name: "hello-world".into(),
        deterministic_responses: Some(vec![
            "```bash\necho hello\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ]),
        deterministic_usage_per_call: None,
        task_timeout_secs: None,
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        resume_from: None,
        interactive_mode: InteractiveMode::Off,
        no_bell: false,
        trace_id: None,
        webhook_url: None,
        webhook_headers: vec![],
        event_log: None,
        event_log_instance_id: None,
        local_workdir: None,
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
        issue_provenance: None,
    };
    run(args).await?;
    println!("hello-world smoke complete");
    println!("trajectory: {}", traj_path.display());
    println!("output: {}", out_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn hello_world_smoke() {
        let dir = tempdir().unwrap();
        main(dir.path().to_path_buf(), None).await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        let Some(traj) = entries
            .iter()
            .find(|e| e.file_name().to_string_lossy().ends_with(".traj.json"))
        else {
            panic!("trajectory file not written");
        };
        let contents = std::fs::read_to_string(traj.path()).unwrap();
        assert!(contents.contains("mini-swe-agent-1.3"));
    }
}
