# Hope Agent 技术架构分析报告

> 生成时间：2026-06-20 | 版本：v0.10.0 | 分析范围：全栈（Rust 后端 + React 前端 + 部署 + CI/CD）

---

## 目录

1. [项目概览](#1-项目概览)
2. [技术栈全景](#2-技术栈全景)
3. [架构分层设计](#3-架构分层设计)
4. [核心后端 (ha-core)](#4-核心后端-ha-core)
5. [服务器层 (ha-server)](#5-服务器层-ha-server)
6. [桌面 Shell (src-tauri)](#6-桌面-shell-src-tauri)
7. [前端架构](./src)](#7-前端架构)
8. [数据持久化与存储](#8-数据持久化与存储)
9. [通信与 Transport 抽象](#9-通信与-transport-抽象)
10. [安全体系](#10-安全体系)
11. [多语言与国际化](#11-多语言与国际化)
12. [CI/CD 与部署](#12-cicd-与部署)
13. [代码规模统计](#13-代码规模统计)
14. [关键设计决策与亮点](#14-关键设计决策与亮点)

---

## 1. 项目概览

Hope Agent 是一款**基于 Rust + React 的本地 AI 助手桌面应用**，支持三种运行模式：

| 模式 | 入口 | 说明 |
|------|------|------|
| 桌面 GUI | `pnpm tauri dev` / Tauri 2 App | 完整图形界面，用户直接交互 |
| HTTP/WS 守护进程 | `hope-agent server start` | REST API + WebSocket 流式，可 Docker 部署 |
| ACP stdio | `hope-agent acp` | IDE 协议直连（类 LSP） |

**核心设计目标：一切复杂逻辑在 ha-core（零 Tauri 依赖），前端只负责展示和交互，Tauri 和 HTTP 服务都是薄壳。**

- **仓库地址**：`https://github.com/shiwenwen/hope-agent`
- **许可证**：MIT
- **开发语言**：Rust (edition 2021, toolchain 1.95) + TypeScript (React 19)
- **包管理器**：Cargo (Rust) + pnpm (Node.js)
- **当前版本**：v0.10.0

---

## 2. 技术栈全景

| 层次 | 技术选型 | 版本/说明 |
|------|----------|-----------|
| **前端框架** | React + TypeScript | React 19，552 个 TS/TSX 源文件 |
| **构建工具** | Vite | Vite 8 |
| **CSS 框架** | Tailwind CSS | v4 |
| **UI 组件库** | shadcn/ui | 基于 Radix UI |
| **桌面框架** | Tauri | Tauri 2 |
| **HTTP 服务器** | axum | axum 0.8 |
| **CLI 框架** | clap | Rust CLI 参数解析 |
| **异步运行时** | tokio | full features |
| **数据库** | SQLite (rusqlite) | WAL 模式 + FTS5 + vec0 向量扩展 |
| **HTTP 客户端** | reqwest | 0.12，含 stream/multipart/tls |
| **序列化** | serde + serde_json | 1.0 |
| **渲染引擎** | Streamdown + Shiki + KaTeX + Mermaid | 流式 Markdown 渲染 |
| **MCP 客户端** | rmcp | 1.5，支持 stdio + SSE + Streamable HTTP + OAuth |
| **多语言** | i18next | 12 种语言 |
| **测试** | Vitest (前端) + Cargo test (后端) | Rust 含单元测试 + 集成测试 |

---

## 3. 架构分层设计

### 3.1 三层物理分层

```
┌──────────────────────────────────────────────┐
│              Frontend (React 19)              │
│    ChatUI / Settings / Dashboard / Cron / ... │
└──────────────────┬───────────────────────────┘
                   │ Transport 抽象层
                   │ (transport.ts)
          ┌────────┴────────┐
          ▼                 ▼
┌──────────────────┐ ┌──────────────────┐
│  src-tauri       │ │  ha-server       │
│  (Tauri 2 薄壳)  │ │  (axum HTTP/WS)  │
│  76 个 .rs 文件   │ │  63 个 .rs 文件   │
└────────┬─────────┘ └────────┬─────────┘
         │                    │
         └────────┬───────────┘
                  ▼
┌──────────────────────────────────────────────┐
│            ha-core (Rust 核心库)              │
│         674 个 .rs 文件，零 Tauri 依赖         │
│   agent / chat_engine / tools / memory /     │
│   knowledge / skills / channel / cron / ...  │
└──────────────────────────────────────────────┘
```

### 3.2 配置管理：单一真相源

整个进程只有一份内存中的 `AppConfig`，通过 `ArcSwap<AppConfig>` 实现无锁并发读：

- **读**：`cached_config()` → `Arc<AppConfig>`，lock-free，O(ns) 级开销
- **写**：`mutate_config((category, source), |cfg| { ... })`，全局 Mutex 串行化，自动落盘到 `~/.hope-agent/config.json`，并 emit `config:changed` 事件
- **备份**：写操作自动生成 autosave 备份到 `~/.hope-agent/backups/autosave/`，支持 Settings → Backups → Rollback 面板回滚

### 3.3 Cargo Workspace 结构

```toml
[workspace]
members = [
    "src-tauri",           # Tauri 桌面 Shell (76 .rs 文件)
    "crates/ha-core",      # 核心业务逻辑 (674 .rs 文件，50 个直接依赖)
    "crates/ha-server",    # HTTP/WS 服务器 (63 .rs 文件)
]
resolver = "2"
```

---

## 4. 核心后端 (ha-core)

ha-core 是整个项目的核心，包含 **674 个 Rust 源文件**，是代码量最大、逻辑最密集的模块。以下按子系统逐一分析。

### 4.1 对话引擎 (chat_engine/)

**入口**：`run_chat_engine()` 函数，统一处理 4 种请求来源：

| 来源 | EventSink 实现 | 说明 |
|------|---------------|------|
| 桌面 GUI | `ChannelSink`（Tauri IPC Channel） | 事件直推 WebView |
| HTTP/WS | `NoopEventSink` + `chat:stream_delta` EventBus | 浏览器通过 `/ws/events` 接收 |
| IM Channel | `ChannelStreamSink`（EventBus + mpsc） | Telegram / WeChat 等渠道 |
| Cron 定时 | `NoopEventSink` | 定时任务结果由 Cron delivery 处理 |
| ACP stdio | stdio 协议输出层 | IDE 直连 |

**核心组件**：
- `context.rs`：Agent 构建 + 上下文恢复/保存 + 工具事件持久化
- `engine.rs`：核心引擎入口
- `persister.rs`：StreamPersister — 流式增量累积 + flush 到 SessionDB
- `stream_broadcast.rs`：`chat:stream_delta` / `chat:stream_end` 事件广播
- `stream_seq.rs`：ChatSource 枚举 + 流序号注册表（重载恢复去重）
- `im_mirror.rs`：GUI/HTTP 入口的 IM live 流式镜像（SinkRegistry fan-out）
- `sink_registry.rs`：次级 sink 注册与 fan-out 管理

**关键特性**：
- **流式事件协议**：引擎内部通过 EventSink trait 抽象，支持多路 fan-out
- **Turn Lifecycle & Stop Recovery**：支持用户中断后安全恢复
- **Failover 集成**：模型调用失败时自动降级到备用模型链
- **Post-turn Effects**：对话轮次结束后的记忆提取、上下文压缩等后处理
- **记忆提取门控**：基于轮次特征的记忆提取决策

### 4.2 Agent 管理 (agent/)

**API 协议** (4 种)：

```
ApiType
├── Anthropic      # /v1/messages
├── OpenaiChat     # /v1/chat/completions
├── OpenaiResponses # /v1/responses
└── Codex          # ChatGPT OAuth
```

**参数注入**：`ChatEngineParams` 一次性构建所有请求参数，包括：
- 基础信息（session_id, agent_id, message, attachments）
- 模型降级链（model_chain）
- Provider 配置快照（避免并发读取时的竞态）
- Agent 配置（温度覆盖、web_search 开关等）

**工具注入决策**：基于 ToolTier 分层模型，由 `tools::dispatch::resolve_tool_fate` 单入口统一决策每个工具是否注入 LLM 请求的 `tools[]`。

### 4.3 工具系统 (tools/)

**50 个内置工具**，188 处 `ToolDefinition` 声明，按 4 层 Tier 分层：

| Tier | 名称 | 说明 | 工具数 |
|------|------|------|--------|
| 1 | Core | 核心基础，强制注入，UI 不显示开关 | 文件系统、交互、SessionAware、Meta、PlanMode |
| 2 | Standard | Agent 默认开启，用户可关闭 | web_fetch, browser, team, pdf, image, weather 等 |
| 3 | Configured | 需全局配置才能启用 | web_search, image_generate, canvas, subagent 等 |
| 特殊 1 | Memory | 由 agent 级 `memory.enabled` 控制 | save_memory, recall_memory 等 6 个工具 |
| 特殊 2 | MCP | 由 `capabilities.mcpEnabled` 控制 | mcp_resource, mcp_prompt + 动态 `mcp__<server>__<tool>` |

**ToolDefinition 结构**：
```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,       // JSON Schema
    pub tier: ToolTier,          // 注入决策的单一真相源
    pub internal: bool,          // 是否豁免审批（与 tier 正交）
    pub concurrent_safe: bool,   // 同轮可并行执行
    pub async_capable: bool,     // 可 detach 成后台 job
}
```

**关键特性**：
- `internal` 与 `tier` 正交：`exec` / `write` 是 Core 但 `internal=false`（需审批），`recall_memory` 也是 Core 但 `internal=true`（自治只读）
- `async_capable`：工具可被 detach 成后台 job，由 `async_jobs/` 模块管理
- `defer_capable`：工具支持被用户放入 deferred 池

### 4.4 Provider 系统 (provider/)

**45 个内置 Provider 模板，335 个预设模型**，分四个类别：

| 模板文件 | 分类 | 数量 |
|----------|------|------|
| `international.ts` | 国际 Provider | 8 个 |
| `china.ts` | 国内 Provider | 11 个 |
| `infrastructure.ts` | 基础设施/聚合 Provider | 20 个 |
| `local.ts` | 本地 Provider | 4 个（Ollama 等） |

**核心类型**：

- **`ProviderConfig`**：Provider 完整配置（id, name, api_type, base_url, api_key, models, thinking_style）
- **`ModelConfig`**：模型配置（id, input_types, context_window, max_tokens, reasoning, thinking_style, cost）
- **`ThinkingStyle`**：5 种推理格式（Openai reasoning_effort, Anthropic budget_tokens, Zai, Qwen enable_thinking, None）

**降价链 (Failover)**：`AppConfig.primary_model` + `fallback_models` 构成模型降级链，当主模型调用失败时自动切换备用模型。

### 4.5 记忆系统 (memory/)

- **存储**：SQLite 数据库，支持向量检索
- **门控**：记忆提取由 Chat Engine 的"记忆提取门控"决定何时触发
- **工具**：6 个记忆工具（save/recall/update/delete/memory_get/update_core_memory）
- **相关文档**：`docs/architecture/memory.md`

### 4.6 知识库 (knowledge/)

- **功能**：知识空间管理，支持文档上传和检索
- **存储**：SQLite + FTS5 全文搜索 + vec0 向量扩展
- **前端**：`src/components/knowledge/` 提供知识库管理界面

### 4.7 技能系统 (skills/)

**33 个技能文件**，分两类：
- **Vendor skills**（来源第三方，记录在 `THIRD_PARTY_NOTICES.md`）：编程方法论文档
- **原创 skills**：办公方法论等

### 4.8 Plan Mode (plan/)

- **功能**：结构化的多步骤任务规划与执行
- **流程**：submit_plan → update_plan_step（逐个步骤执行）→ amend_plan（修改计划）
- **工具**：专用 Plan 工具（submit_plan, update_plan_step, amend_plan），PlanAgentMode 时注入

### 4.9 子代理 (subagent/)

- **功能**：主 Agent 可 spawn 子代理处理独立任务
- **隔离**：每个子代理有独立的上下文窗口
- **通信**：子代理结果通过 EventBus 回传

### 4.10 Team (team/)

- **功能**：多个 Agent 协作完成复杂任务
- **协调**：主 Agent 作为协调者，分发任务给团队成员

### 4.11 定时任务 (cron/)

- **功能**：定时触发对话、通知等
- **配置**：通过前端 Cron UI 管理
- **存储**：SQLite 持久化

### 4.12 ACP 协议 (acp/)

- **功能**：IDE 直连协议（类 LSP），通过 stdio 通信
- **入口**：`hope-agent acp`
- **文档**：`docs/architecture/acp.md`

### 4.13 Channel 系统 (channel/)

- **支持渠道**：Telegram、WeChat 等多平台渠道
- **消息流**：Channel → `ChannelStreamSink` → Chat Engine → 响应回 Channel
- **镜像**：GUI/HTTP 入口的 IM live 流式镜像（`im_mirror.rs`）

### 4.14 上下文压缩 (context_compact/)

- **功能**：长对话自动压缩上下文，控制 token 消耗
- **触发**：基于 token 预算的自动触发或手动触发

### 4.15 安全子系统 (security/)

- **沙箱**：文件操作沙箱，控制读写范围
- **审批**：高权限操作需用户确认（permission.rs）
- **网络**：SSRF 防护、URL 白名单

### 4.16 其他子系统

| 模块 | 功能 |
|------|------|
| `recap/` | 对话摘要与回顾 |
| `dashboard/` | Dashboard 数据聚合（系统状态、使用统计） |
| `awareness/` | 行为感知与上下文增强 |
| `failover/` | 模型降级与错误恢复 |
| `wakeup/` | Agent 自我定时唤醒 |
| `platform/` | 平台检测与环境信息 |
| `project/` | 项目工作目录管理 |
| `session/` | 会话生命周期管理 |
| `logging/` | 日志系统（分类、轮转） |
| `local_llm/` | 本地 LLM 后端集成（Ollama） |
| `async_jobs/` | 异步后台任务管理 |
| `hooks/` | 钩子系统（pre/post tool use 等生命周期事件） |

---

## 5. 服务器层 (ha-server)

ha-server 是基于 **axum 0.8** 的 HTTP/WebSocket 服务器，63 个 Rust 源文件，约 594 处路由注册。

### 5.1 核心组件

| 组件 | 文件 | 说明 |
|------|------|------|
| 路由注册 | `lib.rs` | axum Router，~430 REST 端点 |
| WebSocket | `routes/ws.rs` | `/ws/events` 流式事件推送 |
| CLI | `bin/hope-agent.rs` | clap CLI 入口（server/acp 子命令） |
| 配置 | `config.rs` | 绑定地址（默认 `127.0.0.1:8420`） |
| 中间件 | `middleware/` | 认证、日志、CORS |

### 5.2 REST API 端点分类

- **Health**：`GET /api/health`
- **Sessions**：`POST /sessions`, `GET /sessions`, `GET /sessions/{id}`, DELETE 等
- **Chat**：流式对话、消息历史
- **Config**：配置读写、Provider 管理
- **Tools**：工具调用、审批
- **Channel**：渠道管理
- **Cron**：定时任务管理
- **Knowledge**：知识库 CRUD
- **ACP**：ACP 协议管理

### 5.3 WebSocket 实时流

`/ws/events` 端点通过 EventBus 订阅以下事件：
- `chat:stream_delta`：对话流式增量
- `chat:stream_end`：对话流结束
- `channel:stream_delta`：渠道流式增量
- `config:changed`：配置变更通知
- 其他系统事件

### 5.4 运行模式

```bash
# 前台启动
hope-agent server start

# 注册系统服务（macOS launchd / Linux systemd）
hope-agent server install
hope-agent server uninstall
hope-agent server status
hope-agent server stop
```

---

## 6. 桌面 Shell (src-tauri)

src-tauri 是 Tauri 2 桌面薄壳，76 个 Rust 源文件，628 处 `#[tauri::command]` 注解，44 个命令文件。

### 6.1 命令文件分类

| 文件 | 功能域 |
|------|--------|
| `chat.rs` | 对话相关命令 |
| `config.rs` | 配置管理命令 |
| `provider/crud.rs` | Provider CRUD |
| `provider/models.rs` | 模型管理 |
| `provider/test_*.rs` | Provider 测试 |
| `agent_mgmt.rs` | Agent 管理 |
| `session.rs` | 会话管理 |
| `knowledge.rs` | 知识库管理 |
| `filesystem.rs` | 文件系统操作 |
| `memory.rs` | 记忆管理 |
| `skills.rs` | 技能管理 |
| `cron.rs` | 定时任务 |
| `channel.rs` | 渠道管理 |
| `browser.rs` | 浏览器控制 |
| `mcp.rs` | MCP 协议 |
| `plan.rs` / `plan_index.rs` | Plan 模式 |
| `subagent.rs` | 子代理 |
| `team.rs` | Team 协作 |
| `dashboard.rs` | Dashboard |
| `recap.rs` | 回顾 |
| `acp_control.rs` | ACP 控制 |
| `tasks.rs` / `runtime_tasks.rs` | 任务管理 |
| `dreaming.rs` | Dreaming 模式 |
| `project.rs` / `project_fs.rs` | 项目管理 |
| `local_llm.rs` | 本地 LLM |
| `local_embedding.rs` | 本地 Embedding |
| `local_model_*.rs` | 本地模型管理 |
| `auth.rs` | 认证 |
| `crash.rs` | 崩溃处理 |
| `docker.rs` | Docker 管理 |
| `logging.rs` | 日志 |
| `misc.rs` | 杂项命令 |
| `onboarding.rs` | 新手引导 |
| `stt.rs` | 语音转文本 |
| `url_preview.rs` | URL 预览 |
| `permission.rs` | 权限管理 |
| `background_jobs.rs` | 后台任务 |
| `tauri_wrappers.rs` | Tauri 包装器 |

### 6.2 Tauri 内嵌 HTTP 服务

桌面模式下，`setup.rs` 会启动内嵌的 ha-server 实例，前端通过 Transport 层自动切换到 HTTP 通信（而非 Tauri IPC）。这保证了两种模式（桌面 + 浏览器）的 API 一致性。

---

## 7. 前端架构

### 7.1 目录结构

**552 个 TS/TSX 源文件**，分布在以下主要目录：

```
src/
├── components/
│   ├── chat/          # 对话界面（聊天窗口、消息渲染、附件等）
│   ├── settings/      # 设置界面（Provider 配置、工具配置、Agent 配置等）
│   ├── dashboard/     # Dashboard 面板（系统状态、使用统计）
│   ├── cron/          # 定时任务管理界面
│   ├── knowledge/     # 知识库管理界面
│   ├── config/        # 配置管理组件
│   ├── onboard/       # 新手引导
│   ├── plans/         # Plan 模式执行界面
│   ├── tasks/         # 任务管理
│   ├── team/          # Team 协作界面
│   ├── local-model/   # 本地模型管理
│   ├── common/        # 通用组件
│   ├── ui/            # shadcn/ui 基础组件
│   └── icons/         # 图标组件
├── lib/
│   ├── transport.ts           # Transport 抽象接口
│   ├── transport-tauri.ts     # Tauri IPC 实现
│   └── transport-http.ts      # HTTP SDK 实现
├── i18n/
│   └── locales/               # 12 种语言翻译文件
└── hooks/                      # React Hooks
```

### 7.2 前端依赖

**62 个运行时依赖 + 26 个开发依赖**，主要技术栈：

| 类别 | 依赖 |
|------|------|
| UI 框架 | react 19, react-dom |
| UI 组件 | @radix-ui/*, shadcn/ui |
| 路由 | react-router-dom 7 |
| 状态管理 | zustand |
| 渲染 | streamdown (流式 Markdown), shiki (语法高亮), katex (数学公式), mermaid (图表) |
| 多语言 | i18next, react-i18next |
| 构建 | vite 8, tailwindcss 4, typescript |
| 测试 | vitest, @testing-library/react |

### 7.3 Transport 抽象层

前端通过 Transport 抽象层实现与后端的解耦：

```
Transport (interface)
├── TauriTransport   → Tauri IPC (invoke + Channel)
└── HttpTransport    → HTTP REST + WebSocket
```

- **桌面模式**：自动使用 `TauriTransport`（通过 `invoke` 调用 Tauri commands 和 `Channel` 接收流式事件）
- **浏览器模式**：自动使用 `HttpTransport`（HTTP REST 调用 + WebSocket 订阅 `/ws/events` 接收流式事件）
- **切换**：`getTransport()` 函数根据运行环境自动选择

---

## 8. 数据持久化与存储

### 8.1 数据库

**SQLite (WAL 模式)**，通过 `rusqlite` 0.39 访问：

| 数据库 | 路径 | 存储内容 |
|--------|------|----------|
| session.db | `~/.hope-agent/` | 会话记录、消息历史 |
| memory.db | `~/.hope-agent/` | 记忆向量数据 |
| knowledge.db | `~/.hope-agent/` | 知识库文档 |
| cron.db | `~/.hope-agent/` | 定时任务状态 |
| wakeups.db | `~/.hope-agent/` | Agent 定时唤醒 |
| plans.db | `~/.hope-agent/` | Plan 模式计划 |

**扩展**：
- **FTS5**：全文搜索（知识库、记忆）
- **vec0**：向量扩展，支持语义检索

### 8.2 文件存储

| 路径 | 内容 |
|------|------|
| `~/.hope-agent/config.json` | 应用配置（Provider、Agent、UI 偏好） |
| `~/.hope-agent/backups/autosave/` | 配置自动备份（带 (category, source) tag） |
| `~/.hope-agent/logs/` | 应用日志 |

### 8.3 文件操作系统

ha-core 内置文件操作工具（`exec`, `read`, `write`, `edit`, `ls`, `grep`, `find`, `apply_patch`），支持：
- **沙箱模式**：限制文件访问范围
- **审批门控**：写/修改操作需用户确认
- **远程写闸门**：`FilesystemConfig.allowRemoteWrites`（默认 false）

---

## 9. 通信与 Transport 抽象

### 9.1 EventBus 系统

ha-core 内部的发布-订阅系统，基于 `tokio::sync::broadcast`：

| 事件名 | 说明 |
|--------|------|
| `chat:stream_delta` | 对话流式增量（HTTP 模式推送到 `/ws/events`） |
| `chat:stream_end` | 对话流结束 |
| `channel:stream_delta` | 渠道流式增量 |
| `config:changed` | 配置变更 |
| `subagent:result` | 子代理结果 |
| `tool:event` | 工具事件 |

### 9.2 流式回调处理

Chat Engine 的流式事件通过 `EventSink` trait 输出：

- **桌面模式**：`ChannelSink` → Tauri IPC Channel → 前端 `onEvent` 回调
- **HTTP 模式**：`NoopEventSink` + `chat:stream_delta` EventBus → WebSocket `/ws/events` → 前端
- **IM 模式**：`ChannelStreamSink` → `channel:stream_delta` EventBus + mpsc → IM 渠道

### 9.3 SinkRegistry Fan-out

GUI/HTTP 入口的 turn 在 IM attach 侧走 live 流式镜像：通过 `SinkRegistry` 将 `ChannelStreamSink` 注册为次级 sink，`emit_stream_event` 末尾 fan-out 到 IM 流式预览任务。

---

## 10. 安全体系

### 10.1 沙箱与权限

| 安全层 | 实现 |
|--------|------|
| 文件操作沙箱 | 限制读写路径范围，`FilesystemConfig` |
| 审批系统 | `internal=false` 的工具需用户确认 |
| SSRF 防护 | 网络请求白名单 + 路径验证 |
| 远程写闸门 | `allowRemoteWrites`（默认 false） |

### 10.2 密钥管理

- API Key 存储在 `~/.hope-agent/config.json` 中
- Codex 使用 OAuth 2.1 + PKCE 流程，无需存储 API Key
- MCP 服务器认证支持 OAuth

### 10.3 工具审批

| internal | 行为 |
|----------|------|
| `true` | 自治执行，无需用户确认（如 `recall_memory`、`task_list`） |
| `false` | 需用户审批（如 `exec`、`write`、`send_notification`） |

---

## 11. 多语言与国际化

### 11.1 支持语言

**12 种语言**，位于 `src/i18n/locales/`（12 个 JSON 翻译文件）：

- 简体中文 (zh-CN)、繁体中文 (zh-TW)
- 英语 (en)、日语 (ja)、韩语 (ko)
- 法语 (fr)、德语 (de)、西班牙语 (es)
- 葡萄牙语 (pt)、俄语 (ru)、阿拉伯语 (ar)、印地语 (hi)

### 11.2 i18n 同步工具

```bash
# 检查翻译缺失
node scripts/sync-i18n.mjs --check

# 从翻译文件补齐缺失
node scripts/sync-i18n.mjs --apply
```

---

## 12. CI/CD 与部署

### 12.1 CI 流水线

| Workflow | 文件 | 触发条件 |
|----------|------|----------|
| Lint + Test | `.github/workflows/lint.yml` | `[main, "release/**"]` |
| Rust Check | `.github/workflows/rust.yml` | `[main, "release/**"]` |

**8 项强制 Status Check**：

1. `cargo fmt --all --check`
2. `cargo clippy -p ha-core -p ha-server --all-targets --locked -- -D warnings`
3. `cargo test -p ha-core -p ha-server --locked`
4. `pnpm typecheck`
5. `pnpm lint`
6. `pnpm test`
7. Rust Build (release)
8. Frontend Build

### 12.2 Pre-push 钩子

`.husky/pre-push` 在 `git push` 时自动运行上述 6 项强制检查（Rust 3 项 + 前端 3 项）：

- **应急开关**：`HA_SKIP_PREPUSH=1`（整段跳过）/ `HA_SKIP_PREPUSH_TEST=1`（只跳 cargo test）
- **禁止 `--no-verify`**：会绕过 GPG 等其它钩子

### 12.3 Docker 部署

```bash
# 起 hope-agent
docker compose up -d

# + Ollama 本地 LLM sidecar
docker compose --profile with-ollama up -d
```

完整部署指南见 `docs/deployment/docker.md`。

### 12.4 分支与发布模型

| 分支 | 用途 |
|------|------|
| `main` | 下一个 minor 开发 |
| `release/vX.Y` | 已发布版本的维护分支 |

- **新功能**：从 `main` 切 `feat/<topic>`
- **Bug 修复**：从 `release/vX.Y` 切 `fix/vX.Y-<topic>`，合并后 cherry-pick 回 `main`
- **发版**：`pnpm version X.Y.0` + `pnpm sync:version` 同步版本号

### 12.5 版本同步

```bash
pnpm sync:version    # 以 package.json 为单一来源，同步 src-tauri 版本号
pnpm release:verify  # 校验版本一致性
```

---

## 13. 代码规模统计

### 13.1 文件统计

| 层次 | 文件数 | 说明 |
|------|--------|------|
| ha-core (Rust) | 674 | 核心业务逻辑，零 Tauri 依赖 |
| ha-server (Rust) | 63 | HTTP/WS 服务器 |
| src-tauri (Rust) | 76 | Tauri 桌面 Shell |
| 前端 (TS/TSX) | 552 | React 19 组件与逻辑 |
| 架构文档 (MD) | 47 | `docs/architecture/` |
| 技能文档 (MD) | 33 | `skills/` |
| 翻译文件 (JSON) | 12 | `src/i18n/locales/` |

### 13.2 Tauri 命令

- **628** 处 `#[tauri::command]` 注解
- 分布在 **44** 个命令文件中

### 13.3 HTTP 路由

- **594** 处 `.route()` 注册
- 约 **430** 个 REST 端点

### 13.4 工具与模型

- **50** 个内置工具（188 处 ToolDefinition 声明）
- **45** 个 Provider 模板
- **335** 个预设模型

### 13.5 依赖统计

| 项目 | 运行时依赖 | 开发依赖 |
|------|-----------|----------|
| 前端 (package.json) | 62 | 26 |
| ha-core (Cargo.toml) | 50 | — |

---

## 14. 关键设计决策与亮点

### 14.1 前后端严格分层

- **ha-core 零 Tauri 依赖**：核心逻辑不含任何 Tauri 特定代码，可在三种模式（桌面/HTTP/ACP）下复用
- **Transport 抽象层**：前端通过 `transport.ts` 接口解耦通信方式，桌面模式和 HTTP 模式无感知切换

### 14.2 配置系统设计

- **ArcSwap 无锁读**：读路径 `cached_config()` 零等待，热路径无性能开销
- **全局 Mutex 串行写**：写路径互斥，自动落盘 + emit 事件
- **(category, source) 二元组**：每个配置变更带标签，支持前端精准刷新和配置回滚

### 14.3 工具分层模型

- **4 Tier + 2 特殊路径**：按用户控制粒度分层，而非内部 flag 组合
- **单一决策入口**：`resolve_tool_fate()` 统一所有注入决策
- **正交属性**：`tier`（可见性控制）与 `internal`（审批控制）独立

### 14.4 EventSink 抽象

- **多形态实现**：`ChannelSink`（桌面直推）/ `NoopEventSink`（服务端走 EventBus）/ `ChannelStreamSink`（IM 双路输出）
- **SinkRegistry fan-out**：GUI 入口自动镜像到 IM 渠道

### 14.5 流式渲染管线

- **Streamdown**：增量 Markdown 渲染
- **Shiki**：代码高亮
- **KaTeX**：数学公式
- **Mermaid**：图表渲染

### 14.6 安全与防御性编程

- 文件操作沙箱 vs 审批门控
- 网络请求 SSRF 防护
- `FilesystemConfig.allowRemoteWrites` 远程写闸门
- 配置变更备份与回滚

### 14.7 CI/CD 质量门禁

- Pre-push 钩子自动运行 6 项检查
- GitHub ruleset 强制 8 项 Status Check 全部通过
- `-D warnings` 级别 clippy（ha-core + ha-server）

---

## 附录：架构文档索引

完整的 47 篇架构文档位于 `docs/architecture/`，涵盖以下子系统：

| 文档 | 主题 |
|------|------|
| `overview.md` | 系统架构总览 |
| `backend-separation.md` | 前后端分离架构 |
| `process-model.md` | 进程模型 |
| `config-system.md` | 配置系统 |
| `provider-system.md` | Provider 系统 |
| `agent-team.md` | Agent 与 Team |
| `chat-engine.md` | 对话引擎 |
| `tool-system.md` | 工具系统 |
| `memory.md` | 记忆系统 |
| `context-compact.md` | 上下文压缩 |
| `prompt-system.md` | 提示词系统 |
| `plan-mode.md` | Plan 模式 |
| `subagent.md` | 子代理 |
| `im-channel.md` | IM 渠道 |
| `cron.md` | 定时任务 |
| `knowledge-base.md` | 知识库 |
| `acp.md` | ACP 协议 |
| `mcp.md` | MCP 协议 |
| `skill-system.md` | 技能系统 |
| `slash-commands.md` | Slash 命令 |
| `file-operations.md` | 文件操作 |
| `sandbox.md` | 沙箱系统 |
| `security.md` | 安全体系 |
| `permission-system.md` | 权限系统 |
| `reliability.md` | 可靠性设计 |
| `failover.md` | 降级系统 |
| `session.md` | 会话管理 |
| `dashboard.md` | Dashboard |
| `recap.md` | 对话回顾 |
| `dreaming.md` | Dreaming 模式 |
| `project.md` | 项目系统 |
| `side-query.md` | 侧查询 |
| `hooks.md` | 钩子系统 |
| `canvas.md` | Canvas |
| `browser.md` | 浏览器控制 |
| `image-generation.md` | 图像生成 |
| `macos-control.md` | macOS 控制 |
| `behavior-awareness.md` | 行为感知 |
| `self-update.md` | 自更新 |
| `self-diagnosis-issue-reporting.md` | 自诊断与问题报告 |
| `local-model-loading.md` | 本地模型加载 |
| `logging.md` | 日志系统 |
| `platform.md` | 平台适配 |
| `api-reference.md` | API 参考 |
| `transport-modes.md` | Transport 模式 |
| `cli.md` | CLI 设计 |
| `ask-user.md` | Ask User 交互 |
