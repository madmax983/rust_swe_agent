#![allow(clippy::unwrap_used)]

use maxwells_daemon::model::deterministic::DeterministicModel;
use maxwells_daemon::model::{Model, QueryOpts};

#[tokio::test(flavor = "multi_thread")]
async fn test_deterministic_model_concurrent_query() {
    let mut vec = Vec::new();
    for i in 0..1000 {
        vec.push(i.to_string());
    }

    let model = std::sync::Arc::new(DeterministicModel::new(vec));

    let mut handles = vec![];
    for _ in 0..1000 {
        let m = model.clone();
        handles.push(tokio::spawn(async move {
            let opts = QueryOpts::default();
            let _ = m.query(&[], &opts).await;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    assert_eq!(model.call_count(), 1000);
}
