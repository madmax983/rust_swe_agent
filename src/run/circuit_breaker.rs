/// Circuit-breaker logic for detecting systemic sweep failures.
///
/// After each instance completes, the sweep runner calls `CircuitBreaker::check`
/// with the accumulated completion records. If an operator-actionable failure
/// category dominates at or above the configured share threshold and the minimum
/// sample count has been reached, the check returns `Some(category)` and the
/// sweep halts gracefully.
use crate::trajectory::FailureCategory;

/// An entry in the completion log fed to the circuit breaker.
///
/// `None` category represents a successful or uncategorised completion and
/// counts toward the denominator but never toward an actionable category's
/// share.  Including all completed instances (successes and failures) in
/// the slice ensures the share percentage is relative to the full completion
/// count, not just the failing subset.
pub type CompletionRecord = (Option<FailureCategory>, bool);

/// Stateless circuit-breaker configuration.
///
/// Call [`CircuitBreaker::check`] after each instance completes. The check
/// is O(n) in the number of completed instances and is called at most once
/// per completion, so the total work per sweep is O(n²) worst-case — which
/// is fine for the sizes where systemic failures occur (typically n ≤ 10).
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreaker {
    enabled: bool,
    min_samples: usize,
    share_pct: u8,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(enabled: bool, min_samples: usize, share_pct: u8) -> Self {
        Self {
            enabled,
            min_samples,
            share_pct,
        }
    }

    /// Evaluate the completion record set and return the dominant
    /// actionable failure category if the breaker should trip, or `None`
    /// if the sweep should continue.
    ///
    /// The denominator is always the total number of completed instances
    /// (not just the failing ones), so a partial success rate prevents
    /// false positives.
    #[must_use]
    pub fn check(&self, completed: &[CompletionRecord]) -> Option<FailureCategory> {
        if !self.enabled || completed.len() < self.min_samples {
            return None;
        }
        let total = completed.len();
        // Count only actionable categories; None (success) adds to total but
        // not to any category count — this is what prevents premature trips
        // when successful completions dilute the actionable-failure share.
        let mut counts: std::collections::BTreeMap<FailureCategory, usize> =
            std::collections::BTreeMap::new();
        for (maybe_cat, _) in completed {
            if let Some(cat) = maybe_cat {
                if cat.is_actionable() {
                    *counts.entry(*cat).or_insert(0) += 1;
                }
            }
        }
        // Find the dominant actionable category.
        counts.into_iter().find_map(|(cat, count)| {
            #[allow(clippy::cast_precision_loss)]
            let share = count as f64 / total as f64 * 100.0;
            #[allow(clippy::cast_lossless)]
            if share >= self.share_pct as f64 {
                Some(cat)
            } else {
                None
            }
        })
    }
}
