use proptest::prelude::*;
use rust_swe_agent::agent::parse::extract_action;

proptest! {
    #[test]
    fn extract_action_never_crashes(s in "\\PC*") {
        let _ = extract_action(&s);
    }
}
