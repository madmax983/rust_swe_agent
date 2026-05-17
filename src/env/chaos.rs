//! The Chaos Engineer's favorite tool: `ChaosEnvironment`.
//!
//! This module provides a decorator for any [`Environment`] that deterministically
//! injects simulated failures into bash executions. It's designed to test an agent's
//! resilience to transient errors (like sudden timeouts) without needing an
//! unpredictable, flaky underlying system.
//!
//! By forcing `timed_out` results at set intervals, we can ensure the agent loop
//! gracefully recovers instead of panicking.

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::env::{Environment, RunRequest, RunResult};
use crate::error::EnvError;

/// A decorator that wraps an inner [`Environment`] and injects failures deterministically.
///
/// It counts invocations and, on every Nth invocation, returns a synthesized timeout failure
/// rather than actually delegating the command to the underlying environment.
pub struct ChaosEnvironment {
    inner: Box<dyn Environment>,
    invocation_count: Arc<AtomicUsize>,
    fail_every: usize,
}

impl ChaosEnvironment {
    /// Creates a new `ChaosEnvironment` wrapping the provided `inner` environment.
    ///
    /// The `fail_every` parameter controls the failure frequency. For example, if `fail_every`
    /// is `3`, the 3rd, 6th, and 9th calls to [`Environment::run`] will be simulated timeouts.
    /// If `fail_every` is `0`, no failures are ever injected.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::env::{Environment, LocalEnvironment, RunRequest};
    /// use maxwells_daemon::env::chaos::ChaosEnvironment;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let local = Box::new(LocalEnvironment::new());
    /// let chaos = ChaosEnvironment::new(local, 2); // Fail every 2nd command
    ///
    /// let req = RunRequest::new("echo hello");
    ///
    /// // 1st run: Success
    /// let res1 = chaos.run(req.clone()).await.unwrap();
    /// assert_eq!(res1.exit_code, 0);
    /// assert_eq!(res1.timed_out, false);
    ///
    /// // 2nd run: Deterministic failure
    /// let res2 = chaos.run(req).await.unwrap();
    /// assert_eq!(res2.exit_code, -1);
    /// assert_eq!(res2.timed_out, true);
    /// # }
    /// ```
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
