import re

with open('src/telemetry/mod.rs', 'r') as f:
    content = f.read()

tests_to_replace = {
    "resolve_endpoint_prefers_cli_flag": """    #[test]
    fn resolve_endpoint_prefers_cli_flag() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None),
            ],
            || resolve_endpoint(Some("http://cli-host:4318")),
        );
        assert_eq!(ep.as_deref(), Some("http://cli-host:4318/v1/traces"));
    }""",
    "resolve_endpoint_falls_back_to_env_var": """    #[test]
    fn resolve_endpoint_falls_back_to_env_var() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None),
            ],
            || resolve_endpoint(None),
        );
        assert_eq!(ep.as_deref(), Some("http://env-host:4318/v1/traces"));
    }""",
    "resolve_endpoint_traces_env_var_used_as_full_url": """    #[test]
    fn resolve_endpoint_traces_env_var_used_as_full_url() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
                (
                    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                    Some("http://traces-host:4318/v1/traces"),
                ),
            ],
            || resolve_endpoint(None),
        );
        assert_eq!(ep.as_deref(), Some("http://traces-host:4318/v1/traces"));
    }""",
    "resolve_endpoint_returns_none_when_unset": """    #[test]
    fn resolve_endpoint_returns_none_when_unset() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None::<&str>),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None::<&str>),
            ],
            || resolve_endpoint(None),
        );
        assert!(ep.is_none());
    }""",
    "resolve_metrics_endpoint_prefers_cli_flag": """    #[test]
    fn resolve_metrics_endpoint_prefers_cli_flag() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", None),
            ],
            || resolve_metrics_endpoint(Some("http://cli-host:4318")),
        );
        assert_eq!(ep.as_deref(), Some("http://cli-host:4318/v1/metrics"));
    }""",
    "resolve_metrics_endpoint_falls_back_to_env_var": """    #[test]
    fn resolve_metrics_endpoint_falls_back_to_env_var() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", None),
            ],
            || resolve_metrics_endpoint(None),
        );
        assert_eq!(ep.as_deref(), Some("http://env-host:4318/v1/metrics"));
    }""",
    "resolve_metrics_endpoint_metrics_env_var_used_as_full_url": """    #[test]
    fn resolve_metrics_endpoint_metrics_env_var_used_as_full_url() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
                (
                    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                    Some("http://metrics-host:4318/v1/metrics"),
                ),
            ],
            || resolve_metrics_endpoint(None),
        );
        assert_eq!(ep.as_deref(), Some("http://metrics-host:4318/v1/metrics"));
    }""",
    "resolve_metrics_endpoint_returns_none_when_unset": """    #[test]
    fn resolve_metrics_endpoint_returns_none_when_unset() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None::<&str>),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", None::<&str>),
            ],
            || resolve_metrics_endpoint(None),
        );
        assert!(ep.is_none());
    }""",
    "resolve_metrics_interval_prefers_env_var": """    #[test]
    fn resolve_metrics_interval_prefers_env_var() {
        let _guard = env_lock();
        let interval = temp_env::with_var("OTEL_METRIC_EXPORT_INTERVAL", Some("30000"), || {
            resolve_metrics_interval(Some(10))
        });
        assert_eq!(interval, std::time::Duration::from_secs(30));
    }""",
    "resolve_metrics_interval_prefers_cli_secs": """    #[test]
    fn resolve_metrics_interval_prefers_cli_secs() {
        let _guard = env_lock();
        let interval = temp_env::with_var("OTEL_METRIC_EXPORT_INTERVAL", None::<&str>, || {
            resolve_metrics_interval(Some(10))
        });
        assert_eq!(interval, std::time::Duration::from_secs(10));
    }""",
    "resolve_metrics_interval_defaults_to_15s": """    #[test]
    fn resolve_metrics_interval_defaults_to_15s() {
        let _guard = env_lock();
        let interval = temp_env::with_var("OTEL_METRIC_EXPORT_INTERVAL", None::<&str>, || {
            resolve_metrics_interval(None)
        });
        assert_eq!(interval, std::time::Duration::from_secs(15));
    }""",
    "resolve_metrics_interval_clamps_zero_env_var_to_1s": """    #[test]
    fn resolve_metrics_interval_clamps_zero_env_var_to_1s() {
        let _guard = env_lock();
        let interval = temp_env::with_var("OTEL_METRIC_EXPORT_INTERVAL", Some("0"), || {
            resolve_metrics_interval(None)
        });
        assert_eq!(interval, std::time::Duration::from_secs(1));
    }""",
    "resolve_metrics_interval_clamps_zero_cli_secs_to_1s": """    #[test]
    fn resolve_metrics_interval_clamps_zero_cli_secs_to_1s() {
        let _guard = env_lock();
        let interval = temp_env::with_var("OTEL_METRIC_EXPORT_INTERVAL", None::<&str>, || {
            resolve_metrics_interval(Some(0))
        });
        assert_eq!(interval, std::time::Duration::from_secs(1));
    }""",
    "resolve_metrics_headers_prefers_metrics_var": """    #[test]
    fn resolve_metrics_headers_prefers_metrics_var() {
        let _guard = env_lock();
        let headers = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_HEADERS", Some("a=1,b=2")),
                ("OTEL_EXPORTER_OTLP_METRICS_HEADERS", Some("c=3")),
            ],
            || resolve_metrics_headers(),
        );
        assert_eq!(headers, vec![("c".to_owned(), "3".to_owned())]);
    }"""
}

# Find the end of each block by looking for the next #[test] or end of file
for test_name, new_code in tests_to_replace.items():
    # Use re to replace the entire test function body.
    # The regex matches #[test] \n fn test_name() { ... }
    # Since rust uses nested braces, a naive regex is hard.
    # We can split the string by the function signature and find the matching closing brace.
    sig = f"fn {test_name}() {{"
    if sig in content:
        start_idx = content.find(f"#[test]\n    {sig}")
        if start_idx == -1:
            start_idx = content.find(f"#[test]\n    fn {test_name}() {{")

        brace_count = 0
        end_idx = -1
        # start parsing after the opening brace of the function
        idx = content.find('{', start_idx)
        if idx != -1:
            for i in range(idx, len(content)):
                if content[i] == '{':
                    brace_count += 1
                elif content[i] == '}':
                    brace_count -= 1
                    if brace_count == 0:
                        end_idx = i + 1
                        break

        if start_idx != -1 and end_idx != -1:
            content = content[:start_idx] + new_code + content[end_idx:]

with open('src/telemetry/mod.rs', 'w') as f:
    f.write(content)
