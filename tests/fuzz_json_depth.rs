use rust_swe_agent::redaction::Redactor;

#[test]
fn test_json_depth() {
    let mut value = serde_json::Value::Null;
    for _ in 0..10000 {
        value = serde_json::Value::Array(vec![value]);
    }
    let redactor = Redactor::default_enabled();
    redactor.redact_json_value(&mut value, "trajectory");
}
