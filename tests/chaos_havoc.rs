use maxwells_daemon::env::{Environment, LocalEnvironment, RunRequest};
use std::time::Duration;

#[tokio::test]
async fn havoc_env_oom_is_bounded() {
    let mut env = LocalEnvironment::new();
    let req = RunRequest {
        command: "yes".to_string(),
        timeout: Duration::from_secs(2),
        cancellation: None,
        env: Default::default(),
        cwd: None,
        stdin: None,
    };

    let result = env.run(req).await.unwrap();
    assert!(result.timed_out);
    assert!(result.stdout.len() <= 5 * 1024 * 1024 + 8192, "Buffer exceeded 5MB! Length: {}", result.stdout.len());
}
