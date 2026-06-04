import re

with open("src/stream/sweep_webhook.rs", "r") as f:
    content = f.read()

target = """        // SAFETY: single-threaded test context; no concurrent env reads.
        unsafe { std::env::set_var(&env_name, &unique_val) };
        let redactor = Redactor::default_enabled();

        let sink = SweepWebhookSink::new(url, &[], redactor, "sweep-redact".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::InstanceCompleted {
            instance_id: unique_val.clone(),
            resolved: false,
            failure_category: None,
            cost_usd: None,
            duration_secs: None,
        });

        let socket = accept(&listener).await;
        let req = read_http(socket).await;
        // SAFETY: single-threaded test context; no concurrent env reads.
        unsafe { std::env::remove_var(&env_name) };"""

replacement = """        #[allow(clippy::large_futures)]
        let req = temp_env::async_with_vars(
            [(&env_name, Some(&unique_val))],
            Box::pin(async {
                let redactor = Redactor::default_enabled();

                let sink = SweepWebhookSink::new(url, &[], redactor, "sweep-redact".to_owned()).unwrap();
                sink.emit(SweepNotificationEvent::InstanceCompleted {
                    instance_id: unique_val.clone(),
                    resolved: false,
                    failure_category: None,
                    cost_usd: None,
                    duration_secs: None,
                });

                let socket = accept(&listener).await;
                read_http(socket).await
            })
        ).await;"""

if target in content:
    content = content.replace(target, replacement, 1)
    with open("src/stream/sweep_webhook.rs", "w") as f:
        f.write(content)
    print("Replaced in src/stream/sweep_webhook.rs")
else:
    print("Target not found")
