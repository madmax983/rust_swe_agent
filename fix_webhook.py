import re

with open('src/stream/sweep_webhook.rs', 'r') as f:
    content = f.read()

pattern = r'// SAFETY: single-threaded test context; no concurrent env reads\.\s*unsafe \{ std::env::set_var\(&env_name, &unique_val\) \};\s*let redactor = Redactor::default_enabled\(\);\s*let sink = SweepWebhookSink::new\(url, &\[\], redactor, "sweep-redact"\.to_owned\(\)\)\.unwrap\(\);\s*sink\.emit\(SweepNotificationEvent::InstanceCompleted \{\s*instance_id: unique_val\.clone\(\),\s*resolved: false,\s*failure_category: None,\s*cost_usd: None,\s*duration_secs: None,\s*\}\);\s*let socket = accept\(&listener\)\.await;\s*let req = read_http\(socket\)\.await;\s*// SAFETY: single-threaded test context; no concurrent env reads\.\s*unsafe \{ std::env::remove_var\(&env_name\) \};'

replacement = r'''let redactor = Redactor::default_enabled();

        let sink = SweepWebhookSink::new(url, &[], redactor, "sweep-redact".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::InstanceCompleted {
            instance_id: unique_val.clone(),
            resolved: false,
            failure_category: None,
            cost_usd: None,
            duration_secs: None,
        });

        let socket = accept(&listener).await;
        let req = temp_env::async_with_vars([(&env_name, Some(&unique_val))], read_http(socket)).await;'''

content = re.sub(pattern, replacement, content)
with open('src/stream/sweep_webhook.rs', 'w') as f:
    f.write(content)
