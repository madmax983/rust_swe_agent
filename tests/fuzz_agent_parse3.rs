use maxwells_daemon::agent::parse::extract_action;
use proptest::prelude::*;

proptest! {
    #[test]
    fn extract_action_never_crashes(s in "\\s*```bash\\s*\\n?.*\\n?```") {
        let _ = extract_action(&s);
    }
}
