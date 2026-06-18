// ── Compaction Manifest ─────────────────────────────────────────
//
// Structured observability payload for context compaction. It is attached to
// CompactResult so automatic and manual compaction callers can log/emit the
// same facts without changing compaction behavior.

use serde::Serialize;

use super::boundary::RecentBoundary;
use super::types::CompactDetails;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionManifest {
    pub compaction_id: String,
    pub tier: u8,
    pub trigger: String,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub protected_start_index: Option<usize>,
    pub summarized_range: Option<(usize, usize)>,
    pub rounds_summarized: usize,
    pub tool_results_truncated: usize,
    pub tool_results_soft_trimmed: usize,
    pub tool_results_hard_cleared: usize,
    pub files_recovered: usize,
    pub cache_ttl_throttled: bool,
    pub warnings: Vec<String>,
}

fn new_compaction_id() -> String {
    let ns = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_micros() * 1000);
    format!("cc-{}", ns)
}

impl CompactionManifest {
    pub fn for_result_with_boundary(
        tier: u8,
        trigger: impl Into<String>,
        tokens_before: u32,
        tokens_after: u32,
        details: Option<&CompactDetails>,
        boundary: &RecentBoundary,
    ) -> Self {
        let details = details.cloned();
        Self {
            compaction_id: new_compaction_id(),
            tier,
            trigger: trigger.into(),
            tokens_before,
            tokens_after,
            protected_start_index: Some(boundary.protected_start_index),
            summarized_range: None,
            rounds_summarized: 0,
            tool_results_truncated: details
                .as_ref()
                .map(|d| d.tool_results_truncated)
                .unwrap_or(0),
            tool_results_soft_trimmed: details
                .as_ref()
                .map(|d| d.tool_results_soft_trimmed)
                .unwrap_or(0),
            tool_results_hard_cleared: details
                .as_ref()
                .map(|d| d.tool_results_hard_cleared)
                .unwrap_or(0),
            files_recovered: 0,
            cache_ttl_throttled: false,
            warnings: boundary.warnings.clone(),
        }
    }

    pub fn with_cache_ttl_throttled(mut self, throttled: bool) -> Self {
        self.cache_ttl_throttled = throttled;
        self
    }
}
