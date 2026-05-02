use proptest::prelude::*;
use rust_swe_agent::run::evaluate::parse_repo_from_instance_id;

proptest! {
    #[test]
    fn parse_repo_from_instance_id_never_crashes(s in "\\PC*") {
        let _ = parse_repo_from_instance_id(&s);
    }
}
