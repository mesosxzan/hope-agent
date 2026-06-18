// ── Tier 2: Context Pruning ──

use super::boundary::RecentBoundary;
use super::config::CompactConfig;
use super::estimation::{
    build_tool_id_to_name_map, estimate_message_chars, get_tool_name_for_result,
    get_tool_result_text, is_tool_result, is_user_message, set_tool_result_text,
};
#[cfg(test)]
use super::recent_boundary;
use super::truncation::head_tail_truncate;
use super::types::{PruneResult, ToolResultInfo};
use super::CHARS_PER_TOKEN;
use serde_json::Value;
use std::collections::HashMap;

/// Compute prune priority for a tool result (higher = prune first).
/// Improvement over openclaw: uses age x size instead of pure age.
fn prune_priority(msg_index: usize, total_messages: usize, content_chars: usize) -> f64 {
    let age = 1.0 - (msg_index as f64 / total_messages.max(1) as f64);
    let size = (content_chars as f64 / 100_000.0).min(1.0);
    age * 0.6 + size * 0.4
}

/// Find the first user message index (protects bootstrap context).
fn find_first_user_index(messages: &[Value]) -> Option<usize> {
    messages.iter().position(|m| is_user_message(m))
}

/// Collect info about tool results in the prunable range.
fn collect_prunable_tool_results(
    messages: &[Value],
    prune_start: usize,
    cutoff: usize,
    tool_id_to_name: &HashMap<String, String>,
    config: &CompactConfig,
) -> Vec<ToolResultInfo> {
    let mut results = Vec::new();
    for i in prune_start..cutoff {
        let msg = &messages[i];
        if !is_tool_result(msg) {
            continue;
        }
        let tool_name = get_tool_name_for_result(msg, tool_id_to_name);
        if let Some(ref name) = tool_name {
            if config.is_protected(name) {
                continue;
            }
        }
        let content_chars = get_tool_result_text(msg).map(|t| t.len()).unwrap_or(0);
        results.push(ToolResultInfo {
            msg_index: i,
            tool_name,
            content_chars,
        });
    }
    results
}

/// Tier 2: Prune old context based on usage ratio.
#[cfg(test)]
fn prune_old_context(
    messages: &mut [Value],
    system_prompt: &str,
    context_window: u32,
    max_output_tokens: u32,
    config: &CompactConfig,
) -> PruneResult {
    let boundary = recent_boundary(messages, config.preserve_recent_rounds);
    prune_old_context_with_boundary(
        messages,
        system_prompt,
        context_window,
        max_output_tokens,
        config,
        &boundary,
    )
}

pub(super) fn prune_old_context_with_boundary(
    messages: &mut [Value],
    system_prompt: &str,
    context_window: u32,
    max_output_tokens: u32,
    config: &CompactConfig,
    boundary: &RecentBoundary,
) -> PruneResult {
    let mut result = PruneResult {
        soft_trimmed: 0,
        hard_cleared: 0,
        chars_freed: 0,
    };

    let char_window = context_window as usize * CHARS_PER_TOKEN;
    if char_window == 0 {
        return result;
    }

    // Step 1: Find protected boundary
    let cutoff = boundary.protected_start_index;

    // Step 2: Find first user message (protect bootstrap)
    let prune_start = find_first_user_index(messages).unwrap_or(messages.len());
    if prune_start >= cutoff {
        return result; // No prunable range
    }

    // Step 3: Calculate current ratio
    let total_chars = system_prompt.len()
        + messages
            .iter()
            .map(|m| estimate_message_chars(m))
            .sum::<usize>()
        + (max_output_tokens as usize * CHARS_PER_TOKEN);
    let ratio = total_chars as f64 / char_window as f64;

    if ratio <= config.soft_trim_ratio {
        return result; // Below threshold
    }

    // Step 4: Collect prunable tool results, sorted by priority (highest first)
    let tool_id_to_name = build_tool_id_to_name_map(messages);
    let mut prunable =
        collect_prunable_tool_results(messages, prune_start, cutoff, &tool_id_to_name, config);
    let total_msgs = messages.len();
    prunable.sort_by(|a, b| {
        let pa = prune_priority(a.msg_index, total_msgs, a.content_chars);
        let pb = prune_priority(b.msg_index, total_msgs, b.content_chars);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Step 5: Soft trim phase
    let mut current_chars = total_chars;
    for info in &prunable {
        if info.content_chars <= config.soft_trim_max_chars {
            continue; // Too small to trim
        }
        let target_size = config.soft_trim_head_chars + config.soft_trim_tail_chars + 200; // 200 for markers
        if let Some(text) = get_tool_result_text(&messages[info.msg_index]) {
            let original_len = text.len();
            if original_len <= target_size {
                continue;
            }
            let trimmed = head_tail_truncate(&text, target_size);
            let freed = original_len - trimmed.len();
            set_tool_result_text(&mut messages[info.msg_index], &trimmed);
            current_chars = current_chars.saturating_sub(freed);
            result.soft_trimmed += 1;
            result.chars_freed += freed;

            // Re-check ratio
            let new_ratio = current_chars as f64 / char_window as f64;
            if new_ratio <= config.hard_clear_ratio {
                return result;
            }
        }
    }

    // Step 6: Hard clear phase
    if !config.hard_clear_enabled {
        return result;
    }

    let total_prunable_chars: usize = prunable
        .iter()
        .map(|i| {
            get_tool_result_text(&messages[i.msg_index])
                .map(|t| t.len())
                .unwrap_or(0)
        })
        .sum();

    if total_prunable_chars < config.min_prunable_tool_chars {
        return result; // Not enough benefit
    }

    for info in &prunable {
        let current_ratio = current_chars as f64 / char_window as f64;
        if current_ratio <= config.hard_clear_ratio {
            break;
        }
        if let Some(text) = get_tool_result_text(&messages[info.msg_index]) {
            let original_len = text.len();
            if original_len <= config.hard_clear_placeholder.len() {
                continue; // Already cleared or too small
            }
            set_tool_result_text(
                &mut messages[info.msg_index],
                &config.hard_clear_placeholder,
            );
            let freed = original_len - config.hard_clear_placeholder.len();
            current_chars = current_chars.saturating_sub(freed);
            result.hard_cleared += 1;
            result.chars_freed += freed;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pruning_config() -> CompactConfig {
        CompactConfig {
            soft_trim_ratio: 0.10,
            hard_clear_ratio: 0.20,
            preserve_recent_rounds: 1,
            hard_clear_enabled: false,
            ..CompactConfig::default()
        }
    }

    fn prune(messages: &mut [Value]) -> PruneResult {
        prune_old_context(messages, "", 1_000, 0, &pruning_config())
    }

    #[test]
    fn protected_anthropic_tool_result_is_not_pruned_via_tool_use_id() {
        let protected = "protected-memory-result ".repeat(2_000);
        let mut messages = vec![
            json!({ "role": "user", "content": "old request" }),
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "recall_memory",
                    "input": { "query": "project facts" }
                }]
            }),
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": protected
                }]
            }),
            json!({ "role": "user", "content": "later request" }),
            json!({ "role": "assistant", "content": "recent assistant reply" }),
        ];
        let original = get_tool_result_text(&messages[2]).unwrap();

        let result = prune(&mut messages);

        assert_eq!(result.soft_trimmed, 0);
        assert_eq!(result.hard_cleared, 0);
        assert_eq!(get_tool_result_text(&messages[2]).unwrap(), original);
    }

    #[test]
    fn protected_openai_chat_tool_result_is_not_pruned_via_tool_call_id() {
        let protected = "protected-web-result ".repeat(2_000);
        let mut messages = vec![
            json!({ "role": "user", "content": "old request" }),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "web_fetch",
                        "arguments": "{\"url\":\"https://example.com\"}"
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": protected
            }),
            json!({ "role": "user", "content": "later request" }),
            json!({ "role": "assistant", "content": "recent assistant reply" }),
        ];
        let original = get_tool_result_text(&messages[2]).unwrap();

        let result = prune(&mut messages);

        assert_eq!(result.soft_trimmed, 0);
        assert_eq!(result.hard_cleared, 0);
        assert_eq!(get_tool_result_text(&messages[2]).unwrap(), original);
    }

    #[test]
    fn protected_responses_tool_result_is_not_pruned_via_call_id() {
        let protected = "protected-memory-result ".repeat(2_000);
        let mut messages = vec![
            json!({ "role": "user", "content": "old request" }),
            json!({
                "type": "function_call",
                "call_id": "fc_1",
                "name": "memory_get",
                "arguments": "{\"id\":\"mem_1\"}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "fc_1",
                "output": protected
            }),
            json!({ "role": "user", "content": "later request" }),
            json!({ "role": "assistant", "content": "recent assistant reply" }),
        ];
        let original = get_tool_result_text(&messages[2]).unwrap();

        let result = prune(&mut messages);

        assert_eq!(result.soft_trimmed, 0);
        assert_eq!(result.hard_cleared, 0);
        assert_eq!(get_tool_result_text(&messages[2]).unwrap(), original);
    }

    #[test]
    fn ordinary_openai_chat_tool_result_still_prunes() {
        let ordinary = "ordinary-large-result ".repeat(2_000);
        let mut messages = vec![
            json!({ "role": "user", "content": "old request" }),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"/tmp/a.rs\"}"
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": ordinary
            }),
            json!({ "role": "user", "content": "later request" }),
            json!({ "role": "assistant", "content": "recent assistant reply" }),
        ];
        let original_len = get_tool_result_text(&messages[2]).unwrap().len();

        let result = prune(&mut messages);

        assert_eq!(result.soft_trimmed, 1);
        assert_eq!(result.hard_cleared, 0);
        assert!(get_tool_result_text(&messages[2]).unwrap().len() < original_len);
    }
}
