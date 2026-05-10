#![no_main]

use libfuzzer_sys::fuzz_target;
use rust_swe_agent::redaction::{Redactor, surface};
use rust_swe_agent::config::RedactionCfg;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let redactor = Redactor::default_enabled();
        let _ = redactor.redact_text(text, surface::TRAJECTORY);
    }
});
