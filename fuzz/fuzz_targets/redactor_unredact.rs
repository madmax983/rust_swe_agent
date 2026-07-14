#![no_main]

use libfuzzer_sys::fuzz_target;
use maxwells_daemon::redaction::{Redactor, RedactionCfg, surface};

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let cfg = RedactionCfg::default();
        if let Ok(redactor) = Redactor::from_config(&cfg) {
            let res = redactor.redact_text(s, surface::TRAJECTORY);
            if res.redacted {
                let _unredacted = redactor.unredact_text(&res.text);
            }
        }
    }
});
