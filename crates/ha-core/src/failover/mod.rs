// ── Model Failover: Error Classification & Auth Profile Rotation ───
//
//  Classifies API errors to determine whether to retry the same model,
//  fall back to the next model, or surface the error directly.
//  Also provides per-profile cooldown tracking and session-sticky
//  profile selection for multi-key rotation within a single provider.
//
//  ## Submodules
//
//  - [`executor`] (Phase 3): generic `execute_with_failover` wrapper that
//    lifts the inline rotation + retry + cooldown orchestration out of
//    `chat_engine` so one-shot paths (side_query / summarize_direct) can
//    opt in too.

pub mod executor;

use serde::Serialize;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use crate::provider::{AuthProfile, ProviderConfig};

/// Why a model request failed — drives retry / fallback decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverReason {
    /// 429 Too Many Requests — retryable on same model
    RateLimit,
    /// 503 Service Unavailable / overloaded — retryable on same model
    Overloaded,
    /// Request timeout or connection error — retryable on same model
    Timeout,
    /// 401 Unauthorized / invalid API key — skip to next model
    Auth,
    /// 402 Payment Required / quota exhausted — skip to next model
    Billing,
    /// 404 Model not found — skip to next model
    ModelNotFound,
    /// Context window exceeded — NOT fallback-able (smaller model would be worse)
    ContextOverflow,
    /// Unrecognized error — retry once, then skip to next model
    Unknown,
}

// ── Structured LLM Error ────────────────────────────────────────

/// Structured error metadata from LLM API responses.
///
/// Adapters can construct this to carry HTTP status code and error
/// metadata through the pipeline deterministically, instead of relying
/// on substring matching in [`classify_error`]. When an anyhow error
/// message contains the `LLM_ERR:` prefix (produced by
/// [`LlmError::to_anyhow`]), [`classify_error`] uses the structured
/// data directly; otherwise it falls back to substring matching.
///
/// This is opt-in — adapters that don't construct `LlmError` still
/// work via the substring fallback.
#[derive(Debug, Clone, Serialize)]
pub struct LlmError {
    /// HTTP status code from the API response (if available).
    pub status: Option<u16>,
    /// Provider-specific error code (e.g. "rate_limit_error", "context_length_exceeded").
    pub code: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Retry-After header value in seconds (if the server specified one).
    pub retry_after_secs: Option<u64>,
}

impl LlmError {
    /// Construct a new structured LLM error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            status: None,
            code: None,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// Construct from an HTTP response status code and error body.
    pub fn from_http_status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            code: None,
            message: body.into(),
            retry_after_secs: None,
        }
    }

    /// Set the provider error code.
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Set the Retry-After value from the server response.
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// Parse the `Retry-After` header value.
    ///
    /// Supports both integer seconds (e.g. `"30"`) and HTTP-date format
    /// (returns `None` for the latter since it requires clock comparison).
    pub fn parse_retry_after(value: &str) -> Option<u64> {
        let trimmed = value.trim();
        // Try integer seconds first (most common format)
        if let Ok(secs) = trimmed.parse::<u64>() {
            return Some(secs);
        }
        // HTTP-date format (e.g. "Fri, 28 Jun 2026 12:00:00 GMT") — skip
        None
    }

    /// Convert to an anyhow error with a structured prefix that
    /// [`classify_error`] can detect and parse deterministically.
    ///
    /// Format: `LLM_ERR:{status}:{code}:{retry_after}:{message}`
    /// All fields are separated by `:`; missing fields use `-`.
    pub fn to_anyhow(self) -> anyhow::Error {
        let status = self
            .status
            .map_or_else(|| "-".to_string(), |s| s.to_string());
        let code = self.code.as_deref().unwrap_or("-");
        let retry_after = self
            .retry_after_secs
            .map_or_else(|| "-".to_string(), |s| s.to_string());
        anyhow::anyhow!(
            "LLM_ERR:{}:{}:{}:{}",
            status,
            code,
            retry_after,
            self.message
        )
    }
}

/// Sentinel prefix for structured LLM errors.
const LLM_ERR_PREFIX: &str = "LLM_ERR:";

/// Result of error classification, including server-advised retry delay.
#[derive(Debug, Clone)]
pub struct ClassifiedError {
    /// The classified failover reason.
    pub reason: FailoverReason,
    /// Server-advised retry delay in seconds (from `Retry-After` header).
    /// `None` when the server did not specify a delay.
    pub retry_after_secs: Option<u64>,
}

/// Classify an error, preferring structured `LlmError` data when available.
///
/// If the error message starts with [`LLM_ERR_PREFIX`], the structured
/// fields are used for deterministic classification. Otherwise, the
/// legacy substring-matching fallback is applied.
pub fn classify_error(error_msg: &str) -> FailoverReason {
    classify_error_full(error_msg).reason
}

/// Classify an error with full metadata (including server-advised retry delay).
///
/// Use this when the caller needs the `Retry-After` value for adaptive backoff.
pub fn classify_error_full(error_msg: &str) -> ClassifiedError {
    // ── Structured path: parse LlmError prefix ────────────────────
    if let Some(rest) = error_msg.strip_prefix(LLM_ERR_PREFIX) {
        return classify_structured_error_full(rest);
    }

    // ── Legacy fallback: substring matching ───────────────────────
    ClassifiedError {
        reason: classify_error_by_substring(error_msg),
        retry_after_secs: None,
    }
}

/// Classify from the structured `LLM_ERR` fields, extracting retry_after.
fn classify_structured_error_full(fields: &str) -> ClassifiedError {
    // Format: {status}:{code}:{retry_after}:{message}
    let parts: Vec<&str> = fields.splitn(4, ':').collect();
    if parts.len() < 4 {
        // Malformed structured error — fall back to substring matching
        return ClassifiedError {
            reason: classify_error_by_substring(fields),
            retry_after_secs: None,
        };
    }

    let status = parts[0];
    let code = parts[1];
    let retry_after_str = parts[2];
    let message = parts[3];

    // Extract retry_after if present
    let retry_after_secs = if retry_after_str != "-" {
        retry_after_str.parse::<u64>().ok()
    } else {
        None
    };

    // ── Status-based classification (deterministic) ──────────────
    if status != "-" {
        if let Ok(status_code) = status.parse::<u16>() {
            match status_code {
                429 => {
                    return ClassifiedError {
                        reason: FailoverReason::RateLimit,
                        retry_after_secs,
                    }
                }
                502 | 503 | 521 | 522 | 524 => {
                    return ClassifiedError {
                        reason: FailoverReason::Overloaded,
                        retry_after_secs,
                    }
                }
                401 | 403 => {
                    return ClassifiedError {
                        reason: FailoverReason::Auth,
                        retry_after_secs,
                    }
                }
                402 => {
                    return ClassifiedError {
                        reason: FailoverReason::Billing,
                        retry_after_secs,
                    }
                }
                404 => {
                    return ClassifiedError {
                        reason: FailoverReason::ModelNotFound,
                        retry_after_secs,
                    }
                }
                _ => {}
            }
        }
    }

    // ── Code-based classification ────────────────────────────────
    if code != "-" {
        let lower_code = code.to_lowercase();
        if lower_code.contains("context_length_exceeded")
            || lower_code.contains("context_overflow")
            || lower_code.contains("prompt_too_long")
        {
            return ClassifiedError {
                reason: FailoverReason::ContextOverflow,
                retry_after_secs,
            };
        }
        if lower_code.contains("rate_limit")
            || lower_code.contains("resource_exhausted")
            || lower_code.contains("throttle")
        {
            return ClassifiedError {
                reason: FailoverReason::RateLimit,
                retry_after_secs,
            };
        }
        if lower_code.contains("overloaded") || lower_code.contains("server_error") {
            return ClassifiedError {
                reason: FailoverReason::Overloaded,
                retry_after_secs,
            };
        }
        if lower_code.contains("auth")
            || lower_code.contains("invalid_api_key")
            || lower_code.contains("forbidden")
        {
            return ClassifiedError {
                reason: FailoverReason::Auth,
                retry_after_secs,
            };
        }
        if lower_code.contains("billing")
            || lower_code.contains("quota")
            || lower_code.contains("insufficient")
        {
            return ClassifiedError {
                reason: FailoverReason::Billing,
                retry_after_secs,
            };
        }
        if lower_code.contains("model_not_found") || lower_code.contains("does_not_exist") {
            return ClassifiedError {
                reason: FailoverReason::ModelNotFound,
                retry_after_secs,
            };
        }
    }

    // ── Message-based fallback (same as legacy, but on message only) ──
    ClassifiedError {
        reason: classify_error_by_substring(message),
        retry_after_secs,
    }
}

impl FailoverReason {
    /// Whether this error class should be retried on the **same** model
    /// (with backoff) before moving to the next model in the chain.
    ///
    /// `Unknown` errors get one retry — many are transient (proxy gateway
    /// hiccups, DNS blips, non-standard connection resets) and a single
    /// retry catches the majority without adding excessive latency on
    /// truly permanent failures.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Overloaded | Self::Timeout | Self::Unknown
        )
    }

    /// Maximum retry attempts for this error class.
    /// `Unknown` errors get only 1 attempt (vs 2 for RateLimit/Overloaded/Timeout)
    /// to limit latency on permanent failures while still catching transients.
    pub fn max_retries(&self, policy_max: u32) -> u32 {
        match self {
            Self::Unknown => policy_max.min(1),
            _ => policy_max,
        }
    }

    /// Whether this error should immediately surface to the user
    /// without trying any fallback models.
    /// Note: ContextOverflow is no longer terminal — it triggers compaction first.
    pub fn is_terminal(&self) -> bool {
        false
    }

    /// Whether this error should trigger context compaction before retry.
    pub fn needs_compaction(&self) -> bool {
        matches!(self, Self::ContextOverflow)
    }

    /// Whether this error class should trigger rotation to the next auth profile
    /// within the same provider before falling through to model-level failover.
    pub fn is_profile_rotatable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Overloaded | Self::Auth | Self::Billing
        )
    }

    /// Get the cooldown duration (in seconds) for this error type when applied
    /// to a per-profile cooldown.
    pub fn profile_cooldown_secs(&self) -> u64 {
        match self {
            Self::Overloaded => 30,
            Self::RateLimit => 60,
            Self::Auth => 300,
            Self::Billing => 600,
            _ => 0,
        }
    }
}

// ── Error Classification ──────────────────────────────────────────

// Regex-style patterns for error classification.
// We use simple substring matching for performance.

/// Legacy substring-based error classification.
/// Used as fallback when structured `LlmError` data is not available.
fn classify_error_by_substring(error_msg: &str) -> FailoverReason {
    let lower = error_msg.to_lowercase();

    // ── Context overflow (terminal — never fallback) ──────────────
    if is_context_overflow(&lower) {
        return FailoverReason::ContextOverflow;
    }

    // ── Rate limit (retryable) ────────────────────────────────────
    if lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
        || lower.contains("resource_exhausted")
        || lower.contains("throttl")
    {
        return FailoverReason::RateLimit;
    }

    // ── Overloaded (retryable) ────────────────────────────────────
    if lower.contains("503")
        || lower.contains("overloaded")
        || lower.contains("service unavailable")
        || lower.contains("temporarily unavailable")
        || lower.contains("server_error")
        || lower.contains("internal server error")
        || lower.contains("an error occurred while processing your request")
        || lower.contains("502")  // Bad Gateway
        || lower.contains("521")  // Cloudflare origin down
        || lower.contains("522")  // Cloudflare connection timed out
        || lower.contains("524")
    // Cloudflare timeout
    {
        return FailoverReason::Overloaded;
    }

    // ── Timeout / transport error (retryable) ─────────────────────
    // Includes reqwest/hyper decode failures which typically occur when
    // the server closes a chunked/SSE response body mid-stream after
    // returning 200 headers (seen on dashscope-coding under load).
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("etimedout")
        || lower.contains("econnreset")
        || lower.contains("econnrefused")
        || lower.contains("econnaborted")
        || lower.contains("enetunreach")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("broken pipe")
        || lower.contains("error decoding response body")
        || lower.contains("error reading a body from connection")
        || lower.contains("incomplete message")
        || lower.contains("unexpected eof")
        || lower.contains("connection closed before message completed")
        || lower.contains("no content received")
        || lower.contains("sse stream ended with no content")
    {
        return FailoverReason::Timeout;
    }

    // ── Auth (skip to next model) ─────────────────────────────────
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("authentication")
        || lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("permission denied")
    {
        return FailoverReason::Auth;
    }

    // ── Billing (skip to next model) ──────────────────────────────
    if lower.contains("402")
        || lower.contains("payment required")
        || lower.contains("billing")
        || lower.contains("quota")
        || lower.contains("insufficient_quota")
        || lower.contains("exceeded your current quota")
    {
        return FailoverReason::Billing;
    }

    // ── Model not found (skip to next model) ──────────────────────
    if lower.contains("404")
        || lower.contains("model not found")
        || lower.contains("model_not_found")
        || lower.contains("does not exist")
        || lower.contains("not_found_error")
    {
        return FailoverReason::ModelNotFound;
    }

    FailoverReason::Unknown
}

/// Check if an error message indicates context window overflow.
/// These errors should NEVER trigger model fallback — a smaller context
/// window model would produce an even worse result.
fn is_context_overflow(lower: &str) -> bool {
    lower.contains("context length exceeded")
        || lower.contains("context_length_exceeded")
        || lower.contains("context window")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("token limit")
        || lower.contains("max_tokens") && (lower.contains("exceed") || lower.contains("too large"))
        || lower.contains("input too long")
        || lower.contains("request too large")
}

// ── Retry with Backoff ────────────────────────────────────────────

/// Compute delay for retry attempt `attempt` (0-indexed).
/// Uses exponential backoff: base_ms * 2^attempt, clamped to max_ms,
/// plus random jitter up to ±10%.
pub fn retry_delay_ms(attempt: u32, base_ms: u64, max_ms: u64) -> u64 {
    let delay = base_ms.saturating_mul(2u64.saturating_pow(attempt));
    let clamped = delay.min(max_ms);
    // Simple jitter: ±10%
    let jitter_range = clamped / 10;
    if jitter_range == 0 {
        return clamped;
    }
    let jitter = (rand_simple() % (jitter_range * 2 + 1)) as i64 - jitter_range as i64;
    (clamped as i64 + jitter).max(0) as u64
}

/// Simple pseudo-random number (no external crate needed).
/// Uses both nanos and a thread-local counter to avoid bias from
/// rapid successive calls that may share the same nanosecond value.
fn rand_simple() -> u64 {
    use std::cell::Cell;
    use std::time::SystemTime;
    thread_local! {
        static COUNTER: Cell<u64> = const { Cell::new(0) };
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let count = COUNTER.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v
    });
    // Mix nanos with counter using a simple hash-like operation
    nanos ^ (count.wrapping_mul(6364136223846793005))
}

// ── Auth Profile Cooldown Tracking ────────────────────────────────
//
//  Runtime-only state (not persisted). Tracks per-profile cooldowns after
//  rate-limit / auth / billing errors to avoid retrying a known-bad key.

struct CooldownEntry {
    until: Instant,
}

/// Global per-profile cooldown tracker.
pub struct ProfileCooldownTracker {
    entries: Mutex<HashMap<String, CooldownEntry>>,
}

impl ProfileCooldownTracker {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Mark a profile as cooled down for the given duration.
    pub fn mark_cooldown(&self, profile_id: &str, reason: &FailoverReason) {
        let secs = reason.profile_cooldown_secs();
        if secs == 0 {
            return;
        }
        if let Ok(mut map) = self.entries.lock() {
            // Prune expired entries opportunistically (cap at 100 to avoid unbounded growth)
            if map.len() > 100 {
                let now = Instant::now();
                map.retain(|_, e| now < e.until);
            }
            map.insert(
                profile_id.to_string(),
                CooldownEntry {
                    until: Instant::now() + std::time::Duration::from_secs(secs),
                },
            );
        }
    }

    /// Check if a profile is available (not in cooldown).
    pub fn is_available(&self, profile_id: &str) -> bool {
        if let Ok(map) = self.entries.lock() {
            match map.get(profile_id) {
                Some(entry) => Instant::now() >= entry.until,
                None => true,
            }
        } else {
            true
        }
    }

    /// Filter a list of profiles to only those not currently in cooldown.
    /// Acquires the lock once for all profiles.
    pub fn filter_available(&self, profiles: &[AuthProfile]) -> Vec<AuthProfile> {
        let now = Instant::now();
        if let Ok(map) = self.entries.lock() {
            profiles
                .iter()
                .filter(|p| map.get(&p.id).map_or(true, |e| now >= e.until))
                .cloned()
                .collect()
        } else {
            profiles.to_vec()
        }
    }

    /// Clear the cooldown for a profile (e.g. on success).
    pub fn clear(&self, profile_id: &str) {
        if let Ok(mut map) = self.entries.lock() {
            map.remove(profile_id);
        }
    }
}

pub static PROFILE_COOLDOWNS: LazyLock<ProfileCooldownTracker> =
    LazyLock::new(ProfileCooldownTracker::new);

// ── Session Profile Stickiness ───────────────────────────────────
//
//  Maps (provider_id, session_id) → last-successful profile_id.
//  Ensures cache-friendly behavior by preferring the same key across turns.

/// Per-provider LRU of (session_id → profile_id). We need insertion-order
/// tracking for O(1) "evict oldest" semantics without pulling a full LRU
/// crate, so sessions live in a side `VecDeque` alongside the map: `get`
/// looks up in the map, `set` promotes the key to the back, eviction drops
/// the front. Keeps the whole map bounded without the old "blow away
/// everything at 500" behavior that destroyed session stickiness for
/// every long-running process.
#[derive(Default)]
struct StickyShard {
    map: HashMap<String, String>,
    order: std::collections::VecDeque<String>,
}

impl StickyShard {
    fn promote(&mut self, session_id: &str) {
        if let Some(pos) = self.order.iter().position(|s| s == session_id) {
            self.order.remove(pos);
        }
        self.order.push_back(session_id.to_string());
    }

    fn insert(&mut self, session_id: &str, profile_id: &str, max: usize) {
        self.map
            .insert(session_id.to_string(), profile_id.to_string());
        self.promote(session_id);
        while self.order.len() > max {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
    }
}

pub struct ProfileStickyMap {
    map: Mutex<HashMap<String, StickyShard>>,
}

/// Cap per-provider session entries to prevent unbounded growth.
const STICKY_MAX_SESSIONS_PER_PROVIDER: usize = 500;

impl ProfileStickyMap {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Get the sticky profile ID for a provider+session pair.
    pub fn get(&self, provider_id: &str, session_id: &str) -> Option<String> {
        let mut guard = self.map.lock().ok()?;
        let shard = guard.get_mut(provider_id)?;
        let profile = shard.map.get(session_id).cloned();
        if profile.is_some() {
            shard.promote(session_id);
        }
        profile
    }

    /// Set the sticky profile ID after a successful request.
    /// Uses LRU semantics so hitting the cap evicts the single oldest
    /// session entry instead of wiping every existing stickiness.
    pub fn set(&self, provider_id: &str, session_id: &str, profile_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            let shard = map.entry(provider_id.to_string()).or_default();
            shard.insert(session_id, profile_id, STICKY_MAX_SESSIONS_PER_PROVIDER);
        }
    }
}

pub static PROFILE_STICKY: LazyLock<ProfileStickyMap> = LazyLock::new(ProfileStickyMap::new);

// ── Profile Selection ────────────────────────────────────────────

/// Select the best auth profile for a provider+session combination.
///
/// Priority:
/// 1. Sticky profile from the same session (if still available)
/// 2. First available (non-cooled-down, enabled) profile
/// 3. None (all profiles exhausted)
pub fn select_profile(provider: &ProviderConfig, session_id: &str) -> Option<AuthProfile> {
    let profiles = provider.effective_profiles();
    if profiles.is_empty() {
        return None;
    }

    // Try sticky profile first
    if let Some(sticky_id) = PROFILE_STICKY.get(&provider.id, session_id) {
        if let Some(p) = profiles.iter().find(|p| p.id == sticky_id) {
            if PROFILE_COOLDOWNS.is_available(&p.id) {
                return Some(p.clone());
            }
        }
    }

    // Fall back to first available
    PROFILE_COOLDOWNS
        .filter_available(&profiles)
        .into_iter()
        .next()
}

/// Get the next profile to try after a failure, excluding already-tried profiles.
pub fn next_profile(provider: &ProviderConfig, tried: &[String]) -> Option<AuthProfile> {
    let profiles = provider.effective_profiles();
    PROFILE_COOLDOWNS
        .filter_available(&profiles)
        .into_iter()
        .find(|p| !tried.contains(&p.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit() {
        assert_eq!(
            classify_error("429 Too Many Requests"),
            FailoverReason::RateLimit
        );
        assert_eq!(
            classify_error("Rate limit exceeded"),
            FailoverReason::RateLimit
        );
        assert_eq!(
            classify_error("RESOURCE_EXHAUSTED"),
            FailoverReason::RateLimit
        );
    }

    #[test]
    fn test_overloaded() {
        assert_eq!(
            classify_error("503 Service Unavailable"),
            FailoverReason::Overloaded
        );
        assert_eq!(
            classify_error("The server is overloaded"),
            FailoverReason::Overloaded
        );
        assert_eq!(
            classify_error("502 Bad Gateway"),
            FailoverReason::Overloaded
        );
        assert_eq!(classify_error("server_error"), FailoverReason::Overloaded);
        assert_eq!(
            classify_error("An error occurred while processing your request. Please include the request ID 8d46da73-d9c2-44d5-af24-707fb7680aad in your message."),
            FailoverReason::Overloaded
        );
    }

    #[test]
    fn test_timeout() {
        assert_eq!(classify_error("request timed out"), FailoverReason::Timeout);
        assert_eq!(classify_error("ETIMEDOUT"), FailoverReason::Timeout);
        assert_eq!(
            classify_error("connection reset by peer"),
            FailoverReason::Timeout
        );
        assert_eq!(
            classify_error("error decoding response body"),
            FailoverReason::Timeout
        );
        assert_eq!(
            classify_error("error reading a body from connection"),
            FailoverReason::Timeout
        );
        assert_eq!(
            classify_error("connection closed before message completed"),
            FailoverReason::Timeout
        );
    }

    #[test]
    fn test_auth() {
        assert_eq!(classify_error("401 Unauthorized"), FailoverReason::Auth);
        assert_eq!(classify_error("Invalid API key"), FailoverReason::Auth);
        assert_eq!(classify_error("403 Forbidden"), FailoverReason::Auth);
    }

    #[test]
    fn test_billing() {
        assert_eq!(
            classify_error("402 Payment Required"),
            FailoverReason::Billing
        );
        assert_eq!(
            classify_error("You exceeded your current quota"),
            FailoverReason::Billing
        );
    }

    #[test]
    fn test_model_not_found() {
        assert_eq!(
            classify_error("404 Not Found"),
            FailoverReason::ModelNotFound
        );
        assert_eq!(
            classify_error("model_not_found"),
            FailoverReason::ModelNotFound
        );
        assert_eq!(
            classify_error("The model does not exist"),
            FailoverReason::ModelNotFound
        );
    }

    #[test]
    fn test_context_overflow() {
        assert_eq!(
            classify_error("This model's maximum context length is 200000 tokens"),
            FailoverReason::ContextOverflow
        );
        assert_eq!(
            classify_error("context_length_exceeded"),
            FailoverReason::ContextOverflow
        );
    }

    #[test]
    fn test_unknown() {
        assert_eq!(classify_error("some random error"), FailoverReason::Unknown);
    }

    // ── Structured LlmError classification tests ──────────────────

    #[test]
    fn test_llm_error_to_anyhow_roundtrip() {
        // Full structured error
        let err = LlmError::from_http_status(429, "Too Many Requests")
            .with_code("rate_limit_error")
            .with_retry_after(30);
        let msg = err.to_anyhow().to_string();
        assert!(msg.starts_with("LLM_ERR:429:rate_limit_error:30:Too Many Requests"));
        assert_eq!(classify_error(&msg), FailoverReason::RateLimit);

        // Minimal structured error (status only)
        let err = LlmError::from_http_status(503, "Service Unavailable");
        let msg = err.to_anyhow().to_string();
        assert!(msg.starts_with("LLM_ERR:503:-:-:Service Unavailable"));
        assert_eq!(classify_error(&msg), FailoverReason::Overloaded);

        // No status, only code
        let err = LlmError::new("prompt too long").with_code("context_length_exceeded");
        let msg = err.to_anyhow().to_string();
        assert!(msg.starts_with("LLM_ERR:-:context_length_exceeded:-:prompt too long"));
        assert_eq!(classify_error(&msg), FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_structured_status_classification() {
        let cases = [
            (429, FailoverReason::RateLimit),
            (502, FailoverReason::Overloaded),
            (503, FailoverReason::Overloaded),
            (521, FailoverReason::Overloaded),
            (522, FailoverReason::Overloaded),
            (524, FailoverReason::Overloaded),
            (401, FailoverReason::Auth),
            (403, FailoverReason::Auth),
            (402, FailoverReason::Billing),
            (404, FailoverReason::ModelNotFound),
            (500, FailoverReason::Unknown), // 500 not in status map → Unknown
        ];
        for (status, expected) in cases {
            let err = LlmError::from_http_status(status, "error");
            let msg = err.to_anyhow().to_string();
            assert_eq!(
                classify_error(&msg),
                expected,
                "status {} should classify as {:?}",
                status,
                expected
            );
        }
    }

    #[test]
    fn test_structured_code_classification() {
        let cases = [
            ("context_length_exceeded", FailoverReason::ContextOverflow),
            ("context_overflow", FailoverReason::ContextOverflow),
            ("prompt_too_long", FailoverReason::ContextOverflow),
            ("rate_limit_error", FailoverReason::RateLimit),
            ("resource_exhausted", FailoverReason::RateLimit),
            ("throttle", FailoverReason::RateLimit),
            ("overloaded", FailoverReason::Overloaded),
            ("server_error", FailoverReason::Overloaded),
            ("invalid_api_key", FailoverReason::Auth),
            ("forbidden", FailoverReason::Auth),
            ("billing_not_active", FailoverReason::Billing),
            ("insufficient_quota", FailoverReason::Billing),
            ("model_not_found", FailoverReason::ModelNotFound),
            ("does_not_exist", FailoverReason::ModelNotFound),
        ];
        for (code, expected) in cases {
            let err = LlmError::new("error").with_code(code);
            let msg = err.to_anyhow().to_string();
            assert_eq!(
                classify_error(&msg),
                expected,
                "code {} should classify as {:?}",
                code,
                expected
            );
        }
    }

    #[test]
    fn test_structured_message_fallback() {
        // No status, no code — falls back to substring matching on the message
        let err = LlmError::new("Rate limit exceeded, please slow down");
        let msg = err.to_anyhow().to_string();
        assert_eq!(classify_error(&msg), FailoverReason::RateLimit);

        let err = LlmError::new("something completely unexpected happened");
        let msg = err.to_anyhow().to_string();
        assert_eq!(classify_error(&msg), FailoverReason::Unknown);
    }

    #[test]
    fn test_structured_priority_status_over_code() {
        // Status takes priority over code
        let err = LlmError::from_http_status(429, "error").with_code("context_length_exceeded");
        let msg = err.to_anyhow().to_string();
        assert_eq!(classify_error(&msg), FailoverReason::RateLimit);
    }

    #[test]
    fn test_structured_malformed_fallback() {
        // Malformed LLM_ERR prefix (too few colons) falls back to substring matching
        assert_eq!(
            classify_error("LLM_ERR:429:incomplete"),
            FailoverReason::RateLimit // substring "429" matched
        );
    }

    #[test]
    fn test_legacy_errors_still_work() {
        // Non-structured errors go through substring matching
        assert_eq!(
            classify_error("429 Too Many Requests"),
            FailoverReason::RateLimit
        );
        assert_eq!(
            classify_error("503 Service Unavailable"),
            FailoverReason::Overloaded
        );
        assert_eq!(
            classify_error("context_length_exceeded"),
            FailoverReason::ContextOverflow
        );
        assert_eq!(classify_error("some random error"), FailoverReason::Unknown);
    }

    // ── classify_error_full + retry_after tests ───────────────────

    #[test]
    fn test_classify_error_full_structured() {
        let err = LlmError::from_http_status(429, "Too Many Requests").with_retry_after(30);
        let classified = classify_error_full(&err.to_anyhow().to_string());
        assert_eq!(classified.reason, FailoverReason::RateLimit);
        assert_eq!(classified.retry_after_secs, Some(30));
    }

    #[test]
    fn test_classify_error_full_no_retry_after() {
        let err = LlmError::from_http_status(503, "Service Unavailable");
        let classified = classify_error_full(&err.to_anyhow().to_string());
        assert_eq!(classified.reason, FailoverReason::Overloaded);
        assert_eq!(classified.retry_after_secs, None);
    }

    #[test]
    fn test_classify_error_full_legacy() {
        let classified = classify_error_full("429 Too Many Requests");
        assert_eq!(classified.reason, FailoverReason::RateLimit);
        assert_eq!(classified.retry_after_secs, None);
    }

    #[test]
    fn test_parse_retry_after() {
        assert_eq!(LlmError::parse_retry_after("30"), Some(30));
        assert_eq!(LlmError::parse_retry_after("  60  "), Some(60));
        assert_eq!(LlmError::parse_retry_after("0"), Some(0));
        // HTTP-date format — not supported, returns None
        assert_eq!(
            LlmError::parse_retry_after("Fri, 28 Jun 2026 12:00:00 GMT"),
            None
        );
        assert_eq!(LlmError::parse_retry_after(""), None);
        assert_eq!(LlmError::parse_retry_after("invalid"), None);
    }

    #[test]
    fn test_retryable() {
        assert!(FailoverReason::RateLimit.is_retryable());
        assert!(FailoverReason::Overloaded.is_retryable());
        assert!(FailoverReason::Timeout.is_retryable());
        assert!(FailoverReason::Unknown.is_retryable());
        assert!(!FailoverReason::Auth.is_retryable());
        assert!(!FailoverReason::ContextOverflow.is_retryable());
    }

    #[test]
    fn test_max_retries() {
        // Unknown errors capped at 1 retry regardless of policy
        assert_eq!(FailoverReason::Unknown.max_retries(2), 1);
        assert_eq!(FailoverReason::Unknown.max_retries(0), 0);
        // Other retryable errors use the full policy budget
        assert_eq!(FailoverReason::RateLimit.max_retries(2), 2);
        assert_eq!(FailoverReason::Overloaded.max_retries(3), 3);
        assert_eq!(FailoverReason::Timeout.max_retries(5), 5);
        // Non-retryable errors — max_retries is irrelevant (is_retryable=false)
        // but the method still returns the policy value
        assert_eq!(FailoverReason::Auth.max_retries(2), 2);
    }

    #[test]
    fn test_terminal() {
        // ContextOverflow is no longer terminal — it triggers compaction first.
        assert!(!FailoverReason::ContextOverflow.is_terminal());
        assert!(!FailoverReason::RateLimit.is_terminal());
        assert!(!FailoverReason::Unknown.is_terminal());
    }

    #[test]
    fn test_retry_delay() {
        let d0 = retry_delay_ms(0, 1000, 10000);
        assert!(d0 >= 900 && d0 <= 1100); // ~1000 ±10%

        let d1 = retry_delay_ms(1, 1000, 10000);
        assert!(d1 >= 1800 && d1 <= 2200); // ~2000 ±10%

        let d_max = retry_delay_ms(10, 1000, 10000);
        assert!(d_max >= 9000 && d_max <= 11000); // clamped to ~10000
    }

    // ── Profile rotation tests ──────────────────────────────────

    #[test]
    fn test_is_profile_rotatable() {
        assert!(FailoverReason::RateLimit.is_profile_rotatable());
        assert!(FailoverReason::Overloaded.is_profile_rotatable());
        assert!(FailoverReason::Auth.is_profile_rotatable());
        assert!(FailoverReason::Billing.is_profile_rotatable());
        assert!(!FailoverReason::Timeout.is_profile_rotatable());
        assert!(!FailoverReason::ModelNotFound.is_profile_rotatable());
        assert!(!FailoverReason::ContextOverflow.is_profile_rotatable());
        assert!(!FailoverReason::Unknown.is_profile_rotatable());
    }

    #[test]
    fn test_profile_cooldown_secs() {
        assert_eq!(FailoverReason::Overloaded.profile_cooldown_secs(), 30);
        assert_eq!(FailoverReason::RateLimit.profile_cooldown_secs(), 60);
        assert_eq!(FailoverReason::Auth.profile_cooldown_secs(), 300);
        assert_eq!(FailoverReason::Billing.profile_cooldown_secs(), 600);
        assert_eq!(FailoverReason::Timeout.profile_cooldown_secs(), 0);
    }

    #[test]
    fn test_cooldown_tracker_basic() {
        let tracker = ProfileCooldownTracker::new();
        assert!(tracker.is_available("p1"));

        tracker.mark_cooldown("p1", &FailoverReason::RateLimit);
        assert!(!tracker.is_available("p1"));
        assert!(tracker.is_available("p2"));

        tracker.clear("p1");
        assert!(tracker.is_available("p1"));
    }

    #[test]
    fn test_cooldown_zero_duration_not_tracked() {
        let tracker = ProfileCooldownTracker::new();
        tracker.mark_cooldown("p1", &FailoverReason::Timeout); // 0 secs
        assert!(tracker.is_available("p1"));
    }

    #[test]
    fn test_sticky_map_basic() {
        let sticky = ProfileStickyMap::new();
        assert!(sticky.get("prov1", "sess1").is_none());

        sticky.set("prov1", "sess1", "profile-a");
        assert_eq!(sticky.get("prov1", "sess1").as_deref(), Some("profile-a"));
        assert!(sticky.get("prov1", "sess2").is_none());
    }

    #[test]
    fn test_sticky_map_lru_eviction_preserves_recent() {
        // Hit the cap + 1 with distinct sessions; oldest is evicted but
        // newer ones must survive (old `clear()` wiped everything).
        let sticky = ProfileStickyMap::new();
        for i in 0..STICKY_MAX_SESSIONS_PER_PROVIDER {
            sticky.set("prov1", &format!("sess{}", i), "profile-a");
        }
        // One past the cap triggers eviction of sess0.
        sticky.set(
            "prov1",
            &format!("sess{}", STICKY_MAX_SESSIONS_PER_PROVIDER),
            "profile-a",
        );
        assert!(
            sticky.get("prov1", "sess0").is_none(),
            "oldest entry should have been evicted"
        );
        // Recently used entries must still be present.
        assert_eq!(
            sticky.get("prov1", "sess1").as_deref(),
            Some("profile-a"),
            "recent entries must not be wiped by cap enforcement"
        );
        assert_eq!(
            sticky
                .get(
                    "prov1",
                    &format!("sess{}", STICKY_MAX_SESSIONS_PER_PROVIDER)
                )
                .as_deref(),
            Some("profile-a"),
            "newest entry must be present"
        );
    }

    #[test]
    fn test_sticky_map_lru_promotes_on_get() {
        let sticky = ProfileStickyMap::new();
        // Fill up to the cap so the next insert triggers exactly one
        // eviction. Seed the two oldest entries first so we can observe
        // the promotion effect before fillers arrive.
        sticky.set("prov1", "sess-a", "profile-a");
        sticky.set("prov1", "sess-b", "profile-b");
        for i in 0..(STICKY_MAX_SESSIONS_PER_PROVIDER - 2) {
            sticky.set("prov1", &format!("filler{}", i), "profile-a");
        }
        // Promote sess-a so sess-b is now the oldest.
        assert_eq!(sticky.get("prov1", "sess-a").as_deref(), Some("profile-a"));
        // Next insert overflows the cap by one → pop_front evicts sess-b.
        sticky.set("prov1", "trigger", "profile-a");
        assert_eq!(
            sticky.get("prov1", "sess-a").as_deref(),
            Some("profile-a"),
            "promoted entry must survive eviction"
        );
        assert!(
            sticky.get("prov1", "sess-b").is_none(),
            "untouched older entry should have been evicted"
        );
    }

    #[test]
    fn test_select_profile_basic() {
        use crate::provider::{ApiType, AuthProfile, ProviderConfig};
        let mut cfg = ProviderConfig::new(
            "test".into(),
            ApiType::Anthropic,
            "https://api.anthropic.com".into(),
            String::new(),
        );
        cfg.auth_profiles = vec![
            AuthProfile::new("A".into(), "key-a".into(), None),
            AuthProfile::new("B".into(), "key-b".into(), None),
        ];

        let selected = select_profile(&cfg, "sess1");
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().api_key, "key-a");
    }

    #[test]
    fn test_next_profile_excludes_tried() {
        use crate::provider::{ApiType, AuthProfile, ProviderConfig};
        let mut cfg = ProviderConfig::new(
            "test".into(),
            ApiType::Anthropic,
            "https://api.anthropic.com".into(),
            String::new(),
        );
        let p1 = AuthProfile::new("A".into(), "key-a".into(), None);
        let p1_id = p1.id.clone();
        let p2 = AuthProfile::new("B".into(), "key-b".into(), None);
        cfg.auth_profiles = vec![p1, p2];

        let next = next_profile(&cfg, &[p1_id]);
        assert!(next.is_some());
        assert_eq!(next.unwrap().api_key, "key-b");
    }

    #[test]
    fn test_next_profile_all_tried() {
        use crate::provider::{ApiType, AuthProfile, ProviderConfig};
        let mut cfg = ProviderConfig::new(
            "test".into(),
            ApiType::Anthropic,
            "https://api.anthropic.com".into(),
            String::new(),
        );
        let p1 = AuthProfile::new("A".into(), "key-a".into(), None);
        let p1_id = p1.id.clone();
        let p2 = AuthProfile::new("B".into(), "key-b".into(), None);
        let p2_id = p2.id.clone();
        cfg.auth_profiles = vec![p1, p2];

        let next = next_profile(&cfg, &[p1_id, p2_id]);
        assert!(next.is_none());
    }
}
