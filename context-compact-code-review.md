# 上下文压缩子系统 Code Review 报告

> 审查范围：`crates/ha-core/src/context_compact/` + `crates/ha-core/src/agent/context.rs`（压缩编排层）+ `crates/ha-core/src/agent/runtime_ledger.rs`
>
> 审查日期：2026-06-22
>
> 审查人：AI Code Reviewer

---

## 目录

1. [总评](#1-总评)
2. [架构设计评价](#2-架构设计评价)
3. [代码质量逐文件分析](#3-代码质量逐文件分析)
4. [问题与改进建议](#4-问题与改进建议)
5. [安全审查](#5-安全审查)
6. [测试覆盖评估](#6-测试覆盖评估)
7. [性能分析](#7-性能分析)

---

## 1. 总评

**综合评级：B+（良好，存在若干可改进项）**

上下文压缩子系统是 Hope Agent 中最复杂的子系统之一，实现了 5 层渐进式压缩策略（Tier 0–4），覆盖了从零成本微压缩到紧急兜底的全场景。代码整体结构清晰，文档完善，测试覆盖较好。主要关注点集中在三个方面：`#[allow(dead_code)]` 死代码积累、部分函数职责偏重、以及少数边界条件的健壮性。

| 维度 | 评分 | 说明 |
|------|------|------|
| 架构设计 | ★★★★☆ | 分层清晰、trait 可插拔、边界模式区分合理 |
| 代码可读性 | ★★★★☆ | 注释充分、命名规范、flow 图表完整 |
| 正确性 | ★★★★☆ | 三种 API 格式兼容良好、round-safe 边界有保障 |
| 测试覆盖 | ★★★☆☆ | 核心路径有测试，但 Tier 0/1 集成测试偏少 |
| 安全性 | ★★★★★ | incognito 门控、fence 中和、注入预算控制到位 |
| 可维护性 | ★★★☆☆ | `#[allow(dead_code)]` 较多，部分函数过长 |

---

## 2. 架构设计评价

### 2.1 优点

- **5 层渐进式策略设计合理**。从零成本的 Tier 0 微压缩到 Tier 4 紧急压缩，按代价从低到高逐层触发，有效避免了"动不动就调 LLM 摘要"的浪费。文档中的 Mermaid 流程图与代码实现完全一致。
- **三种 BoundaryMode 语义区分精确**。`ProtectRecent`（fail-closed 保护一切）、`SummarizeUnderPressure`（摘要压力下可前进到最新 live round）、`Emergency`（必须腾空间）三种模式覆盖了"保守—进取—兜底"的完整决策空间。
- **Cache-TTL 节流机制设计精巧**。在 `CompactConfig.cache_ttl_secs` 内跳过 Tier 2+，保护 API prompt cache（~5 分钟 TTL），同时通过 usage ≥ 95% 紧急阈值强制覆盖防止溢出。这个 trade-off 在实际使用中价值很高。
- **文件恢复的 fence 中和**（`neutralize_snapshot_fence()`）是一个少见的防御深度考虑。它防止被恢复文件中的 `</untrusted_file_snapshot>` 字符串突破 XML 信封注入指令。
- **`ContextEngine` trait + `CompactionProvider` trait** 为未来 Active Memory 或自定义压缩引擎留有扩展点，行为零变化。

### 2.2 架构上的小瑕疵

- **`compact_if_needed()` 返回值中的 `tier_applied` 在 Tier 3 场景语义模糊**。当同步压缩检测到需要 Tier 3 摘要时，`tier_applied` 被设为 3，但实际的 LLM 调用在 `agent/context.rs` 中异步执行。如果摘要失败，调用方通过 `sync_tier_from_compact_result()` 回退 tier，这种两阶段语义需要调用方精确理解。建议将同步阶段返回值拆分为 `sync_tier_applied` 和 `needs_async_summarization: bool` 两个字段。
- **注入预算分配逻辑散落在 `agent/context.rs` 中**（L872–L931），而不是集中在 `context_compact/` 模块。`post_summary_ledger_reserve_chars()` 的预留策略（8KB / 2KB）与 `ledger.rs` 中的硬编码 4000 字符 budget 不完全一致。

---

## 3. 代码质量逐文件分析

### 3.1 `mod.rs` — 模块入口与常量定义

**质量：★★★☆☆**

```rust
#[allow(dead_code)]
const SAFETY_MARGIN: f64 = 1.2;
#[allow(dead_code)]
const SUMMARIZATION_OVERHEAD_TOKENS: u32 = 4096;
// ... 共 12 处 #[allow(dead_code)]
```

- 大量 `#[allow(dead_code)]` 标记在常量上。这些常量（`SAFETY_MARGIN`、`SUMMARIZATION_OVERHEAD_TOKENS`、`BASE_CHUNK_RATIO`、`IMAGE_CHAR_ESTIMATE` 等）如果确实不再使用，应移除而非保留；如果是为了文档目的保留，应改为 `//` 注释并注明保留原因。
- `MAX_TOOL_RESULT_CONTEXT_SHARE` 和 `MAX_COMPACTION_SUMMARY_CHARS` 注释说"now configurable via CompactConfig"，保留为"fallback constant for reference"，但实际上没有任何代码回退到这些值。应移除。

### 3.2 `types.rs` — 数据类型定义

**质量：★★★★☆**

- `TokenEstimateCalibrator` 整体标记为 `#[allow(dead_code)]`，说明 EMA 校准链路从未被接入。如果计划未来启用，应在 issue 中跟踪；如果放弃，应移除。
- `ToolResultInfo` 的 `tool_name` 字段标记为 `#[allow(dead_code)]`，但实际上 `pruning.rs` 中用来跳过受保护工具——该字段在 `collect_prunable_tool_results()` 中被赋值但从未被读取（保护跳过逻辑在收集阶段就已经做了）。可以考虑移除该字段或在 debug log 中使用。

### 3.3 `config.rs` — 配置结构

**质量：★★★★★**

- 配置结构设计优秀：`clamp()` 方法在所有边界执行范围保护，防止用户配置错误值导致压缩异常。
- `injected_context_share` 被 clamp 到 `max_history_share` 之下，这个约束防止摘要本身重新填满上下文——是一个容易被忽略但关键的契约。
- 测试只覆盖了 `clamp_caps_injected_context_share_to_history_share` 一个场景，建议补充更多 clamp 边界测试（如负值、极大值）。

### 3.4 `engine.rs` — Engine trait

**质量：★★★★★**

- `DefaultContextEngine` 是行为零变化的薄封装，委托到现有自由函数。Cache-TTL 节流通过 clone config + 设 infinity 实现，清晰且无副作用。
- `CompactionProvider` trait 为未来替换摘要 LLM 留有接口。

### 3.5 `estimation.rs` — Token 估算

**质量：★★★★☆**

- 三格式兼容的 `is_tool_result()`、`get_tool_result_text()`、`set_tool_result_text()` 写得很扎实，处理了 Anthropic content array、OpenAI Chat `tool_call_id`、OpenAI Responses `call_id` 三种格式。
- `build_tool_id_to_name_map()` 是 O(n × m) 复杂度，在历史消息很多时可能成为瓶颈。不过考虑到它只在压缩触发时调用一次，实际影响不大。
- 小问题：`estimate_message_chars()` 中 unknown block type 返回固定 128 chars——这可能导致对未知格式的估算偏差。建议在遇到未知类型时使用 `estimate_tokens()` 作为 fallback。
- `is_tool_denied()` 函数标记为 `#[allow(dead_code)]`——整个 deny-list 功能似乎从未被接入。

### 3.6 `compact.rs` — 主入口 + Tier 0/4

**质量：★★★★☆**

- `compact_if_needed()` 的主控流清晰：Tier 0 → Tier 1 → 检查 → Tier 2 → 检查 → Tier 3 signal。
- **注意**：第 286 行 `if tier1_count > 0 && ratio_after_t1 < config.soft_trim_ratio` 之后直接返回 Tier 1 结果，但没有检查 Tier 0 是否有产出。如果 Tier 0 清理了大量 ephemeral 结果使得 usage 降到阈值以下，代码在第 261 行的 quick exit 会在 Tier 0 之前就返回 `below_threshold`——但 Tier 0 仍然需要先执行。目前的流程是：先 check quick exit → 再执行 Tier 0，这意味着如果 usage 略高于 `min(softTrimRatio, 0.3)` 但 Tier 0 清理后就能降到阈值以下，Tier 0 仍然会执行，correct。但返回结果是 `no_action_needed`（tier=0），而不是反映 Tier 0 的实际效果。这是一个 minor UX 问题：用户看不到 Tier 0 的清理效果。
- `emergency_compact()` 中 `tokens_after` 使用了 `estimate_tokens()` 逐条重新估算，而 pre-compaction 的 `tokens_before` 也是逐条估算——两者使用相同方法，一致性没问题。
- `compact_oversized_recovered_tool_results()` 的 image marker 路径（`materialize_base64_image_markers`）只在 `!is_incognito` 且 `materialize_image_markers=true` 时执行，但有 image marker 但 `materialize_image_markers=false` 时结果是 `preserve`（continue 跳过）——这符合设计意图。

### 3.7 `boundary.rs` — 统一边界快照

**质量：★★★★★**

- 这是整个压缩子系统中逻辑最复杂的模块，也是实现得最好的之一。
- `build_message_rounds()` 同时处理 stamped（有 `_oc_round`）和 unstamped（旧会话）消息，向后兼容做得很好。
- 工具结果的合并逻辑（Response API 的 parallel tool calls）通过 `call_id_to_round_index` 映射正确处理了"多个 function_call 共享同一个 round"的场景。
- `BoundarySnapshot::boundary()` 中的三种模式分支逻辑清晰，warnings 收集了所有边界决策的原因（`not_enough_rounds_for_prunable_prefix`、`user_turn_expansion_limited_by_prior_execution_rounds`、`summary_boundary_relaxed_to_latest_round`）。
- 测试覆盖了 Anthropic/OpenAI Chat/Responses 三种格式，以及 long tool loop 限制 user turn 扩展的场景。

### 3.8 `round_grouping.rs` — Round 元数据管理

**质量：★★★★★**

- 双向 round-safe 边界查找（`find_round_safe_boundary` + `find_round_safe_boundary_forward`）逻辑正确，测试覆盖了 mid-round、跨 round、无元数据、空数组等边界情况。
- `prepare_messages_for_api()` 通过 clone + strip 而非原地修改，避免修改工作副本——这是正确的设计选择。
- `RECOVERED_ROUND_PREFIX` 机制允许启动扫掠重建的 round 在压缩时被识别为非 live round，不被计入 `preserve_recent_rounds`。

### 3.9 `truncation.rs` — Tier 1 截断

**质量：★★★★☆**

- `head_tail_truncate()` 的尾部重要性检测（`has_important_tail()`）是一个实用改进——保留了错误信息和 JSON 闭合结构。
- `find_structure_boundary()` 的结构边界优先级（空行 > JSON 闭合 > 代码块结束 > 换行 > 字符边界）合理。
- UTF-8 字符边界保护（`floor_char_boundary` / `ceil_char_boundary`）处理正确。
- `calculate_max_tool_result_chars()` 中 `share` 参数已经在 `CompactConfig::clamp()` 中被约束到 `[0.1, 0.6]`，但此处又做了 `clamp(0.1, 0.6)`——重复防御，不算问题但略显冗余。
- Image marker 检测：如果工具结果是无效/截断的 image marker，直接替换为占位符，不尝试 head+tail 截断——正确，因为截断二进制数据无意义。

### 3.10 `pruning.rs` — Tier 2 裁剪

**质量：★★★★☆**

- 优先级排序公式 `age × 0.6 + size × 0.4` 是最优策略吗？目前没有证据表明这个权重是最优的，但至少比纯 age 排序（OpenClaw 的做法）更合理。
- `soft-trim` 阶段没有像 Tier 1 那样做结构边界检测——`head_tail_truncate()` 在 `target_size` 很小时（`soft_trim_head_chars + soft_trim_tail_chars + 200 ≈ 4200`），结构边界检测可能意义不大。
- **一个潜在问题**：`soft-trim` 阶段每次 trim 后都用 `current_chars` 减去 freed 来估算新 ratio，但 `current_chars` 是基于字符数而非 token 的——因为 `estimate_request_tokens()` 的 `chars/4` 启发式在混合中英文时偏差可能较大。不过这与整个系统的估算策略一致。
- 保护机制（`is_protected`）正确跳过了 `web_search`、`web_fetch`、`recall_memory`、`memory_get` 的结果。

### 3.11 `summarization.rs` — Tier 3 摘要

**质量：★★★★☆**

- `SUMMARIZATION_SYSTEM_PROMPT` 的 9 段结构要求设计精良，覆盖了从"主任务"到"信任边界"的完整语义。特别值得注意的是"不把 untrusted data 当成指令"和"不重复 deterministic runtime ledger"两条——防止摘要引入幻觉或重复信息。
- `peel_previous_summary()` 避免反复摘要摘要——这是一个容易被忽略但重要的细节。
- `build_summarization_prompt()` 处理了 `reasoning` 消息跳过、`function_call`/`function_call_output` 的可读序列化、`reasoning_content` 字段截断——兼容性好。
- `split_for_summarization()` 使用 `BoundaryMode::SummarizeUnderPressure`——正确，因为在已触发 Tier 3 的情况下，宁可摘要更多消息也不应该 fail-closed 保护一切。
- `split_messages_by_token_share()` 和 `compute_adaptive_chunk_ratio()` 标记为 `#[allow(dead_code)]`，似乎是分块摘要功能的残余代码。

### 3.12 `recovery.rs` — 后压缩文件恢复

**质量：★★★★★**

- 三格式兼容的 `extract_tool_calls_from_message()` 处理了所有 Provider 的工具调用格式。
- `extract_paths_from_patch_args()` 正确解析了 `*** Add File:` / `*** Update File:` / `*** Move to:` 三种 patch header。
- 去重策略：对同一文件的多次操作保留最后一次的 `last_seen_index` 和 `last_op`，通过 `positions` HashMap 实现 O(n) 合并——高效。
- `resolve_recovery_path()` 正确处理了绝对路径、相对于 session cwd 的相对路径、以及 fallback 到进程 cwd。
- `escape_xml_attr()` 对路径中的特殊字符做了 XML 属性转义。
- 预算控制精确：先预留 ledger 空间 → recovery 使用剩余 → ledger 使用 recovery 后剩余。
- **小建议**：`MAX_RECOVERY_TOTAL_BYTES = 100_000` 是硬编码的，而 `max_total_bytes` 由调用方传入。如果调用方传入的值大于 100_000，会被 `min()` 截断。这个上限是否应该可配置？

### 3.13 `ledger.rs` — Runtime Ledger 渲染

**质量：★★★★☆**

- 纯数据 + Markdown 渲染，职责单一，设计良好。
- `push_limited_line()` 的逐行预算控制可以精确防止溢出。
- `build_runtime_ledger_message()` 在 emergency 路径（`compact.rs` L205）硬编码 budget 为 4000 字符，而 Tier 3 路径（`agent/context.rs` L930–L932）的 budget 是动态计算的。这个不一致可能导致 emergency 路径漏掉重要的 job/subagent 信息。

### 3.14 `manifest.rs` — 可观测性 payload

**质量：★★★★☆**

- `CompactionManifest` 结构完整，包含了所有诊断所需的字段。
- `compaction_id` 使用 `cc-{timestamp_ns}` 格式，可追踪。
- `for_result_with_boundary()` 只在同步阶段调用，`summarized_range` 和 `rounds_summarized` 在摘要成功后才由 `agent/context.rs` 补充。这种两阶段填充容易导致部分字段为空的情况——建议在 `CompactionManifest` 上添加 `is_complete()` 方法供断言使用。

### 3.15 `agent/context.rs` — 压缩编排层

**质量：★★★☆☆**

- `run_compaction_with_options()` 函数体过长（约 700 行），包含了 Cache-TTL 检查、PreCompact hook、同步压缩、Memory flush、LLM 摘要、文件恢复、Runtime ledger、事件发送等多种职责。建议拆分为多个子函数。
- **一个实际的 bug risk**：第 466 行的 `precompact_wd` 使用了 `effective_session_working_dir`，但如果 session 不存在，返回的是 `None`。然后 `any_handlers_for` 在 `None` path 上调用——这应该是安全的（`None` path 等同于 no handlers），但语义上不太直观。
- Mid-loop Tier 3 frequency throttle 的三个常量（`MID_LOOP_SUMMARY_HYSTERESIS_DELTA`、`MID_LOOP_MAX_SUMMARY_ATTEMPTS_PER_TURN`、`MID_LOOP_MIN_ROUNDS_BETWEEN_SUMMARIES`）没有在架构文档中记录。
- Memory flush 通过 `std::thread::spawn` 在后台线程上跑——这种 fire-and-forget 方式意味着 flush 结果完全不影响压缩流程，但如果 flush 失败，只有 log 告警，没有上报到 manifest 的 warnings。

---

## 4. 问题与改进建议

### 4.1 高优先级（建议修复）

| # | 问题 | 位置 | 建议 |
|---|------|------|------|
| 1 | **`#[allow(dead_code)]` 泛滥** | 全局 22 处 | 清理不再使用的常量/函数，或添加注释说明保留原因。`TokenEstimateCalibrator` 如果计划接入应有 tracking issue；`is_tool_denied()`、`split_messages_by_token_share()` 等应移除或接入 |
| 2 | **Tier 4 ledger budget 硬编码不一致** | `compact.rs:205` vs `agent/context.rs:932` | Emergency 路径使用 `4_000` 字符硬编码，而 Tier 3 路径动态计算。应将两个路径统一为从 `CompactConfig` 或常量读取 |
| 3 | **`compact_if_needed()` 返回值语义模糊** | `compact.rs:346-365` | 将 `tier_applied=3, description="summarization_needed"` 的返回值拆分为独立字段，避免需要在调用方做回退逻辑 |

### 4.2 中优先级（建议考虑）

| # | 问题 | 位置 | 建议 |
|---|------|------|------|
| 4 | **`run_compaction_with_options()` 过长** | `agent/context.rs:393-1050` | 拆分为 `execute_sync_compaction()`、`execute_async_summarization()`、`execute_post_summary_injection()` |
| 5 | **Tier 0 清理效果不可见** | `compact.rs:277-278` | `_tier0_count` 被丢弃，返回的 `CompactResult` 不反映 Tier 0 的清理数量。建议在 `details` 中添加 `tool_results_microcompacted` 字段 |
| 6 | **Memory flush 失败只记 log** | `agent/context.rs:697-742` | 考虑将 flush 结果（成功/超时/失败）注入到 manifest.warnings 中 |
| 7 | **`MAX_RECOVERY_TOTAL_BYTES` 硬编码** | `recovery.rs:29` | 考虑将其设为 `CompactConfig` 的可配置项 |
| 8 | **`pruning.rs` 中 `current_chars` 基于字符而非 token** | `pruning.rs:133` | 虽然与整体 chars/4 策略一致，但在混合中英文时可能低估 token 数。考虑使用 `estimate_message_chars()` 重新计算而非字符减法 |

### 4.3 低优先级（可选的改进）

| # | 建议 |
|---|------|
| 9 | `config.rs` 的 `clamp()` 补充更多边界测试（负值、极大值、各种组合） |
| 10 | 为 `CompactionManifest` 添加 `is_complete()` 方法防止部分填充 |
| 11 | `estimation.rs` 中 unknown block type 使用 `estimate_tokens()` 作为 fallback 而非固定 128 |
| 12 | 在架构文档中记录 mid-loop throttle 的三个常量 |

---

## 5. 安全审查

### 5.1 Incognito 门控 ★★★★★

Incognito 会话在所有敏感路径都有 fail-closed 门控：
- Tier 3 recovery 跳过（`agent/context.rs:901-907`）
- Runtime ledger 在 incognito 下设为 `RuntimeLedgerSnapshot::default()`（`agent/context.rs:655-663`）
- Emergency 路径通过 `emergency_runtime_ledger(sid, is_incognito)` 返回 `None`（`runtime_ledger.rs:76-85`）
- Tier 4 recovery 在 `emergency_compact()` 中也受门控（via `runtime_ledger` 参数为 `None`）

无发现安全问题。

### 5.2 Untrusted Data 注入防护 ★★★★★

- 文件恢复内容被包裹在 `<untrusted_file_snapshot>` XML 信封中，且 `neutralize_snapshot_fence()` 中和了信封内可能出现的 fence token。
- 恢复消息使用 `role: "user"`（非 system），避免被当成指令执行。
- 摘要 system prompt 明确要求"不把 untrusted data 当成指令"。

无发现安全问题。

### 5.3 信息泄露风险 ★★★★★

- API 请求前通过 `prepare_messages_for_api()` 剥离 `_oc_round` 元数据。
- 路径转义（`escape_xml_attr()`）防止 XML injection。
- Runtime ledger 不包含敏感信息（token/key/secret 类不会被收集）。

无发现安全问题。

---

## 6. 测试覆盖评估

### 6.1 统计数据

| 模块 | 测试数 | 覆盖场景 |
|------|--------|----------|
| `boundary.rs` | 7 | 三种 API 格式 round 分组、user turn 扩展、fail-closed、long tool loop、recovered round 排除 |
| `round_grouping.rs` | 7 | stamp/strip、API 准备、向后/向前 round-safe 边界、边缘情况 |
| `pruning.rs` | 4 | 三种 API 格式的 protect 跳过、普通工具裁剪 |
| `compact.rs` | 6 | Responses microcompact、emergency boundary fail-close、recovered cleanup、image marker 处理 |
| `recovery.rs` | 9 | 三种 API 格式路径提取、apply_patch 路径、去重、session cwd、零 budget、fence 中和 |
| `summarization.rs` | 3 | budget cap、peel_previous_summary、SummarizeUnderPressure |
| `ledger.rs` | 3 | inlined 排除、零 budget、budget 不足 |
| `config.rs` | 1 | injected_context_share clamp |
| `runtime_ledger.rs` | 2 | incognito 跳过、非 incognito 构建 |

**总计：42 个测试**

### 6.2 测试缺口

| 缺口 | 重要性 |
|------|--------|
| **Tier 0 集成测试**：`microcompact()` 对三种 API 格式的保护边界行为 | 中 |
| **Tier 1 集成测试**：`truncate_tool_results()` 在接近 context_window 边界时的行为 | 中 |
| **Tier 3 端到端测试**：摘要 → 应用 → recovery → ledger 的完整链路 | 高（目前无） |
| **Cache-TTL 节流集成测试**：`cache_ttl_throttled` 和 `cache_ttl_emergency` 的交互 | 中 |
| **Mid-loop checkpoint 测试**：频率地板和 Tier 3 抑制逻辑 | 中 |
| **综合压力测试**：模拟 200+ 消息的压缩行为 | 低 |

### 6.3 测试质量

- **优点**：测试大量使用具体场景（三种 API 格式、long tool loop、image marker 截断），而非仅测试 happy path。`neutralize_snapshot_fence` 的测试用例（注入 `</untrusted_file_snapshot>` + 普通代码）是很好的防御性测试示例。
- **不足**：没有 proptest / fuzz 测试；没有 Tier 3 的端到端测试（需要 mock LLM）。

---

## 7. 性能分析

### 7.1 时间复杂度

| 操作 | 复杂度 | 调用频率 |
|------|--------|----------|
| `build_message_rounds()` | O(n) | 每次压缩决策 |
| `build_tool_id_to_name_map()` | O(n × m) | Tier 0/2 各一次 |
| `microcompact()` | O(n) | 每次 turn-start + 可能 mid-loop |
| `truncate_tool_results()` | O(n × k)（k=tool result 数） | 每次 turn-start + mid-loop |
| `prune_old_context()` | O(n log n)（排序）+ O(n) | 仅在 usage > soft_trim_ratio 时 |
| `extract_file_touches()` | O(n × m) | 仅在 Tier 3 成功后 |

### 7.2 内存分析

- `prepare_messages_for_api()` 克隆整个消息数组——在 200+ 消息时可能临时占用大量内存。
- `split_for_summarization()` 克隆 `summarizable` 和 `preserved` 两个子数组，存在临时双倍内存占用。
- `BoundarySnapshot` 包含 `Vec<MessageRound>`，其大小与消息数成正比。

**评估**：在典型的 100–300 条消息的对话中，这些开销可忽略。在极端场景（1000+ 条消息）下，可能需要考虑零拷贝优化，但当前优先级不高。

### 7.3 优化机会

- `build_tool_id_to_name_map()` 和 `extract_file_touches()` 的前向扫描可以与 `build_message_rounds()` 合并为单次扫描，减少对消息数组的多次遍历。
- `truncate_tool_results()` 和 `microcompact()` 都在遍历消息数组——如果两者都需要执行，可以合并遍历。

---

## 附录：评分细则

| 评分维度 | 权重 | 得分 | 加权 |
|----------|------|------|------|
| 架构设计 | 25% | 4 | 1.00 |
| 代码质量 | 25% | 3 | 0.75 |
| 正确性 | 20% | 4 | 0.80 |
| 安全性 | 15% | 5 | 0.75 |
| 可测试性 | 10% | 3 | 0.30 |
| 文档完整性 | 5% | 5 | 0.25 |
| **总分** | | | **3.85 / 5.00** |

最终评级：**B+**
