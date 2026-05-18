use maxwells_daemon::model::litellm::is_anthropic_model;
use proptest::prelude::*;

proptest! {
    #[test]
    fn is_anthropic_model_never_crashes(s in "\\PC*") {
        let _ = is_anthropic_model(&s);
    }
}
