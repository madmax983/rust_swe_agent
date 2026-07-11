import re

with open('src/run/mini.rs', 'r') as f:
    content = f.read()

pattern = r'unsafe\s*\{\s*std::env::set_var\("TEST_MANIFEST_API_KEY",\s*fake_secret\)\s*\}\;\s*let\s*args\s*=\s*make_mini_args_for_manifest_test\(tmp\.path\(\)\.to_path_buf\(\),\s*"secret-test"\);\s*run\(args\)\.await\.unwrap\(\);\s*// Clean up env var\s*unsafe\s*\{\s*std::env::remove_var\("TEST_MANIFEST_API_KEY"\)\s*\}\;'

replacement = r'''let args = make_mini_args_for_manifest_test(tmp.path().to_path_buf(), "secret-test");
        temp_env::async_with_vars([("TEST_MANIFEST_API_KEY", Some(fake_secret))], run(args)).await.unwrap();'''

content = re.sub(pattern, replacement, content)

with open('src/run/mini.rs', 'w') as f:
    f.write(content)


with open('src/stream/sweep_webhook.rs', 'r') as f:
    content = f.read()

pattern2 = r'unsafe\s*\{\s*std::env::set_var\(&env_name,\s*&unique_val\)\s*\}\;\s*let\s*redactor\s*=\s*Redactor::default_enabled\(\);\s*let\s*sink\s*=\s*SweepWebhookSink::new\(url,\s*&\[\],\s*redactor,\s*"sweep-redact"\.to_owned\(\)\)\.unwrap\(\);\s*sink\.emit\(SweepNotificationEvent::InstanceCompleted\s*\{\s*instance_id:\s*unique_val\.clone\(\),\s*resolved:\s*false,\s*failure_category:\s*None,\s*cost_usd:\s*None,\s*duration_secs:\s*None,\s*\}\);\s*let\s*req\s*=\s*read_http\(socket\)\.await;\s*unsafe\s*\{\s*std::env::remove_var\(&env_name\)\s*\}\;'

replacement2 = r'''let redactor = Redactor::default_enabled();

        let sink = SweepWebhookSink::new(url, &[], redactor, "sweep-redact".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::InstanceCompleted {
            instance_id: unique_val.clone(),
            resolved: false,
            failure_category: None,
            cost_usd: None,
            duration_secs: None,
        });

        let req = temp_env::async_with_vars([(&env_name, Some(&unique_val))], read_http(socket)).await;'''

content = re.sub(pattern2, replacement2, content)

with open('src/stream/sweep_webhook.rs', 'w') as f:
    f.write(content)
