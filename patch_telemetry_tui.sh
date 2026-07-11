#!/bin/bash
set -e

# I'll just write a quick python script for these two files.
cat << 'PYEOF' > patch_telemetry_tui.py
import re

def fix_confirm_tui():
    with open('src/agent/confirm_tui.rs', 'r') as f:
        content = f.read()

    new_test = """    #[test]
    fn test_mouse_scroll_step_from_env() {
        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", None::<&str>, || {
            assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        });

        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", Some("7"), || {
            assert_eq!(mouse_scroll_step_from_env(), 7);
        });

        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", Some("not-a-number"), || {
            assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        });
        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", Some("0"), || {
            assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        });
    }"""

    # replace the whole test
    content = re.sub(r'#\[test\]\n\s*fn test_mouse_scroll_step_from_env\(\) \{.*?\n\s*\}\n', new_test + '\n', content, flags=re.DOTALL)

    with open('src/agent/confirm_tui.rs', 'w') as f:
        f.write(content)

fix_confirm_tui()

def fix_telemetry():
    with open('src/telemetry/mod.rs', 'r') as f:
        content = f.read()

    # We have lots of cases in telemetry, let's just do it manually with replacements
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

    for test_name, new_code in tests_to_replace.items():
        # Match from #[test] to the end of the test function body
        pattern = r'#\[test\]\n\s*fn ' + test_name + r'\(\) \{.*?\n\s*\}'
        content = re.sub(pattern, new_code, content, flags=re.DOTALL)

    with open('src/telemetry/mod.rs', 'w') as f:
        f.write(content)

fix_telemetry()
PYEOF
python3 patch_telemetry_tui.py
