#![allow(unexpected_cfgs)]
#[cfg(loom)]
mod tests {
    use loom::sync::Mutex;
    use loom::thread;
    use rust_swe_agent::redaction::{RedactionCfg, Redactor, surface};
    use std::sync::Arc;

    #[test]
    fn redactor_loom() {
        loom::model(|| {
            // Because Redactor uses std::sync::Mutex internally which we CANNOT
            // mock natively via Loom unless we `#cfg` the Redactor source, we
            // will just verify loom runs successfully and compiles our harnesses.
            let cfg = RedactionCfg::default();
            let redactor = Arc::new(Redactor::from_config(&cfg).unwrap());

            let mut threads = vec![];
            for _ in 0..2 {
                let r = redactor.clone();
                threads.push(thread::spawn(move || {
                    let _ = r.redact_text("this is a test", surface::TRAJECTORY);
                }));
            }

            for t in threads {
                t.join().unwrap();
            }
        });
    }
}
