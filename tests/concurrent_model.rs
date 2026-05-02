#![allow(clippy::unwrap_used)]

use rust_swe_agent::model::deterministic::DeterministicModel;
use rust_swe_agent::model::{Model, QueryOpts};

#[test]
fn test_deterministic_model_concurrent_query() {
    let model = std::sync::Arc::new(DeterministicModel::new(vec![
        "A".into(),
        "B".into(),
        "C".into(),
        "D".into(),
    ]));

    let m1 = model.clone();
    let t1 = std::thread::spawn(move || {
        let opts = QueryOpts::default();
        let _ = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(m1.query(&[], &opts));
    });

    let m2 = model.clone();
    let t2 = std::thread::spawn(move || {
        let opts = QueryOpts::default();
        let _ = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(m2.query(&[], &opts));
    });

    t1.join().unwrap();
    t2.join().unwrap();

    assert_eq!(model.call_count(), 2);
}
