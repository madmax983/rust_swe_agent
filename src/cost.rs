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
    /// Returns the string representation of this [`CostSource`] for telemetry.
    ///
    /// The string representation is used when exporting data to metrics or reporting systems.
    ///
    /// # Examples
    ///
    /// ```
    /// use maxwells_daemon::cost::CostSource;
    ///
    /// assert_eq!(CostSource::ProviderReported.label(), "provider_reported");
    /// assert_eq!(CostSource::FreeTierInferred.label(), "free_tier_inferred");
    /// ```
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider_reported",
            Self::RateCardEstimate => "rate_card_estimate",
            Self::FreeTierInferred => "free_tier_inferred",
            Self::Unknown => "unknown",
        }
    }

    /// Merges two [`CostSource`] variants, favoring the broader or less reliable provenance.
    ///
    /// This is useful when aggregating cost across multiple inference calls. If any call's cost
    /// provenance is `Unknown`, the aggregate is `Unknown`. If any is `RateCardEstimate` (and none `Unknown`),
    /// the aggregate is `RateCardEstimate`. `ProviderReported` is only preserved if all calls were reported
    /// or inferred as free tier.
    ///
    /// # Examples
    ///
    /// ```
    /// use maxwells_daemon::cost::CostSource;
    ///
    /// let provider = CostSource::ProviderReported;
    /// let estimated = CostSource::RateCardEstimate;
    ///
    /// assert_eq!(provider.combine(estimated), CostSource::RateCardEstimate);
    /// assert_eq!(CostSource::Unknown.combine(estimated), CostSource::Unknown);
    /// ```
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

/// Calculates the estimated cost of an inference request in USD.
///
/// Converts token counts to their estimated dollar cost based on the standard `claude-3-5-sonnet`
/// rate card or model-specific multipliers. This helps project the financial impact of prompts and caching
/// even if the provider does not include billing data directly in the response.
///
/// # Examples
///
/// ```
/// use maxwells_daemon::cost::estimate_cost_usd;
///
/// let cost = estimate_cost_usd(1_000_000, 0, 0, 1_000_000, "claude-3-5-sonnet-20241022");
/// // 1M prompt tokens ($3.00) + 1M completion tokens ($15.00)
/// assert!((cost - 18.0).abs() < 1e-6);
/// ```
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

/// Checks whether the model identifier string belongs to a free-tier API.
///
/// By identifying free-tier models (which typically end in `:free`), the system can prevent attributing
/// artificial costs to experiments that did not actually spend budget.
///
/// # Examples
///
/// ```
/// use maxwells_daemon::cost::is_free_tier_model;
///
/// assert!(is_free_tier_model("openrouter/deepseek/deepseek-chat-v3.1:free"));
/// assert!(!is_free_tier_model("claude-3-5-sonnet"));
/// ```
#[must_use]
pub fn is_free_tier_model(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .is_some_and(|name| name.ends_with(":free"))
        || model.ends_with(":free")
}
