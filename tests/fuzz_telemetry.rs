use maxwells_daemon::telemetry::parse_otlp_header_env;
use proptest::prelude::*;

proptest! {
    #[test]
    fn parse_otlp_header_env_does_not_panic(s in "\\PC*") {
        let _ = parse_otlp_header_env(&s);
    }
}
