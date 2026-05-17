use maxwells_daemon::agent::parse::extract_action;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100_000))]
    #[test]
    fn extract_action_never_crashes(s in "\\PC*") {
        let _ = extract_action(&s);
    }
}
