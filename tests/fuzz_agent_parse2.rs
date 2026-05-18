use maxwells_daemon::agent::parse::extract_action;
use proptest::prelude::*;

proptest! {
    #[test]
    fn extract_action_never_crashes(s in "(```bash|```|COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT|\n|.)*") {
        let _ = extract_action(&s);
    }
}
