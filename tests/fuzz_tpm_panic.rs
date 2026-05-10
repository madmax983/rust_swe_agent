#![allow(clippy::unwrap_used)]
#[test]
fn test_tpm_panic() {
    use rust_swe_agent::run::rate_limit::RateLimitGovernor;
    let governor = RateLimitGovernor::new(None, Some(1), 1).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // We call check_and_maybe_consume directly so it doesn't sleep
        let _ = governor.check_and_maybe_consume(u64::MAX).await;
    });
}
