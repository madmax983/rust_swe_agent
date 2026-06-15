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
    fn should_return_correct_label_for_cost_source() {
        assert_eq!(CostSource::ProviderReported.label(), "provider_reported");
        assert_eq!(CostSource::RateCardEstimate.label(), "rate_card_estimate");
        assert_eq!(CostSource::FreeTierInferred.label(), "free_tier_inferred");
        assert_eq!(CostSource::Unknown.label(), "unknown");
    }

    #[test]
    fn should_combine_cost_sources_correctly() {
        // Unknown combinations
        assert_eq!(
            CostSource::Unknown.combine(CostSource::ProviderReported),
            CostSource::Unknown
        );
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::Unknown),
            CostSource::Unknown
        );

        // RateCardEstimate overrides ProviderReported and FreeTierInferred
        assert_eq!(
            CostSource::RateCardEstimate.combine(CostSource::ProviderReported),
            CostSource::RateCardEstimate
        );
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::RateCardEstimate),
            CostSource::RateCardEstimate
        );

        // ProviderReported vs FreeTierInferred
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::FreeTierInferred),
            CostSource::ProviderReported
        );
        assert_eq!(
            CostSource::FreeTierInferred.combine(CostSource::ProviderReported),
            CostSource::ProviderReported
        );

        // Same sources
        assert_eq!(
            CostSource::FreeTierInferred.combine(CostSource::FreeTierInferred),
            CostSource::FreeTierInferred
        );
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::ProviderReported),
            CostSource::ProviderReported
        );
    }

    #[test]
    fn should_estimate_cost_usd_accurately_for_non_anthropic() {
        let cost = estimate_cost_usd(1_000_000, 1_000_000, 1_000_000, 1_000_000, "gpt-4o");
        // Non-anthropic multipliers are 1.0.
        // 1M prompt = $3.0
        // 1M cache_read = $3.0 * 1.0 = $3.0
        // 1M cache_creation = $3.0 * 1.0 = $3.0
        // 1M completion = $15.0
        // Total = 3 + 3 + 3 + 15 = 24.0
        let expected = 24.0;
        assert!(
            (cost - expected).abs() < 1e-6,
            "Expected {expected}, got {cost}"
        );
    }

    #[test]
    fn should_estimate_cost_usd_accurately_for_anthropic() {
        let cost = estimate_cost_usd(
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
            "claude-3-5-sonnet",
        );
        // Anthropic multipliers are 0.10 for cache_read, 1.25 for cache_creation.
        // 1M prompt = $3.0
        // 1M cache_read = $3.0 * 0.10 = $0.30
        // 1M cache_creation = $3.0 * 1.25 = $3.75
        // 1M completion = $15.0
        // Total = 3 + 0.30 + 3.75 + 15 = 22.05
        let expected = 22.05;
        assert!(
            (cost - expected).abs() < 1e-6,
            "Expected {expected}, got {cost}"
        );
    }

    #[test]
    fn should_detect_free_tier_models() {
        let test_cases = vec![
            ("google/gemini-pro:free", true),
            ("openrouter/auto:free", true),
            ("llama3:free", true),
            ("gpt-4o", false),
            ("claude-3-5-sonnet", false),
            ("anthropic/claude-3-opus", false),
        ];

        for (model, expected) in test_cases {
            assert_eq!(
                is_free_tier_model(model),
                expected,
                "Failed on model: {model}"
            );
        }
    }
}
