use proptest::prelude::*;
use rust_swe_agent::agent::parse::extract_action;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100_000))]
    #[test]
    fn extract_action_never_crashes(s in "\\PC*") {
        let _ = extract_action(&s);
    }
}
