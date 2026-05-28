use maxwells_daemon::ids::StepIdx;
use proptest::prelude::*;

proptest! {
    #[test]
    fn step_idx_next_never_panics(x in 0u32..=u32::MAX) {
        let step = StepIdx::new(x);
        let _ = step.next();
    }
}
