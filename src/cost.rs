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
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn test_cost_source_label() {
        let cases = vec![
            (CostSource::ProviderReported, "provider_reported"),
            (CostSource::RateCardEstimate, "rate_card_estimate"),
            (CostSource::FreeTierInferred, "free_tier_inferred"),
            (CostSource::Unknown, "unknown"),
        ];
        for (source, expected) in cases {
            assert_eq!(source.label(), expected);
            assert_eq!(source.to_string(), expected);
        }
    }

    #[test]
    fn test_cost_source_combine() {
        let cases = vec![
            (
                CostSource::Unknown,
                CostSource::Unknown,
                CostSource::Unknown,
            ),
            (
                CostSource::Unknown,
                CostSource::ProviderReported,
                CostSource::Unknown,
            ),
            (
                CostSource::ProviderReported,
                CostSource::Unknown,
                CostSource::Unknown,
            ),
            (
                CostSource::RateCardEstimate,
                CostSource::ProviderReported,
                CostSource::RateCardEstimate,
            ),
            (
                CostSource::ProviderReported,
                CostSource::RateCardEstimate,
                CostSource::RateCardEstimate,
            ),
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
            (
                CostSource::FreeTierInferred,
                CostSource::FreeTierInferred,
                CostSource::FreeTierInferred,
            ),
        ];

        for (a, b, expected) in cases {
            assert_eq!(a.combine(b), expected, "combining {a:?} and {b:?} failed");
        }
    }

    #[test]
    fn test_estimate_cost_usd() {
        let cases = vec![
            (1_000_000, 1_000_000, 1_000_000, 1_000_000, "gpt-4o", 24.0),
            (
                1_000_000,
                1_000_000,
                1_000_000,
                1_000_000,
                "claude-3-5-sonnet",
                22.05,
            ),
            (2_000_000, 0, 0, 500_000, "gpt-4", 13.5),
        ];

        for (p, cr, cc, c, model, expected) in cases {
            let cost = estimate_cost_usd(p, cr, cc, c, model);
            assert_eq!(cost, expected, "estimation for {model} failed");
        }
    }

    #[test]
    fn test_is_free_tier_model() {
        let cases = vec![
            ("gemini-1.5-flash:free", true),
            ("openrouter/gemini-1.5-flash:free", true),
            ("gpt-4o", false),
            ("free-model-not-suffix", false),
        ];

        for (model, expected) in cases {
            assert_eq!(
                is_free_tier_model(model),
                expected,
                "free tier check for {model} failed"
            );
        }
    }
}
