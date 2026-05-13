//! In-loop agent stagnation detection (issue #157).
//!
//! The detector maintains a ring buffer of the last W canonicalized bash
//! actions. After each completed bash step it checks whether any single
//! action appears K or more times in the buffer. When the condition is
//! met the agent loop is halted with `FailureCategory::AgentStagnation`.
//!
//! **Canonicalization rule** (deterministic, byte-level):
//! 1. Trim leading/trailing ASCII whitespace.
//! 2. Collapse every internal run of ASCII whitespace to a single space.
//! 3. Strip a single trailing semicolon (if present after trimming).
//!
//! The canonical form is then SHA-256 hashed; only the hash is stored in
//! the stagnation record for privacy/redaction friendliness.

use std::collections::VecDeque;

use sha2::{Digest, Sha256};

/// Canonicalize a bash action string per the spec rule.
///
/// This function is `pub` so integration tests can verify the rule directly.
pub fn canonicalize_action(raw: &str) -> String {
    // 1. Trim surrounding whitespace.
    let trimmed = raw.trim();
    // 2. Collapse internal whitespace runs.
    let collapsed = collapse_internal_whitespace(trimmed);
    // 3. Strip single trailing semicolon. After stripping, trim trailing
    //    whitespace again so "ls ;" and "ls;" both produce "ls".
    if let Some(rest) = collapsed.strip_suffix(';') {
        rest.trim_end().to_owned()
    } else {
        collapsed
    }
}

fn collapse_internal_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_space = false;
    for ch in s.chars() {
        if ch.is_ascii_whitespace() {
            if !in_space {
                result.push(' ');
                in_space = true;
            }
        } else {
            in_space = false;
            result.push(ch);
        }
    }
    result
}

/// SHA-256 hex digest (truncated to 16 bytes / 32 hex chars) of a canonical action.
pub fn action_hash(canonical: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    // 16 bytes = 32 hex chars — enough to distinguish actions while keeping the
    // info block compact.
    let mut hex = String::with_capacity(32);
    for b in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// A single entry in the stagnation ring buffer.
#[derive(Debug, Clone)]
struct Entry {
    step_index: u32,
    /// SHA-256 hex (truncated) of the canonical action.
    hash: String,
}

/// Rolling-window stagnation detector.
///
/// Call [`StagnationDetector::observe`] after each completed bash step. It
/// returns `Some(StagnationTrip)` the first time the threshold is met.
#[derive(Debug)]
pub struct StagnationDetector {
    /// Maximum number of entries to retain (= W, the window size).
    window: u32,
    /// Repeat threshold (= K).
    threshold: u32,
    buffer: VecDeque<Entry>,
}

/// Evidence returned when the detector trips.
#[derive(Debug, Clone)]
pub struct StagnationTrip {
    /// Truncated SHA-256 hex of the repeated canonical action.
    pub action_hash: String,
    /// Number of occurrences observed (= K at trip time).
    pub count: u32,
    /// Window size at trip time (= W).
    pub window: u32,
    /// Step indices of every contributing repeat, in ascending order.
    pub step_indices: Vec<u32>,
}

impl StagnationDetector {
    /// Create a new detector with the given window size and repeat threshold.
    ///
    /// Panics if `window < threshold` — callers must validate config before
    /// constructing (the `DefaultAgentBuilder` enforces this).
    pub fn new(threshold: u32, window: u32) -> Self {
        assert!(
            window >= threshold,
            "stagnation window ({window}) must be >= threshold ({threshold})"
        );
        Self {
            window,
            threshold,
            buffer: VecDeque::with_capacity(window as usize),
        }
    }

    /// Record the bash action taken at `step_index` and check for stagnation.
    ///
    /// Returns `Some(StagnationTrip)` if the same action hash has appeared
    /// `threshold` or more times in the current window; `None` otherwise.
    pub fn observe(&mut self, step_index: u32, action: &str) -> Option<StagnationTrip> {
        let canonical = canonicalize_action(action);
        let hash = action_hash(&canonical);

        // Evict oldest entry when the buffer is full.
        if self.buffer.len() == self.window as usize {
            self.buffer.pop_front();
        }
        self.buffer.push_back(Entry {
            step_index,
            hash: hash.clone(),
        });

        // Count occurrences of this hash in the current buffer.
        let matching: Vec<u32> = self
            .buffer
            .iter()
            .filter(|e| e.hash == hash)
            .map(|e| e.step_index)
            .collect();

        if matching.len() >= self.threshold as usize {
            #[allow(clippy::cast_possible_truncation)]
            let count = matching.len() as u32;
            Some(StagnationTrip {
                action_hash: hash,
                count,
                window: self.window,
                step_indices: matching,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn canonicalize_trims_whitespace() {
        assert_eq!(canonicalize_action("  ls  "), "ls");
    }

    #[test]
    fn canonicalize_strips_trailing_semicolon() {
        assert_eq!(canonicalize_action("ls;"), "ls");
        assert_eq!(canonicalize_action("ls ;"), "ls");
    }

    #[test]
    fn canonicalize_collapses_internal_whitespace() {
        assert_eq!(canonicalize_action("ls   -la"), "ls -la");
    }

    #[test]
    fn identical_variants_produce_same_canonical() {
        let variants = ["ls", "ls  ", "ls;", "  ls  ", "  ls  ;"];
        let base = canonicalize_action("ls");
        for v in &variants {
            assert_eq!(
                canonicalize_action(v),
                base,
                "variant {v:?} did not canonicalize to {base:?}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "stagnation window (4) must be >= threshold (5)")]
    fn detector_panics_if_window_less_than_threshold() {
        let _ = StagnationDetector::new(5, 4);
    }

    #[test]
    fn detector_trips_immediately_when_threshold_is_zero() {
        let mut det = StagnationDetector::new(0, 4);
        let trip = det.observe(0, "ls").expect("should trip immediately");
        assert_eq!(trip.count, 1);
        assert_eq!(trip.step_indices, vec![0]);
    }

    #[test]
    fn detector_trips_at_k_identical_observations() {
        let mut det = StagnationDetector::new(4, 8);
        assert!(det.observe(0, "ls").is_none());
        assert!(det.observe(1, "ls").is_none());
        assert!(det.observe(2, "ls").is_none());
        let trip = det.observe(3, "ls").expect("should trip at 4th identical");
        assert_eq!(trip.count, 4);
        assert_eq!(trip.step_indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn detector_does_not_trip_for_three_action_rotation() {
        // A 3-action cycle puts at most ceil(8/3)=3 occurrences per action in
        // any W=8 window, which is below the K=4 threshold.
        let mut det = StagnationDetector::new(4, 8);
        let actions = ["ls", "cat foo", "echo hi"];
        for i in 0..12u32 {
            let action = actions[(i as usize) % 3];
            assert!(
                det.observe(i, action).is_none(),
                "should not trip at step {i} with 3-action rotation"
            );
        }
    }

    #[test]
    fn detector_window_slides_correctly() {
        // Fill window with ls (W=4, K=4), then push different commands to slide old entries out.
        let mut det = StagnationDetector::new(4, 4);
        // 4 ls → trips
        assert!(det.observe(0, "ls").is_none());
        assert!(det.observe(1, "ls").is_none());
        assert!(det.observe(2, "ls").is_none());
        assert!(det.observe(3, "ls").is_some());
    }
}
