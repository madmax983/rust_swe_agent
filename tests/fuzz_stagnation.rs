use maxwells_daemon::stagnation::{StagnationDetector, canonicalize_action};
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_canonicalize_action_no_panic(s in ".*") {
        canonicalize_action(&s);
    }

    #[test]
    fn test_stagnation_detector_no_panic(actions in prop::collection::vec(".*", 0..100)) {
        let mut det = StagnationDetector::new(4, 8);
        for (i, action) in actions.into_iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            det.observe(i as u32, &action);
        }
    }
}
