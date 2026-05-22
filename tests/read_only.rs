#![allow(clippy::unwrap_used)]
use maxwells_daemon::{
    config::Config,
    run::mini::{InteractiveMode, MiniArgs, run},
};

#[tokio::test]
async fn read_only_blocks_bash_and_preserves_git_status() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg(&repo)
        .status()
        .unwrap();
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    std::process::Command::new("git")
        .current_dir(&repo)
        .args(["add", "."])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(&repo)
        .args([
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ])
        .status()
        .unwrap();

    let mut cfg = Config::defaults().unwrap();
    cfg.root.environment.workdir = repo.display().to_string();

    let result = run(MiniArgs {
        task: "readonly".into(),
        extra_context: None,
        config: cfg,
        output_dir: tmp.path().join("runs"),
        trajectory_name: "ro".into(),
        deterministic_responses: Some(vec!["```bash\nprintf 'x' > a.txt\n```".into()]),
        deterministic_usage_per_call: None,
        task_timeout_secs: Some(20),
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        resume_from: None,
        interactive_mode: InteractiveMode::Off,
        trace_id: None,
        webhook_url: None,
        webhook_headers: vec![],
        local_workdir: Some(repo.clone()),
        read_only: true,
        allow_mcp_in_read_only: false,
    })
    .await;
    assert!(
        result.is_err(),
        "read-only violation should return an error"
    );

    let traj = std::fs::read_to_string(tmp.path().join("runs/ro.traj.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&traj).unwrap();
    assert_eq!(
        v["info"]["failure_category"].as_str(),
        Some("read_only_violation")
    );

    let status = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).trim().is_empty());
}
