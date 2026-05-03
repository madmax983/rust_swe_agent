#![allow(clippy::unwrap_used)]

use rust_swe_agent::model::deterministic::DeterministicModel;
use rust_swe_agent::model::{Model, QueryOpts};

#[tokio::test(flavor = "multi_thread")]
async fn test_deterministic_model_concurrent_query() {
    let model = std::sync::Arc::new(DeterministicModel::new(vec![
        "A".into(),
        "B".into(),
        "C".into(),
        "D".into(),
    ]));

    let m1 = model.clone();
    let h1 = tokio::spawn(async move {
        let opts = QueryOpts::default();
        let _ = m1.query(&[], &opts).await;
    });

    let m2 = model.clone();
    let h2 = tokio::spawn(async move {
        let opts = QueryOpts::default();
        let _ = m2.query(&[], &opts).await;
    });

    let _ = h1.await;
    let _ = h2.await;

    assert_eq!(model.call_count(), 2);
}
