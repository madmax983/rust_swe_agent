use maxwells_daemon::run::evaluate::parse_repo_from_instance_id;
use proptest::prelude::*;

proptest! {
    #[test]
    fn parse_repo_does_not_panic(s in "\\PC*") {
        let _ = parse_repo_from_instance_id(&s);
    }
}
