use maxwells_daemon::stagnation::{action_hash, canonicalize_action};
use proptest::prelude::*;

proptest! {
    #[test]
    fn canonicalize_action_never_crashes(s in "\\PC*") {
        let _ = canonicalize_action(&s);
    }

    #[test]
    fn action_hash_never_crashes(s in "\\PC*") {
        let _ = action_hash(&s);
    }
}
