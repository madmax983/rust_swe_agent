//! Port of `run/hello_world.py`: sanity-check the pipeline with a scripted
//! model. Runs a two-turn agent that echoes hello, then submits "ok".

use std::path::PathBuf;

use super::mini::{MiniArgs, run};
use crate::config::Config;
use crate::error::Error;

pub async fn main(output_dir: PathBuf) -> Result<(), Error> {
    let cfg = Config::defaults()?;
    let args = MiniArgs {
        task: "Say hello".to_owned(),
        extra_context: None,
        config: cfg,
        output_dir,
        trajectory_name: "hello-world".into(),
        deterministic_responses: Some(vec![
            "```bash\necho hello\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ]),
    };
    run(args).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn hello_world_smoke() {
        let dir = tempdir().unwrap();
        main(dir.path().to_path_buf()).await.unwrap();
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
        assert!(contents.contains("mini-swe-agent-1.1"));
    }
}
