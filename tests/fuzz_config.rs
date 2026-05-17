use maxwells_daemon::config::Config;
use proptest::prelude::*;

proptest! {
    #[test]
    fn config_from_toml_str_never_crashes(s in "\\PC*") {
        let _ = Config::from_toml_str(&s);
    }
}
