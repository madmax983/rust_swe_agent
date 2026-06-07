use maxwells_daemon::config::schema::RedactionCfg;
use maxwells_daemon::redaction::Redactor;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5000))]
    #[test]
    fn string_boundary_byte_slicing_fuzz(
        ref s in ".*",
        ref secret in ".*",
    ) {
        let config = RedactionCfg {
            enabled: true,
            unsafe_allow_secret_leaks: false,
            secret_literals: vec![secret.clone()],
            custom_patterns: vec![],
        };
        if let Ok(redactor) = Redactor::from_config(&config) {
            let redacted = redactor.redact_text(s, "test");
            let _unredacted = redactor.unredact_text(&redacted.text);
            let _ = redactor.check(s);
        }
    }
}
