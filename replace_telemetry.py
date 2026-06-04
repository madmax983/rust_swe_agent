import re

with open("src/telemetry/mod.rs", "r") as f:
    content = f.read()

# We need to remove the ENV_LOCK and replace the unsafe env blocks in 4 tests.

# 1. Remove ENV_LOCK
target_env_lock = """    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }"""
replacement_env_lock = """    use super::*;"""
content = content.replace(target_env_lock, replacement_env_lock)

# 2. Test: resolve_endpoint_prefers_cli_flag
target_1 = """    #[test]
    fn resolve_endpoint_prefers_cli_flag() {
        let _guard = env_lock();
        // SAFETY: serialized by ENV_LOCK; no other thread mutates this var.
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://env-host:4318");
            std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
        }
        let ep = resolve_endpoint(Some("http://cli-host:4318"));
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        // CLI flag is a base URL; /v1/traces is appended.
        assert_eq!(ep.as_deref(), Some("http://cli-host:4318/v1/traces"));
    }"""
replacement_1 = """    #[test]
    fn resolve_endpoint_prefers_cli_flag() {
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None),
            ],
            || {
                let ep = resolve_endpoint(Some("http://cli-host:4318"));
                // CLI flag is a base URL; /v1/traces is appended.
                assert_eq!(ep.as_deref(), Some("http://cli-host:4318/v1/traces"));
            },
        );
    }"""
content = content.replace(target_1, replacement_1)

# 3. Test: resolve_endpoint_falls_back_to_env_var
target_2 = """    #[test]
    fn resolve_endpoint_falls_back_to_env_var() {
        let _guard = env_lock();
        // SAFETY: serialized by ENV_LOCK; no other thread mutates this var.
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://env-host:4318");
            std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
        }
        let ep = resolve_endpoint(None);
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        // Generic env var is a base URL; /v1/traces is appended.
        assert_eq!(ep.as_deref(), Some("http://env-host:4318/v1/traces"));
    }"""
replacement_2 = """    #[test]
    fn resolve_endpoint_falls_back_to_env_var() {
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None),
            ],
            || {
                let ep = resolve_endpoint(None);
                // Generic env var is a base URL; /v1/traces is appended.
                assert_eq!(ep.as_deref(), Some("http://env-host:4318/v1/traces"));
            },
        );
    }"""
content = content.replace(target_2, replacement_2)

# 4. Test: resolve_endpoint_traces_env_var_used_as_full_url
target_3 = """    #[test]
    fn resolve_endpoint_traces_env_var_used_as_full_url() {
        let _guard = env_lock();
        // SAFETY: serialized by ENV_LOCK; no other thread mutates this var.
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::set_var(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://traces-host:4318/v1/traces",
            );
        }
        let ep = resolve_endpoint(None);
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
        }
        // Trace-specific env var is a full URL; used as-is without appending.
        assert_eq!(ep.as_deref(), Some("http://traces-host:4318/v1/traces"));
    }"""
replacement_3 = """    #[test]
    fn resolve_endpoint_traces_env_var_used_as_full_url() {
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", Some("http://traces-host:4318/v1/traces")),
            ],
            || {
                let ep = resolve_endpoint(None);
                // Trace-specific env var is a full URL; used as-is without appending.
                assert_eq!(ep.as_deref(), Some("http://traces-host:4318/v1/traces"));
            },
        );
    }"""
content = content.replace(target_3, replacement_3)

# 5. Test: resolve_endpoint_returns_none_when_unset
target_4 = """    #[test]
    fn resolve_endpoint_returns_none_when_unset() {
        let _guard = env_lock();
        // SAFETY: serialized by ENV_LOCK; no other thread mutates this var.
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
        }
        let ep = resolve_endpoint(None);
        assert!(ep.is_none());
    }"""
replacement_4 = """    #[test]
    fn resolve_endpoint_returns_none_when_unset() {
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None::<&str>),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None::<&str>),
            ],
            || {
                let ep = resolve_endpoint(None);
                assert!(ep.is_none());
            },
        );
    }"""
content = content.replace(target_4, replacement_4)

with open("src/telemetry/mod.rs", "w") as f:
    f.write(content)
print("Replaced in src/telemetry/mod.rs")
