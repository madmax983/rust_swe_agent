use proptest::prelude::*;
use rust_swe_agent::run::mini::slugify;

proptest! {
    #[test]
    fn slugify_never_crashes(s in "\\PC*") {
        let _ = slugify(&s);
    }
}
