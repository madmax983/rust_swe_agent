use loom::sync::Mutex;
use loom::thread;
use maxwells_daemon::agent::confirm::{ConfirmContext, ConfirmDecision};
use std::sync::Arc;

#[allow(clippy::unwrap_used)]
pub struct LoomScriptedConfirmer {
    decisions: Mutex<std::collections::VecDeque<ConfirmDecision>>,
    calls: Mutex<u32>,
}

impl LoomScriptedConfirmer {
    #[allow(clippy::unwrap_used)]
    #[must_use]
    pub fn new(decisions: impl IntoIterator<Item = ConfirmDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            calls: Mutex::new(0),
        }
    }

    #[allow(clippy::unwrap_used)]
    #[must_use]
    pub fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }

    #[allow(clippy::unwrap_used)]
    pub fn confirm_sync(&self, _ctx: &ConfirmContext) -> ConfirmDecision {
        *self.calls.lock().unwrap() += 1;
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ConfirmDecision::Approve)
    }
}

#[allow(clippy::unwrap_used, clippy::redundant_clone)]
#[test]
fn test_scripted_confirmer_concurrent() {
    loom::model(|| {
        let decisions: Vec<ConfirmDecision> =
            vec![ConfirmDecision::Approve, ConfirmDecision::Abort];
        let confirmer = Arc::new(LoomScriptedConfirmer::new(decisions));
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "x".into(),
            step: 0,
            step_limit: 1,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
            rationale: String::new(),
        };

        let c1 = confirmer.clone();
        let ctx1 = ctx.clone();
        let t1 = thread::spawn(move || {
            let _ = c1.confirm_sync(&ctx1);
        });

        let c2 = confirmer.clone();
        let ctx2 = ctx.clone();
        let t2 = thread::spawn(move || {
            let _ = c2.confirm_sync(&ctx2);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(confirmer.call_count(), 2);
    });
}
