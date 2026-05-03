use proptest::prelude::*;
use rust_swe_agent::ids::StepIdx;

proptest! {
    #[test]
    fn step_idx_next_never_panics(x in 0u32..=u32::MAX) {
        let step = StepIdx(x);
        let _ = step.next();
    }
}
