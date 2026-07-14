use maxwells_daemon::config::schema::RedactionCfg;
use maxwells_daemon::redaction::{Redactor, surface};
use proptest::prelude::*;

proptest! {
    #[test]
    fn unredact_does_not_crash(s in "\\PC*") {
        let cfg = RedactionCfg::default();
        if let Ok(redactor) = Redactor::from_config(&cfg) {
            let res = redactor.redact_text(&s, surface::TRAJECTORY);
            if res.redacted {
                let _unredacted = redactor.unredact_text(&res.text);
            }
        }
    }
}
