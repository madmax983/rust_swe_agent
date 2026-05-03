//! Adaptive rate-limit governor for SWE-bench sweeps (issue #44).
//!
//! Provides:
//! - Token-bucket enforcement of `--max-rpm` / `--max-input-tpm` ceilings.
//! - Global `Retry-After` floor: when any worker receives a 429 with a
//!   `Retry-After` header, all subsequent `acquire()` calls block until the
//!   floor passes — preventing per-worker retry storms.
//! - AIMD concurrency reduction: on 3 consecutive 429s within 60 s with no
//!   `Retry-After`, effective concurrency is halved for 60 s, then restored
//!   one slot every 30 s.
//! - Telemetry accumulated in `RateLimitEvents` and emitted in `results.json`.
//!
//! The governor is created only when at least one flag is set (`--max-rpm` or
//! `--max-input-tpm`). When neither is provided the whole code path is a no-op.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Per-sweep rate-limit telemetry emitted in `results.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitEvents {
    /// Total number of API calls that were delayed (throttled) by the governor.
    pub throttled_calls: u64,
    /// Cumulative seconds spent waiting across all throttled calls.
    pub total_throttled_seconds: f64,
    /// Peak number of concurrently in-flight tasks observed during the sweep.
    pub peak_concurrent: u32,
    /// Echoed from `--max-rpm`. `None` when the flag was not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_max_rpm: Option<u32>,
    /// Echoed from `--max-input-tpm`. `None` when the flag was not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_max_input_tpm: Option<u64>,
}

struct GovernorInner {
    // ── RPM token bucket ──────────────────────────────────────────────────────
    rpm_tokens: f64,
    rpm_last_refill: Instant,

    // ── Input-TPM token bucket ────────────────────────────────────────────────
    tpm_tokens: f64,
    tpm_last_refill: Instant,

    // ── Global Retry-After floor ──────────────────────────────────────────────
    // Set whenever any worker receives a 429 with a Retry-After header.
    // All workers' next acquire() must wait until this instant.
    retry_after_until: Option<Instant>,

    // ── AIMD state ────────────────────────────────────────────────────────────
    // Count of consecutive 429s in the current 60-second window that had no
    // Retry-After header (a Retry-After resets the counter).
    consecutive_no_ra_429s: u32,
    window_start: Instant,
    // Number of dispatch slots currently suppressed by AIMD.
    suppressed_slots: u32,
    // Next scheduled slot-restoration instant.
    next_slot_restoration: Option<Instant>,

    // ── Telemetry ─────────────────────────────────────────────────────────────
    events: RateLimitEvents,
}

impl GovernorInner {
    fn new(max_rpm: Option<u32>, max_input_tpm: Option<u64>) -> Self {
        let now = Instant::now();
        // Initialise buckets with 1 token so the very first request goes through
        // immediately but the second must wait for the refill interval. This
        // prevents an artificial burst of max_rpm requests on startup.
        let rpm_tokens = if max_rpm.is_some() { 1.0 } else { 0.0 };
        let tpm_tokens = if max_input_tpm.is_some() { 1.0 } else { 0.0 };
        Self {
            rpm_tokens,
            rpm_last_refill: now,
            tpm_tokens,
            tpm_last_refill: now,
            retry_after_until: None,
            consecutive_no_ra_429s: 0,
            window_start: now,
            suppressed_slots: 0,
            next_slot_restoration: None,
            events: RateLimitEvents {
                configured_max_rpm: max_rpm,
                configured_max_input_tpm: max_input_tpm,
                ..Default::default()
            },
        }
    }

    fn refill_rpm(&mut self, max_rpm: u32) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.rpm_last_refill).as_secs_f64();
        let max = f64::from(max_rpm);
        let rate = max / 60.0; // tokens per second
        self.rpm_tokens = f64::mul_add(elapsed, rate, self.rpm_tokens).min(max);
        self.rpm_last_refill = now;
    }

    fn refill_tpm(&mut self, max_tpm: u64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.tpm_last_refill).as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let max = max_tpm as f64;
        let rate = max / 60.0; // tokens per second
        self.tpm_tokens = f64::mul_add(elapsed, rate, self.tpm_tokens).min(max);
        self.tpm_last_refill = now;
    }
}

/// Adaptive rate-limit governor shared across all sweep workers via `Arc`.
///
/// Created by `RateLimitGovernor::new`; returns `None` when neither flag is
/// set so the fast path is a single `Option::is_none()` check.
pub struct RateLimitGovernor {
    max_rpm: Option<u32>,
    max_input_tpm: Option<u64>,
    /// Original sweep parallelism — used to compute the AIMD halved slot count.
    parallelism: u32,
    inner: Mutex<GovernorInner>,
}

impl RateLimitGovernor {
    /// Construct a governor. Returns `None` when both `max_rpm` and
    /// `max_input_tpm` are `None` — opt-in only, no behavior change for
    /// existing users.
    #[must_use]
    pub fn new(max_rpm: Option<u32>, max_input_tpm: Option<u64>, parallelism: u32) -> Option<Self> {
        if max_rpm.is_none() && max_input_tpm.is_none() {
            return None;
        }
        Some(Self {
            max_rpm,
            max_input_tpm,
            parallelism: parallelism.max(1),
            inner: Mutex::new(GovernorInner::new(max_rpm, max_input_tpm)),
        })
    }

    /// Block (not spin) until the governor allows the next API call.
    ///
    /// `input_token_estimate`: best-effort count of input tokens for the
    /// upcoming call (used for TPM accounting). Pass `0` when unknown.
    ///
    /// Must NOT be called while holding any lock that would prevent other
    /// tasks from progressing during the sleep.
    pub async fn acquire(&self, input_token_estimate: u64) {
        loop {
            let sleep_for = self.check_and_maybe_consume(input_token_estimate).await;
            match sleep_for {
                None => break,
                Some(dur) => {
                    let start = Instant::now();
                    tokio::time::sleep(dur).await;
                    let elapsed = start.elapsed().as_secs_f64();
                    let mut inner = self.inner.lock().await;
                    inner.events.total_throttled_seconds += elapsed;
                }
            }
        }
    }

    /// Single check-and-consume pass. Returns the duration to sleep before
    /// retrying, or `None` if the call may proceed (tokens consumed atomically).
    async fn check_and_maybe_consume(&self, input_token_estimate: u64) -> Option<Duration> {
        let mut inner = self.inner.lock().await;

        // Refill buckets based on time elapsed since last call.
        if let Some(rpm) = self.max_rpm {
            inner.refill_rpm(rpm);
        }
        if let Some(tpm) = self.max_input_tpm {
            inner.refill_tpm(tpm);
        }

        let now = Instant::now();

        // 1. Global Retry-After floor takes priority.
        if let Some(until) = inner.retry_after_until {
            if until > now {
                inner.events.throttled_calls += 1;
                return Some(until - now);
            }
            inner.retry_after_until = None;
        }

        // 2. RPM bucket.
        if let Some(rpm) = self.max_rpm {
            if inner.rpm_tokens < 1.0 {
                let deficit = 1.0 - inner.rpm_tokens;
                let rate = f64::from(rpm) / 60.0;
                inner.events.throttled_calls += 1;
                return Some(Duration::from_secs_f64(deficit / rate));
            }
        }

        // 3. Input-TPM bucket (only gated when estimate is non-zero).
        if let Some(tpm) = self.max_input_tpm {
            #[allow(clippy::cast_precision_loss)]
            let needed = input_token_estimate as f64;
            if needed > 0.0 && inner.tpm_tokens < needed {
                let deficit = needed - inner.tpm_tokens;
                #[allow(clippy::cast_precision_loss)]
                let rate = tpm as f64 / 60.0;
                inner.events.throttled_calls += 1;
                return Some(Duration::from_secs_f64(deficit / rate));
            }
        }

        // All clear — consume tokens atomically before releasing the lock.
        if self.max_rpm.is_some() {
            inner.rpm_tokens -= 1.0;
        }
        if self.max_input_tpm.is_some() {
            #[allow(clippy::cast_precision_loss)]
            let tokens = input_token_estimate as f64;
            inner.tpm_tokens -= tokens;
        }
        None
    }

    /// Report a 429 response from the provider.
    ///
    /// - `retry_after_secs`: parsed from the `Retry-After` header, or `None`
    ///   if the header was absent. A `Some` value sets the global floor for
    ///   *all* workers and resets the AIMD counter. A `None` value increments
    ///   the AIMD counter; three in a 60-second window halves concurrency.
    pub async fn report_429(&self, retry_after_secs: Option<u64>) {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();

        inner.events.throttled_calls += 1;

        if let Some(secs) = retry_after_secs {
            let until = now + Duration::from_secs(secs);
            // Keep the furthest-future floor if multiple workers race.
            match inner.retry_after_until {
                Some(existing) if existing >= until => {}
                _ => inner.retry_after_until = Some(until),
            }
            // Retry-After resets the AIMD window — the provider is giving
            // explicit guidance, so AIMD storm counting should restart.
            inner.consecutive_no_ra_429s = 0;
            inner.window_start = now;
        } else {
            // No Retry-After — track toward AIMD threshold.
            if now.duration_since(inner.window_start) > Duration::from_secs(60) {
                // Previous window expired; start a new one.
                inner.consecutive_no_ra_429s = 1;
                inner.window_start = now;
            } else {
                inner.consecutive_no_ra_429s += 1;
            }

            if inner.consecutive_no_ra_429s >= 3 && inner.suppressed_slots == 0 {
                // Suppress half the slots, but always leave at least one active
                // so the JoinSet never drains to empty with pending work remaining.
                let halve = (self.parallelism / 2).min(self.parallelism.saturating_sub(1));
                inner.suppressed_slots = halve;
                // Hold period: 60 s. First restoration after hold + 30 s.
                inner.next_slot_restoration =
                    Some(now + Duration::from_secs(60) + Duration::from_secs(30));
                inner.consecutive_no_ra_429s = 0;
                inner.window_start = now;
                let parallelism = self.parallelism;
                drop(inner);
                tracing::warn!(
                    suppressed_slots = halve,
                    parallelism,
                    "rate-limit: AIMD triggered — halving effective concurrency"
                );
            }
        }
    }

    /// Check whether AIMD slot-restoration is due and apply it. Returns `true`
    /// when a slot was restored (caller should emit a structured log event).
    ///
    /// Call this from the consumer loop before each spawn decision so that
    /// restoration happens as task completions free up slots naturally.
    pub async fn tick_aimd(&self) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.suppressed_slots == 0 {
            return false;
        }
        let now = Instant::now();
        if let Some(restore_at) = inner.next_slot_restoration {
            if now >= restore_at {
                inner.suppressed_slots -= 1;
                inner.next_slot_restoration = if inner.suppressed_slots > 0 {
                    Some(now + Duration::from_secs(30))
                } else {
                    None
                };
                return true;
            }
        }
        false
    }

    /// Returns the number of dispatch slots currently suppressed by AIMD.
    /// The consumer loop should only spawn when `in_flight < parallelism - suppressed`.
    pub async fn suppressed_slots_count(&self) -> u32 {
        self.inner.lock().await.suppressed_slots
    }

    /// Update the peak-concurrent counter. Call with the current in-flight
    /// task count after each spawn.
    pub async fn update_peak_concurrent(&self, count: u32) {
        let mut inner = self.inner.lock().await;
        if count > inner.events.peak_concurrent {
            inner.events.peak_concurrent = count;
        }
    }

    /// Snapshot of accumulated telemetry. Called once at sweep completion.
    pub async fn events(&self) -> RateLimitEvents {
        let inner = self.inner.lock().await;
        inner.events.clone()
    }

    /// Parse a `Retry-After` duration (in seconds) from a provider error
    /// message string. Handles:
    /// - Numeric: `"retry-after: 30"` / `"retry_after: 30"`
    /// - HTTP-date: `"retry-after: Wed, 21 Oct 2026 12:00:00 GMT"`
    /// Returns `None` when no recognizable pattern is found.
    #[must_use]
    pub fn parse_retry_after_from_error(msg: &str) -> Option<u64> {
        let lower = msg.to_lowercase();
        for prefix in ["retry-after: ", "retry_after: ", "retry after: "] {
            if let Some(pos) = lower.find(prefix) {
                let rest = msg[pos + prefix.len()..].trim();
                // Try numeric seconds first.
                let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(secs) = num.parse::<u64>() {
                    return Some(secs);
                }
                // Try HTTP-date: "Wed, 21 Oct 2026 12:00:00 GMT"
                if let Some(secs) = parse_http_date_secs_from_now(rest) {
                    return Some(secs);
                }
            }
        }
        None
    }
}

/// Parse an RFC 7231 HTTP-date string and return seconds until that instant.
/// Returns `None` on parse failure or if the date is in the past.
/// Format: `<day-name>, <day> <month> <year> <HH>:<MM>:<SS> GMT`
fn parse_http_date_secs_from_now(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    // ["Wed,", "21", "Oct", "2026", "12:00:00", "GMT"]
    if parts.len() != 6 || !parts[5].eq_ignore_ascii_case("gmt") {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month: i64 = match parts[2].to_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let time: Vec<&str> = parts[4].split(':').collect();
    if time.len() != 3 {
        return None;
    }
    let h: u64 = time[0].parse().ok()?;
    let m: u64 = time[1].parse().ok()?;
    let sc: u64 = time[2].parse().ok()?;
    let target_unix = civil_to_unix(year, month, day, h, m, sc)?;
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(target_unix.saturating_sub(now_unix))
}

/// Convert a proleptic Gregorian civil date + time to a Unix timestamp (seconds
/// since 1970-01-01T00:00:00Z). Uses Howard Hinnant's days-since-epoch formula.
pub(crate) fn civil_to_unix(
    year: i64,
    month: i64,
    day: i64,
    h: u64,
    m: u64,
    s: u64,
) -> Option<u64> {
    // Shift so March is month 1, to simplify leap-day arithmetic.
    let (y, mp) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // year-of-era [0, 399]
    let doy = (153 * mp + 2) / 5 + day - 1; // day-of-year [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // day-of-era [0, 146096]
    let days: i64 = era * 146_097 + doe - 719_468; // days since 1970-01-01
    if days < 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    let total = days as u64 * 86_400 + h * 3_600 + m * 60 + s;
    Some(total)
}
