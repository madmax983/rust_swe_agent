use proptest::prelude::*;
use rust_swe_agent::model::litellm::is_anthropic_model;

proptest! {
    #[test]
    fn is_anthropic_model_never_crashes(s in "\\PC*") {
        let _ = is_anthropic_model(&s);
    }
}
