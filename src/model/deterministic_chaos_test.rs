#![allow(clippy::expect_used)]
#[cfg(test)]
mod tests {
    use crate::model::Model;
    use crate::model::QueryOpts;
    use crate::model::deterministic::DeterministicModel;
    use std::sync::Arc;
    use tokio::task;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_chaos_concurrency_deterministic_model() {
        let mut handlers = vec![];
        let m = Arc::new(DeterministicModel::new(
            (0..1000).map(|i| format!("response {i}")),
        ));

        for _ in 0..100 {
            let m_clone = Arc::clone(&m);
            handlers.push(task::spawn(async move {
                for _ in 0..10 {
                    m_clone
                        .query(&[], &QueryOpts::default())
                        .await
                        .expect("model query failed");
                }
            }));
        }

        for handler in handlers {
            handler.await.unwrap_or_else(|_| panic!("join failed"));
        }

        assert_eq!(m.call_count(), 1000);
    }
}
