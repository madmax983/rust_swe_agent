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
    fn test_cost_source_combine() {
        use CostSource::*;

        // Test Unknown dominates
        assert_eq!(Unknown.combine(ProviderReported), Unknown);
        assert_eq!(RateCardEstimate.combine(Unknown), Unknown);
        assert_eq!(Unknown.combine(Unknown), Unknown);

        // Test RateCardEstimate dominates if no Unknown
        assert_eq!(RateCardEstimate.combine(ProviderReported), RateCardEstimate);
        assert_eq!(FreeTierInferred.combine(RateCardEstimate), RateCardEstimate);
        assert_eq!(RateCardEstimate.combine(RateCardEstimate), RateCardEstimate);

        // Test ProviderReported dominates FreeTierInferred
        assert_eq!(ProviderReported.combine(FreeTierInferred), ProviderReported);
        assert_eq!(FreeTierInferred.combine(ProviderReported), ProviderReported);
        assert_eq!(ProviderReported.combine(ProviderReported), ProviderReported);

        // Test FreeTierInferred only combined with itself
        assert_eq!(FreeTierInferred.combine(FreeTierInferred), FreeTierInferred);
    }

    #[test]
    fn test_cost_source_label_display() {
        use CostSource::*;
        assert_eq!(ProviderReported.label(), "provider_reported");
        assert_eq!(RateCardEstimate.label(), "rate_card_estimate");
        assert_eq!(FreeTierInferred.label(), "free_tier_inferred");
        assert_eq!(Unknown.label(), "unknown");

        assert_eq!(ProviderReported.to_string(), "provider_reported");
    }

    #[test]
    fn test_estimate_cost_usd_anthropic() {
        // prompt: 1000, read: 2000, create: 3000, completion: 4000
        // Input cost: 1000 / 1M * 3.0 = 0.003
        // Read cost: 2000 / 1M * 3.0 * 0.10 = 0.0006
        // Create cost: 3000 / 1M * 3.0 * 1.25 = 0.01125
        // Completion cost: 4000 / 1M * 15.0 = 0.06
        // Total = 0.003 + 0.0006 + 0.01125 + 0.06 = 0.07485
        let cost = estimate_cost_usd(1000, 2000, 3000, 4000, "claude-3-5-sonnet");
        assert!((cost - 0.07485).abs() < f64::EPSILON);
    }

    #[test]
    fn test_estimate_cost_usd_non_anthropic() {
        // Cache multipliers are 1.0 for non-anthropic
        // prompt: 1000, read: 2000, create: 3000, completion: 4000
        // Input cost: 1000 / 1M * 3.0 = 0.003
        // Read cost: 2000 / 1M * 3.0 * 1.0 = 0.006
        // Create cost: 3000 / 1M * 3.0 * 1.0 = 0.009
        // Completion cost: 4000 / 1M * 15.0 = 0.06
        // Total = 0.003 + 0.006 + 0.009 + 0.06 = 0.078
        let cost = estimate_cost_usd(1000, 2000, 3000, 4000, "gpt-4o");
        assert!((cost - 0.078).abs() < f64::EPSILON);
    }

    #[test]
    fn test_is_free_tier_model() {
        assert!(is_free_tier_model("google/gemini-pro:free"));
        assert!(is_free_tier_model("gemini-pro:free"));
        assert!(!is_free_tier_model("google/gemini-pro"));
        assert!(!is_free_tier_model("claude-3-5-sonnet"));
    }
}
