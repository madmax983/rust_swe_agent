use maxwells_daemon::run::mini::slugify;
use proptest::prelude::*;

proptest! {
    #[test]
    fn slugify_never_crashes(s in "\\PC*") {
        let _ = slugify(&s);
    }
}
