# kiro-rs 架构与代码地图

> 本文档由代码扫描生成，描述当前 `src/` 实际结构（93 个 `.rs` 文件，~26.8k 行）。
> 目标读者：维护者 / 二次开发者。配置与使用说明见 [README](../README.md)。

---

## 1. 这是什么

`kiro-rs` 是一个用 Rust 写的 **协议转换代理**。它对外暴露 **Anthropic Claude API** 兼容端点，对内把请求翻译成 **Kiro / AWS CodeWhisperer（Q）** 协议，再把上游的 AWS Event Stream 二进制响应解码回 Anthropic SSE。

```
┌──────────────┐   Anthropic API    ┌──────────────────────────────┐   Kiro/CodeWhisperer   ┌──────────────┐
│ Claude Code  │  /v1/messages      │           kiro-rs            │  generateAssistant     │  AWS Q 上游   │
│ / 任意客户端  │ ─────────────────> │  转换 → 多凭据 → 解码 → 回写   │ ─────────────────────> │  (event-stream)│
└──────────────┘  <───── SSE ────── └──────────────────────────────┘  <──── 二进制帧 ─────── └──────────────┘
                                            │         ▲
                                       Admin API   Admin UI (嵌入式 React)
```

核心难点都在「中转层补齐上游不支持的能力」：
- 上游**不支持** Anthropic 的 prompt caching → 自实现多断点前缀缓存与计费（`anthropic/prompt_cache.rs` + `cache_accounting.rs`）。
- 上游用 **AWS Event Stream** 二进制帧 → 自写状态机解码器（`kiro/parser/`）。
- 上游按账号有**配额/限流** → 多凭据池 + 故障转移 + 瞬态冷却（`kiro/token_manager/`）。
- 上游 thinking / 模型名语义与 Anthropic 不一致 → 预处理规范化（`anthropic/preprocess.rs` + `converter/model_map.rs`）。

---

## 2. 请求生命周期（热路径）

以 `POST /v1/messages`（流式）为例，串起所有核心模块：

```
1. 路由 + 认证
   anthropic/router.rs  ──auth_middleware──>  anthropic/handlers.rs::post_messages
        (CORS / 50MB body limit / 60s body 读超时)

2. 请求预处理
   anthropic/preprocess.rs   inject_system_prompt() 剥离客户端限制 + 注入 preset/custom
                             override_thinking_from_model_name() 规范化 thinking
   anthropic/prompt_filter.rs  剥离 Claude Code 内置安全限制片段

3. Prompt cache 查询（计费决定）
   anthropic/cache_accounting.rs::lookup_prompt_cache()  ── 查 ──>  anthropic/prompt_cache.rs
        → CacheDecision { input/cache_read/cache_creation, 复用的 conversation_id }

4. 协议转换 Anthropic → Kiro
   anthropic/converter/mod.rs::convert_request()
        ├ model_map.rs   模型名 → Kiro 模型 + 上下文窗口
        ├ session.rs     conversation_id / trigger 类型
        ├ tools.rs       工具定义转换、tool_use/tool_result 配对校验
        ├ content.rs     文本/图片/工具结果、tool_choice 指令
        └ history.rs     历史消息合并、thinking 前缀注入
   → kiro/model/requests/* 的请求体

5. 上游调用（多凭据 + 重试）
   handler 持有 Arc<dyn UpstreamProvider>（trait，解耦具体实现）
   → kiro/provider.rs::KiroProvider::call_api_stream()
        ├ kiro/token_manager/   acquire_context() 选凭据 + 拿并发 permit + 保证 token 有效
        ├ kiro/endpoint/ide.rs  注入 profileArn、AWS UA、组装 URL/headers
        └ http_client.rs        reqwest 客户端（代理：凭据级 > 全局）

6. 响应解码 + SSE 转换
   kiro/parser/decoder.rs   AWS event-stream 二进制帧 → kiro/model/events/* 事件
   anthropic/stream/context.rs::process_kiro_event()  事件 → Anthropic SSE
        ├ sse_state.rs   SSE 事件 / 块状态机
        ├ thinking.rs    <thinking> 标签检测提取
        ├ signature.rs   伪 signature / message id
        └ tokens.rs      token 估算与按预算截断
   (/cc 端点改用 stream/buffered.rs：等 contextUsageEvent 后再发 message_start)

7. 收尾
   kiro/token_manager/failure.rs   成功/失败/瞬态错误上报 → 冷却 / 切换凭据
   anthropic/cache_accounting.rs::record_cache_outcome()  回写缓存断点
   kiro/metrics.rs   写 RequestRecord 到环形缓冲（Admin 指标用）
   出错时 anthropic/error_map.rs   上游错误 → Anthropic 错误协议 + Retry-After
```

非流式 `call_api()` 路径相同，只是在第 6 步聚合完整响应而非逐帧 SSE。

---

## 3. 模块分类（按职责）

### 3.1 入口 / 装配
| 文件 | 职责 |
|---|---|
| `src/main.rs` | 进程入口。解析参数、加载 config/credentials、装配 `MultiTokenManager` → `KiroProvider` → 三个 Router（Anthropic / Admin / Admin UI），启动 axum |
| `src/http_client.rs` | 统一构建 `reqwest::Client`，封装 `ProxyConfig`（HTTP/SOCKS5 + 认证）与 TLS 后端选择 |

### 3.2 配置模型 `src/model/`
| 文件 | 职责 |
|---|---|
| `arg.rs` | clap 命令行参数（`--config` / `--credentials`） |
| `config.rs` | `config.json` 全量结构体：监听、region、代理、冷却、并发、prompt cache、system prompt、端点等 |
| `runtime.rs` | 运行时可热改配置（`Arc<RwLock<…>>`）：retry / system-prompt，Admin 写入时同步回写 `config.json` |

### 3.3 公共工具 `src/common/`
| 文件 | 职责 |
|---|---|
| `auth.rs` | API Key 校验（常量时间比较，防时序攻击） |
| `hash.rs` | 通用哈希工具 |
| `io.rs` | 原子写文件，敏感文件 `_secure` 变体自动 `chmod 0600` |
| `redact.rs` | 错误消息脱敏，防 token reflection |

### 3.4 Anthropic 兼容层 `src/anthropic/`

**HTTP 骨架**
| 文件 | 职责 |
|---|---|
| `router.rs` | `/v1` 与 `/cc/v1` 路由、CORS、body 限制与超时 |
| `middleware.rs` | `AppState`（共享 provider/config/cache）、API Key 认证中间件 |
| `handlers.rs` | 四个 handler：`get_models` / `post_messages` / `post_messages_cc` / `count_tokens` |
| `types.rs` | Anthropic 请求/响应类型定义 |
| `error_map.rs` | 上游 `anyhow::Error` → Anthropic 错误协议 + `Retry-After` |
| `models.rs` | `/v1/models` 列表：上游映射 + 内置静态回退 |

**请求预处理 / 提示词治理**
| 文件 | 职责 |
|---|---|
| `preprocess.rs` | system prompt 注入/剥离、thinking 配置规范化（Opus 4.7 `enabled→adaptive`） |
| `prompt_filter.rs` | 剥离 Claude Code 内置安全限制/沙箱指令片段 |
| `prompt_presets.rs` | 内置预设库（`override`/`pentest`/`nsfw`/`code_complete`/`concise`），content 来自 `presets/*.md` |
| `document.rs` | PDF/document 块 base64 解码抽文字，塞回 user message（上游不接受 document 块） |
| `token_count.rs` | 文本 token 估算（中西文分档系数） |

**协议转换 `converter/`**
| 文件 | 职责 |
|---|---|
| `mod.rs` | 入口 `convert_request()`，编排下列子模块 |
| `model_map.rs` | 模型名映射 + 上下文窗口判断 |
| `session.rs` | conversation_id 提取、trigger 类型判断 |
| `tools.rs` | 工具定义转换、工具名缩短、tool_use/tool_result 配对校验 |
| `content.rs` | 文本/图片/工具结果提取、tool_choice 指令、图片格式 |
| `history.rs` | 历史消息构建：thinking 前缀注入、user/assistant 合并 |

**Prompt cache（中转层自实现）**
| 文件 | 职责 |
|---|---|
| `prompt_cache.rs` | 缓存**存储**：多断点指纹 LRU、conversation_id 复用表、TTL 分桶（5m/1h） |
| `cache_accounting.rs` | 缓存**计费**：把存储查询翻译成 per-request 的 input/cache_creation/cache_read 三字段 |

**流式响应 `stream/`**
| 文件 | 职责 |
|---|---|
| `context.rs` | `StreamContext` 热路径核心：Kiro 事件 → Anthropic SSE |
| `buffered.rs` | `/cc` 端点缓冲版：等 contextUsageEvent 再发 message_start（input_tokens 准确） |
| `sse_state.rs` | `SseEvent` / `SseStateManager` 块状态机 |
| `thinking.rs` | `<thinking>` 标签检测提取（文本协议） |
| `signature.rs` | 伪 signature / message id（Protobuf/varint） |
| `tokens.rs` | token 估算与按预算截断 |

**工具**
| 文件 | 职责 |
|---|---|
| `websearch.rs` | WebSearch 工具 ↔ Kiro MCP 转换与响应生成 |

### 3.5 Kiro 客户端 `src/kiro/`

**核心**
| 文件 | 职责 |
|---|---|
| `provider.rs` | `KiroProvider`：`call_api` / `call_api_stream` / `call_mcp` / `list_upstream_models`，多凭据故障转移 + 重试循环。对外通过 `UpstreamProvider` trait（dyn 兼容、装箱 future）暴露，让 Anthropic 层不硬依赖具体实现，便于测试替换 |
| `machine_id.rs` | 设备指纹生成 |
| `metrics.rs` | `MetricsRecorder` 请求级指标，写 8192 容量环形缓冲，Admin 按滑窗算分位 |

**端点抽象 `endpoint/`**
| 文件 | 职责 |
|---|---|
| `mod.rs` | `KiroEndpoint` trait：URL/headers/body 的端点差异点 + 限流分类默认实现 |
| `ide.rs` | IDE 端点实现（CodeWhisperer `q.{region}.amazonaws.com`，注入 profileArn + aws-sdk UA） |

**Token 管理 `token_manager/`**
| 文件 | 职责 |
|---|---|
| `mod.rs` | `MultiTokenManager` 主结构与状态 |
| `acquire.rs` | `acquire_context()` 热路径：选凭据 + 并发 permit + 保证 token 有效 |
| `selection.rs` | 凭据选择与负载均衡（priority/balanced）、有效冷却计算 |
| `refresh.rs` | Token 刷新调度（singleflight 锁防刷新风暴） |
| `failure.rs` | 成功/失败/瞬态错误上报与凭据切换 |
| `failure_kind.rs` | 上游瞬态错误分类（provider↔manager 契约）、可疑目录分组 |
| `persistence.rs` | 凭据/统计/负载均衡模式原子持久化 |
| `admin_ops.rs` | Admin CRUD：snapshot / 启停 / 优先级 / 增删 / 余额 / 模式切换 |

**AWS Event Stream 解析 `parser/`**
| 文件 | 职责 |
|---|---|
| `mod.rs` | 解析器入口 |
| `decoder.rs` | 流式解码状态机（Ready/Parsing/Recovering/Stopped 四态，容错续传） |
| `frame.rs` | 消息帧解析（Total/Header Length + Prelude CRC + Headers + Payload + Msg CRC） |
| `header.rs` | 帧头部解析 |
| `crc.rs` | CRC32（ISO-HDLC）校验 |
| `error.rs` | 解析错误类型 |

**数据模型 `model/`**
| 文件/目录 | 职责 |
|---|---|
| `credentials.rs` | OAuth 凭证模型，单/多凭据格式加载 |
| `token_refresh.rs` | Token 刷新请求/响应模型 |
| `usage_limits.rs` | `getUsageLimits` 余额查询模型 |
| `events/` | 流式响应事件：`assistant` / `tool_use` / `reasoning`(4.8+ 原生 thinking) / `context_usage` / `base` |
| `requests/` | 上行请求体：`kiro`(主结构) / `conversation` / `tool` |

### 3.6 Admin API `src/admin/`
| 文件 | 职责 |
|---|---|
| `mod.rs` | 导出 `AdminState` / `AdminService` / `create_admin_router` |
| `router.rs` | `/api/admin/*` 路由表（见 §4） |
| `middleware.rs` | `AdminState` + Admin API Key 认证 |
| `handlers.rs` | HTTP handler 层 |
| `service.rs` | 业务逻辑：凭据 CRUD、system-prompt、cache、retry 配置 |
| `metrics.rs` | 把 `MetricsRecorder` 原始 buffer 聚合成 Admin UI / Prometheus 视图（请求时计算） |
| `types.rs` | Admin API 类型 |
| `error.rs` | Admin 错误类型 |

### 3.7 Admin UI `src/admin_ui/` + `admin-ui/`
| 路径 | 职责 |
|---|---|
| `src/admin_ui/router.rs` | rust-embed 把 `admin-ui/dist/` 嵌进二进制，`/admin` 静态服务 |
| `admin-ui/` | React + TS + Vite + Tailwind 前端工程（构建产物会被嵌入） |

前端结构：`components/`（dashboard、credential-card、各 dialog、metrics-* 面板）、`components/ui/`（基础组件）、`hooks/`（use-credentials / use-metrics / use-system-prompt）、`api/` + `types/`。

---

## 4. Admin API 端点

需 `admin_api_key`（`config.json`，空字符串视为未启用）。

| 方法 | 路径 | 用途 |
|---|---|---|
| GET | `/api/admin/metrics` | 聚合指标（滑窗分位/计数） |
| GET | `/api/admin/metrics/prometheus` | Prometheus 文本格式 |
| GET/PUT | `/api/admin/runtime/retry-config` | 重试配置热读写 |
| GET/PUT | `/api/admin/runtime/prompt-cache-config` | prompt cache 配置热读写 |
| POST | `/api/admin/runtime/prompt-cache-config/clear` | 清空 prompt cache |
| GET/POST | `/api/admin/credentials` | 列出 / 新增凭据 |
| DELETE | `/api/admin/credentials/{id}` | 删除凭据 |
| POST | `/api/admin/credentials/{id}/disabled` | 启用/禁用 |
| POST | `/api/admin/credentials/{id}/priority` | 改优先级 |
| POST | `/api/admin/credentials/{id}/group` | 改分组 |
| POST | `/api/admin/credentials/{id}/reset` | 重置失败计数 |
| POST | `/api/admin/credentials/{id}/refresh` | 强制刷新 token |
| GET | `/api/admin/credentials/{id}/balance` | 查余额 |
| GET/PUT | `/api/admin/config/load-balancing` | 负载均衡模式 |
| GET/PUT | `/api/admin/config/system-prompt` | system prompt 配置 |
| GET | `/api/admin/config/system-prompt/presets` | 列出预设 |
| GET | `/api/admin/config/system-prompt/presets/{id}` | 预设内容 |
| POST | `/api/admin/config/system-prompt/user-presets` | 新增用户预设 |
| PUT/DELETE | `/api/admin/config/system-prompt/user-presets/{id}` | 改/删用户预设 |

---

## 5. 跨模块共享的状态

进程启动时在 `main.rs` 构建、被多处 `Arc` 共享：

- **`MultiTokenManager`** — provider 取凭据 / Admin 管凭据，共享同一池。
- **`UpstreamProvider`（trait）** — Anthropic handler 通过 `Arc<dyn UpstreamProvider>` 调上游，`KiroProvider` 是唯一实现；解耦让 handler 不硬依赖具体类型，可注入 mock 测试。
- **`MetricsRecorder`** — provider 写记录 / Admin 读聚合。
- **`PromptCache`** — Anthropic handler 命中计费 / Admin 清缓存，共享同一实例。
- **`SharedPromptConfig` / `SharedRetryConfig`**（`model/runtime.rs`）— handler 读 / Admin 热改，改动同步回写 `config.json`。
- **`config_writer: Arc<Mutex<Config>>`** — Admin 写 system prompt 等字段时回写磁盘。

---

## 6. 技术栈

Rust 2024 / axum 0.8 / tokio / reqwest（rustls 或 native-tls）/ serde（preserve_order）/ tracing / clap / parking_lot / rust-embed（嵌前端）/ lopdf（PDF 抽取）/ sha2 + crc + subtle（缓存指纹 / event-stream CRC / 常量时间比较）。

测试随实现下沉到各模块的 `#[cfg(test)] mod tests`。
</content>
</invoke>
