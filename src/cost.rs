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
    fn should_return_correct_label_and_display_for_cost_source() {
        assert_eq!(CostSource::ProviderReported.label(), "provider_reported");
        assert_eq!(
            CostSource::ProviderReported.to_string(),
            "provider_reported"
        );

        assert_eq!(CostSource::RateCardEstimate.label(), "rate_card_estimate");
        assert_eq!(
            CostSource::RateCardEstimate.to_string(),
            "rate_card_estimate"
        );

        assert_eq!(CostSource::FreeTierInferred.label(), "free_tier_inferred");
        assert_eq!(
            CostSource::FreeTierInferred.to_string(),
            "free_tier_inferred"
        );

        assert_eq!(CostSource::Unknown.label(), "unknown");
        assert_eq!(CostSource::Unknown.to_string(), "unknown");
    }

    #[test]
    fn should_combine_cost_sources_correctly() {
        // Unknown contaminates everything
        assert_eq!(
            CostSource::Unknown.combine(CostSource::ProviderReported),
            CostSource::Unknown
        );
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::Unknown),
            CostSource::Unknown
        );
        assert_eq!(
            CostSource::Unknown.combine(CostSource::RateCardEstimate),
            CostSource::Unknown
        );
        assert_eq!(
            CostSource::Unknown.combine(CostSource::FreeTierInferred),
            CostSource::Unknown
        );
        assert_eq!(
            CostSource::Unknown.combine(CostSource::Unknown),
            CostSource::Unknown
        );

        // RateCardEstimate contaminates everything except Unknown
        assert_eq!(
            CostSource::RateCardEstimate.combine(CostSource::ProviderReported),
            CostSource::RateCardEstimate
        );
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::RateCardEstimate),
            CostSource::RateCardEstimate
        );
        assert_eq!(
            CostSource::RateCardEstimate.combine(CostSource::FreeTierInferred),
            CostSource::RateCardEstimate
        );
        assert_eq!(
            CostSource::RateCardEstimate.combine(CostSource::RateCardEstimate),
            CostSource::RateCardEstimate
        );

        // ProviderReported vs FreeTierInferred -> ProviderReported
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::FreeTierInferred),
            CostSource::ProviderReported
        );
        assert_eq!(
            CostSource::FreeTierInferred.combine(CostSource::ProviderReported),
            CostSource::ProviderReported
        );

        // Same with same
        assert_eq!(
            CostSource::ProviderReported.combine(CostSource::ProviderReported),
            CostSource::ProviderReported
        );
        assert_eq!(
            CostSource::FreeTierInferred.combine(CostSource::FreeTierInferred),
            CostSource::FreeTierInferred
        );
    }

    #[test]
    fn should_estimate_cost_usd_for_anthropic() {
        let cost = estimate_cost_usd(
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
            "claude-3-5-sonnet",
        );
        // 3.0 (prompt) + 3.0 * 0.1 (read) + 3.0 * 1.25 (creation) + 15.0 (completion)
        // 3.0 + 0.3 + 3.75 + 15.0 = 22.05
        assert!((cost - 22.05).abs() < f64::EPSILON);
    }

    #[test]
    fn should_estimate_cost_usd_for_non_anthropic() {
        let cost = estimate_cost_usd(1_000_000, 1_000_000, 1_000_000, 1_000_000, "gpt-4o");
        // 3.0 (prompt) + 3.0 * 1.0 (read) + 3.0 * 1.0 (creation) + 15.0 (completion)
        // 3.0 + 3.0 + 3.0 + 15.0 = 24.0
        assert!((cost - 24.0).abs() < f64::EPSILON);
    }

    #[test]
    fn should_identify_free_tier_model() {
        assert!(is_free_tier_model("google/gemini-1.5-pro:free"));
        assert!(is_free_tier_model("gemini-1.5-pro:free"));
        assert!(!is_free_tier_model("google/gemini-1.5-pro"));
        assert!(!is_free_tier_model("claude-3-5-sonnet"));
        assert!(!is_free_tier_model("google/gemini-1.5-pro:free-tier"));
    }
}
