import re

with open('src/run/mini.rs', 'r') as f:
    content = f.read()

content = re.sub(
    r'unsafe \{ std::env::set_var\("TEST_MANIFEST_API_KEY", fake_secret\) \};\s*let args = make_mini_args_for_manifest_test\(tmp\.path\(\)\.to_path_buf\(\), "secret-test"\);\s*run\(args\)\.await\.unwrap\(\);\s*unsafe \{ std::env::remove_var\("TEST_MANIFEST_API_KEY"\) \};',
    r'''let args = make_mini_args_for_manifest_test(tmp.path().to_path_buf(), "secret-test");
        temp_env::async_with_vars([("TEST_MANIFEST_API_KEY", Some(fake_secret))], run(args)).await.unwrap();''',
    content
)

with open('src/run/mini.rs', 'w') as f:
    f.write(content)


with open('src/stream/sweep_webhook.rs', 'r') as f:
    content = f.read()

content = re.sub(
    r'unsafe \{ std::env::set_var\(\&env_name, \&unique_val\) \};\s*let redactor = Redactor::default_enabled\(\);\s*let sink = SweepWebhookSink::new\(url, \&\[\], redactor, "sweep-redact"\.to_owned\(\)\)\.unwrap\(\);\s*sink\.emit\(SweepNotificationEvent::InstanceCompleted \{\s*instance_id: unique_val\.clone\(\),\s*resolved: false,\s*failure_category: None,\s*cost_usd: None,\s*duration_secs: None,\s*\}\);\s*let req = read_http\(socket\)\.await;\s*unsafe \{ std::env::remove_var\(\&env_name\) \};',
    r'''let redactor = Redactor::default_enabled();

        let sink = SweepWebhookSink::new(url, &[], redactor, "sweep-redact".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::InstanceCompleted {
            instance_id: unique_val.clone(),
            resolved: false,
            failure_category: None,
            cost_usd: None,
            duration_secs: None,
        });

        let req = temp_env::async_with_vars([(&env_name, Some(&unique_val))], read_http(socket)).await;''',
    content
)

with open('src/stream/sweep_webhook.rs', 'w') as f:
    f.write(content)
