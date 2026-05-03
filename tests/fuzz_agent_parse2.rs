use proptest::prelude::*;
use rust_swe_agent::agent::parse::extract_action;

proptest! {
    #[test]
    fn extract_action_never_crashes(s in "(```bash|```|COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT|\n|.)*") {
        let _ = extract_action(&s);
    }
}
