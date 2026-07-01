//! Cost accounting helpers shared by trajectories and sweep reports.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::model::litellm::is_anthropic_model;

/// Baseline model used for normalized counterfactual cost reporting.
pub const BASELINE_COST_MODEL: &str = "claude-3-5-sonnet";

/// Standard `claude-3-5-sonnet` USD pricing per 1M tokens. Used for baseline
/// cost estimates; actual run cost is tracked separately when provider/model
/// telemetry supplies it.
pub const SONNET_INPUT_USD_PER_MTOK: f64 = 3.0;
pub const SONNET_OUTPUT_USD_PER_MTOK: f64 = 15.0;
pub const ANTHROPIC_CACHE_READ_MULTIPLIER: f64 = 0.10;
pub const ANTHROPIC_CACHE_CREATION_MULTIPLIER: f64 = 1.25;

/// Provenance for the actual cost number recorded on run artifacts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    ProviderReported,
    RateCardEstimate,
    FreeTierInferred,
    #[default]
    Unknown,
}

impl CostSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider_reported",
            Self::RateCardEstimate => "rate_card_estimate",
            Self::FreeTierInferred => "free_tier_inferred",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub const fn combine(self, next: Self) -> Self {
        match (self, next) {
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::RateCardEstimate, _) | (_, Self::RateCardEstimate) => Self::RateCardEstimate,
            (Self::ProviderReported | Self::FreeTierInferred, Self::ProviderReported)
            | (Self::ProviderReported, Self::FreeTierInferred) => Self::ProviderReported,
            (Self::FreeTierInferred, Self::FreeTierInferred) => Self::FreeTierInferred,
        }
    }
}

impl fmt::Display for CostSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[must_use]
pub fn estimate_cost_usd(
    prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    completion_tokens: u64,
    model: &str,
) -> f64 {
    let (cache_read_multiplier, cache_creation_multiplier) = if is_anthropic_model(model) {
        (
            ANTHROPIC_CACHE_READ_MULTIPLIER,
            ANTHROPIC_CACHE_CREATION_MULTIPLIER,
        )
    } else {
        (1.0, 1.0)
    };
    #[allow(clippy::cast_precision_loss)]
    let p = prompt_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let cr = cache_read_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let cc = cache_creation_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let c = completion_tokens as f64;
    let input_cost = p / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK;
    let cache_read_cost = cr / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK * cache_read_multiplier;
    let cache_creation_cost =
        cc / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK * cache_creation_multiplier;
    let completion_cost = c / 1_000_000.0 * SONNET_OUTPUT_USD_PER_MTOK;
    input_cost + cache_read_cost + cache_creation_cost + completion_cost
}

#[must_use]
pub fn is_free_tier_model(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .is_some_and(|name| name.ends_with(":free"))
        || model.ends_with(":free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_correct_label_and_display() {
        assert_eq!(CostSource::ProviderReported.label(), "provider_reported");
        assert_eq!(CostSource::RateCardEstimate.label(), "rate_card_estimate");
        assert_eq!(CostSource::FreeTierInferred.label(), "free_tier_inferred");
        assert_eq!(CostSource::Unknown.label(), "unknown");

        assert_eq!(
            CostSource::ProviderReported.to_string(),
            "provider_reported"
        );
    }

    #[test]
    fn should_combine_cost_sources_pessimistically() {
        let cases = vec![
            // Unknown dominates everything
            (
                CostSource::Unknown,
                CostSource::ProviderReported,
                CostSource::Unknown,
            ),
            (
                CostSource::Unknown,
                CostSource::RateCardEstimate,
                CostSource::Unknown,
            ),
            (
                CostSource::Unknown,
                CostSource::FreeTierInferred,
                CostSource::Unknown,
            ),
            (
                CostSource::Unknown,
                CostSource::Unknown,
                CostSource::Unknown,
            ),
            (
                CostSource::ProviderReported,
                CostSource::Unknown,
                CostSource::Unknown,
            ),
            (
                CostSource::RateCardEstimate,
                CostSource::Unknown,
                CostSource::Unknown,
            ),
            // RateCardEstimate dominates ProviderReported and FreeTierInferred
            (
                CostSource::RateCardEstimate,
                CostSource::ProviderReported,
                CostSource::RateCardEstimate,
            ),
            (
                CostSource::RateCardEstimate,
                CostSource::FreeTierInferred,
                CostSource::RateCardEstimate,
            ),
            (
                CostSource::RateCardEstimate,
                CostSource::RateCardEstimate,
                CostSource::RateCardEstimate,
            ),
            (
                CostSource::ProviderReported,
                CostSource::RateCardEstimate,
                CostSource::RateCardEstimate,
            ),
            // ProviderReported dominates FreeTierInferred
            (
                CostSource::ProviderReported,
                CostSource::FreeTierInferred,
                CostSource::ProviderReported,
            ),
            (
                CostSource::FreeTierInferred,
                CostSource::ProviderReported,
                CostSource::ProviderReported,
            ),
            (
                CostSource::ProviderReported,
                CostSource::ProviderReported,
                CostSource::ProviderReported,
            ),
            // FreeTierInferred only if both are FreeTierInferred
            (
                CostSource::FreeTierInferred,
                CostSource::FreeTierInferred,
                CostSource::FreeTierInferred,
            ),
        ];

        for (a, b, expected) in cases {
            assert_eq!(
                a.combine(b),
                expected,
                "failed for {a:?} combined with {b:?}"
            );
        }
    }

    #[test]
    fn should_estimate_cost_usd_for_anthropic_model() {
        // Anthropic models get special multipliers: 0.10 for cache read, 1.25 for cache creation.
        // SONNET_INPUT_USD_PER_MTOK = 3.0, SONNET_OUTPUT_USD_PER_MTOK = 15.0
        let p = 1_000_000; // $3.0
        let cr = 1_000_000; // $3.0 * 0.10 = $0.30
        let cc = 1_000_000; // $3.0 * 1.25 = $3.75
        let c = 1_000_000; // $15.0
        let expected = 3.0 + 0.30 + 3.75 + 15.0; // 22.05

        let cost = estimate_cost_usd(p, cr, cc, c, "claude-3-5-sonnet-20241022");
        assert!(
            (cost - expected).abs() < f64::EPSILON,
            "Cost was {cost}, expected {expected}"
        );
    }

    #[test]
    fn should_estimate_cost_usd_for_non_anthropic_model() {
        // Non-Anthropic models use 1.0 multiplier for both cache operations.
        let p = 1_000_000; // $3.0
        let cr = 1_000_000; // $3.0 * 1.0 = $3.0
        let cc = 1_000_000; // $3.0 * 1.0 = $3.0
        let c = 1_000_000; // $15.0
        let expected = 3.0 + 3.0 + 3.0 + 15.0; // 24.0

        let cost = estimate_cost_usd(p, cr, cc, c, "gpt-4o");
        assert!(
            (cost - expected).abs() < f64::EPSILON,
            "Cost was {cost}, expected {expected}"
        );
    }

    #[test]
    fn should_identify_free_tier_model() {
        assert!(is_free_tier_model("google/gemini-2.5-flash:free"));
        assert!(is_free_tier_model("deepseek-r1:free"));
        assert!(is_free_tier_model(":free"));

        assert!(!is_free_tier_model("google/gemini-2.5-flash"));
        assert!(!is_free_tier_model("claude-3-5-sonnet"));
        assert!(!is_free_tier_model("free-model-not-ending-with-free"));
    }
}
