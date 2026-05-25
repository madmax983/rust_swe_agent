use maxwells_daemon::agent::confirm_cli::parse_line_decision;
use proptest::prelude::*;

proptest! {
    #[test]
    fn parse_line_decision_does_not_panic(s in "\\\\PC*") {
        let _ = parse_line_decision(&s);
    }
}
