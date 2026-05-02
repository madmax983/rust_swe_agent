use proptest::prelude::*;
use rust_swe_agent::config::Config;

proptest! {
    #[test]
    fn config_from_toml_str_never_crashes(s in "\\PC*") {
        let _ = Config::from_toml_str(&s);
    }
}
