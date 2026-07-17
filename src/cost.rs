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

    /// Combines two `CostSource` values, returning the most accurate or pessimistic provenance.
    ///
    /// When aggregating costs from multiple steps in a trajectory, we need a single `CostSource`
    /// to describe the final total. This function acts as a merge operator, downgrading the trust
    /// level if any part of the calculation came from a less reliable source.
    ///
    /// The precedence (from least to most reliable) is:
    /// `Unknown` > `RateCardEstimate` > `ProviderReported` > `FreeTierInferred`
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::cost::CostSource;
    ///
    /// // A known cost mixed with an unknown cost becomes unknown.
    /// assert_eq!(
    ///     CostSource::ProviderReported.combine(CostSource::Unknown),
    ///     CostSource::Unknown
    /// );
    ///
    /// // Estimates downgrade reported costs.
    /// assert_eq!(
    ///     CostSource::ProviderReported.combine(CostSource::RateCardEstimate),
    ///     CostSource::RateCardEstimate
    /// );
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

/// Estimates the cost of an LLM completion in USD based on token usage.
///
/// This provides a baseline cost metric for counterfactual reporting (e.g., "what would this run
/// have cost on a different model?"). It uses standard pricing for `claude-3-5-sonnet` as the
/// baseline, applying specific multipliers if the model is identified as an Anthropic model
/// (which supports explicit caching token charges).
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::cost::estimate_cost_usd;
///
/// // A basic completion without caching.
/// let cost = estimate_cost_usd(1000, 0, 0, 500, "gpt-4o");
/// // Use a small epsilon for floating point comparison
/// assert!((cost - 0.0105).abs() < f64::EPSILON); // (1000 * 3.0 / 1M) + (500 * 15.0 / 1M)
///
/// // Caching applies different multipliers for Anthropic models.
/// let anthropic_cost = estimate_cost_usd(1000, 500, 200, 500, "claude-3-5-sonnet-20241022");
/// assert!(anthropic_cost > 0.0);
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

/// Determines if a model string indicates a free-tier inference provider.
///
/// We use this to correctly attribute `CostSource::FreeTierInferred` when the cost is zero,
/// rather than reporting it as an unknown or missing value. It checks for standard suffixes
/// like `:free` often used by OpenAI/Anthropic proxies or platforms like OpenRouter.
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::cost::is_free_tier_model;
///
/// assert!(is_free_tier_model("google/gemini-pro:free"));
/// assert!(is_free_tier_model("meta-llama/llama-3-8b-instruct:free"));
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
