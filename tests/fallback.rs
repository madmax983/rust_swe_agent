use rust_swe_agent::model::{FallbackModel, DeterministicModel, QueryOpts, Model};

#[tokio::test]
async fn test_fallback_integration() {
    let primary = Box::new(DeterministicModel::new(vec![]));
    let secondary = Box::new(DeterministicModel::new(vec!["secondary_success".into()]));
    let model = FallbackModel::new(primary, secondary);

    let res = model.query(&[], &QueryOpts::default()).await.unwrap();
    assert_eq!(res.content, "secondary_success");
}
