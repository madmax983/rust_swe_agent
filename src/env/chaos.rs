use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::env::{Environment, RunRequest, RunResult};
use crate::error::EnvError;

/// A decorator that wraps an inner `Environment` and injects failures deterministically.
pub struct ChaosEnvironment {
    inner: Box<dyn Environment>,
    invocation_count: Arc<AtomicUsize>,
    fail_every: usize,
}

impl ChaosEnvironment {
    pub fn new(inner: Box<dyn Environment>, fail_every: usize) -> Self {
        Self {
            inner,
            invocation_count: Arc::new(AtomicUsize::new(0)),
            fail_every,
        }
    }
}

#[async_trait]
impl Environment for ChaosEnvironment {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        let count = self.invocation_count.fetch_add(1, Ordering::SeqCst) + 1;

        if self.fail_every > 0 && count % self.fail_every == 0 {
            // Inject a simulated timeout failure
            return Ok(RunResult {
                stdout: String::new(),
                stderr: "simulated chaos failure: timed out".to_string(),
                exit_code: -1,
                timed_out: true,
            });
        }

        self.inner.run(req).await
    }

    async fn shutdown(&mut self) -> Result<(), EnvError> {
        self.inner.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::env::local::LocalEnvironment;

    #[tokio::test]
    async fn test_chaos_environment_injects_failures() {
        let inner = Box::new(LocalEnvironment::new());
        let env = ChaosEnvironment::new(inner, 3); // Fail every 3rd command

        let req = RunRequest::new("echo hello");

        // 1st request - Should succeed
        let res1 = env.run(req.clone()).await.unwrap();
        assert_eq!(res1.exit_code, 0);

        // 2nd request - Should succeed
        let res2 = env.run(req.clone()).await.unwrap();
        assert_eq!(res2.exit_code, 0);

        // 3rd request - Should fail (timed out simulation)
        let res3 = env.run(req.clone()).await.unwrap();
        assert!(res3.timed_out);
        assert_eq!(res3.exit_code, -1);
    }
}
