//! Anthropic API Handler 函数

#![allow(clippy::too_many_arguments)] // handler 需要透传 provider/cache/runtime/state 等多个上下文

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::model::config::SystemPromptPosition;
use crate::model::runtime::SharedPromptConfig;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, canonical_anthropic_model, convert_request_with_options};
use super::middleware::AppState;
use super::prompt_cache::{CacheProfile, GLOBAL_ACCOUNT, PromptCache, build_profile_from_request};
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, SystemMessage, Thinking,
};
use super::websearch;

/// 中转层 Prompt cache 决定结果
///
/// 决定本次请求要使用哪个 conversation_id，以及向客户端上报的 cache_*_input_tokens。
///
/// **多断点精确计费**：基于 [`super::prompt_cache`] 的多断点 + 最长前缀匹配算法，
/// `cache_creation` / `cache_read` 都是真实值（不再恒 0）。客户端 UI 和下游计费
/// 系统（sub2api）据此看到正确的命中分解。
///
/// **`skipped` 状态**：客户端没打任何 `cache_control: ephemeral` 标记时，整个 cache
/// 模块跳过——不查询、不回写、不上报，行为回到"无 cache"基线。
#[derive(Debug, Clone)]
pub(crate) struct CacheDecision {
    /// 命中时强制用此 conversation_id（让上游 session 缓存复用）
    pub forced_conversation_id: Option<String>,
    /// cache_read_input_tokens：命中前缀的累积 token（向客户端上报）
    pub cache_read_input_tokens: i32,
    /// cache_creation_input_tokens：本次新增写入的 token（向客户端上报）
    pub cache_creation_input_tokens: i32,
    /// 本次请求的 profile（请求成功后用于 update 回写）；skipped 时为 None
    pub profile: Option<CacheProfile>,
    /// 是否跳过 cache（客户端没显式启用）
    pub skipped: bool,
}

impl CacheDecision {
    /// 跳过 cache 的默认决定：所有 cache 字段归零，conversation_id 走默认逻辑
    fn skipped() -> Self {
        Self {
            forced_conversation_id: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            profile: None,
            skipped: true,
        }
    }
}

/// 计算 prompt cache 决定（命中查询 + usage 计算，但**不**回写——回写在请求成功后做）
///
/// `total_input_tokens`：本次请求的全量 input tokens（用于 85% 上限与 TTL 分桶）。
fn lookup_prompt_cache(
    cache: &PromptCache,
    payload: &MessagesRequest,
    total_input_tokens: i32,
) -> CacheDecision {
    // 没有任何 cache_control 标记 → 跳过，行为与"无 cache"基线一致
    let Some(profile) = build_profile_from_request(payload, total_input_tokens) else {
        tracing::trace!("prompt_cache: skipped (no cache_control marker on client request)");
        return CacheDecision::skipped();
    };

    let usage = cache.compute(GLOBAL_ACCOUNT, &profile);
    let forced_conversation_id = cache.lookup_conversation(&profile);

    tracing::debug!(
        "prompt_cache: creation={} read={} (5m={} 1h={}) breakpoints={} conv_reuse={}",
        usage.cache_creation,
        usage.cache_read,
        usage.creation_5m,
        usage.creation_1h,
        profile.breakpoints.len(),
        forced_conversation_id.is_some(),
    );

    CacheDecision {
        forced_conversation_id,
        cache_read_input_tokens: usage.cache_read,
        cache_creation_input_tokens: usage.cache_creation,
        profile: Some(profile),
        skipped: false,
    }
}

/// 请求成功后回写 cache：写入断点 fingerprint + conversation_id 复用映射
fn record_cache_outcome(
    cache: &PromptCache,
    decision: &CacheDecision,
    actual_conversation_id: &str,
) {
    if decision.skipped {
        return;
    }
    if let Some(profile) = decision.profile.as_ref() {
        cache.update(GLOBAL_ACCOUNT, profile, actual_conversation_id);
    }
}

/// 安全将 `u64` token 计数转换为 `i32`，超出范围时饱和到 `i32::MAX`
///
/// `count_all_tokens` 返回 `u64`，但下游 SSE 协议、context window 计算和
/// `CountTokensResponse` 都用 `i32`。直接 `as i32` 在极端大请求下会 wrap 成负数
/// 或被截断。此函数保证结果始终在 `[0, i32::MAX]` 范围内。
fn saturating_to_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// 将 KiroProvider 错误映射为符合 Anthropic 错误协议的 HTTP 响应
///
/// **Claude Code 客户端 retry 行为关键路径**：
///
/// Claude Code（Anthropic SDK）见到非 200 响应时按错误类型决定 retry 间隔：
/// - `429` + `Retry-After` → 严格按 header 等待（最理想）
/// - `429` 无 header → 指数退避 0.5/1/2/4/8s（但仍 retry）
/// - `5xx` → 立即 retry（短间隔），雪上加霜
///
/// 因此当上游 18 次 retry 全失败时，把内部"上游 429 风暴"信息透传成
/// `429 + Retry-After: 30s`，让客户端理解"现在是限频，等 30s 再来"，
/// 而不是用 502 让客户端立刻 retry 火上浇油。
///
/// Retry-After 取值依据：
/// - 上游 cooldown 默认 30s（见 `cooldown_seconds`）
/// - 给 30s 让号池有恢复窗口
/// - OVERAGE 限额是月度/小时窗口，给 60s 更稳
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }

    // 客户端请求总超时（CF 524 防御触发，详见 provider::REQUEST_TOTAL_TIMEOUT_SECS）
    // 内部 retry 90s 仍未拿到响应——号池打废，让客户端等 60s 让 cooldown 恢复
    if err_str.contains(crate::kiro::provider::REQUEST_TIMEOUT_MARKER) {
        tracing::error!(error = %err, "客户端请求总超时（CF 524 防御）：返回 503 + Retry-After: 60");
        return build_retryable_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Service is currently overloaded with retries. Please retry after the cooldown window.",
            60,
        );
    }

    // 上游限频/账户风控（"suspicious activity" / "Too Many Requests"）
    // 这是当前最常见的失败模式：18 次 retry 全 429 后兜底到此分支
    let is_rate_limit = err_str.contains("429")
        || err_str.contains("Too Many Requests")
        || err_str.contains("suspicious activity")
        || err_str.contains("rate limit");

    // 上游月度/小时 overage 限额耗尽
    let is_overage = err_str.contains("OVERAGE_REQUEST_LIMIT_EXCEEDED")
        || err_str.contains("limit for overages");

    if is_overage {
        tracing::error!(error = %err, "上游限额耗尽（OVERAGE）：返回 429 + Retry-After: 60");
        return build_retryable_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "Upstream account hit overage limit. Please retry after the rate limit window resets.",
            60,
        );
    }

    if is_rate_limit {
        tracing::error!(error = %err, "上游限频（429 风暴）：返回 429 + Retry-After: 120");
        return build_retryable_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "Upstream rate limit reached after multiple retries. Please retry shortly.",
            // 30 → 120：directory 维度被严风控时，30s 客户端 retry 还是同一波号→进一步加严。
            // 拉到 120s 让客户端 backoff 久点，给 directory 真正喘息恢复的窗口。
            120,
        );
    }

    // 凭据问题（401/403）：上游 token 失效但强制刷新失败
    if err_str.contains("401") || err_str.contains("403") {
        tracing::error!(error = %err, "上游认证失败：返回 503 + Retry-After: 60");
        return build_retryable_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Upstream credential authentication failed. Service temporarily unavailable.",
            60,
        );
    }

    // 其他错误（网络/未知）：保守返回 503 + Retry-After: 30
    // 比 502 更友好——Anthropic SDK 处理 503 时按 Retry-After 间隔，避免立刻 retry
    tracing::error!(error = %err, "上游未知错误：返回 503 + Retry-After: 30");
    build_retryable_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "overloaded_error",
        "Upstream API temporarily unavailable. Please retry shortly.",
        30,
    )
}

/// 构造带 Retry-After header 的 Anthropic 标准错误响应
///
/// 客户端见到 `Retry-After` 会按指定秒数等待再 retry，避免短间隔 retry 风暴
/// 进一步压垮上游。
fn build_retryable_error(
    status: StatusCode,
    error_type: &str,
    message: &str,
    retry_after_secs: u64,
) -> Response {
    let body = serde_json::to_vec(&ErrorResponse::new(error_type, message)).unwrap_or_else(|_| {
        br#"{"error":{"type":"api_error","message":"serialization failed"}}"#.to_vec()
    });

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::RETRY_AFTER, retry_after_secs.to_string())
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            // 极度退化路径：builder 不会失败但保留兜底
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("api_error", "internal error")),
            )
                .into_response()
        })
}

/// GET /v1/models
///
/// 返回可用的模型列表。优先尝试从上游（Kiro / CodeWhisperer 的
/// `ListAvailableModels`）获取真实可用模型，用其结果**过滤**内置静态列表
/// （只暴露上游确认可用的系列）；上游获取失败时回退到完整静态列表，
/// 保证该端点永不因上游问题而失败。
pub async fn get_models(State(state): State<AppState>) -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = match &state.kiro_provider {
        Some(provider) => match provider.list_upstream_models().await {
            Ok(upstream) if !upstream.is_empty() => {
                tracing::info!("上游 ListAvailableModels 返回 {} 个模型", upstream.len());
                models_from_upstream(&upstream)
            }
            Ok(_) => {
                tracing::warn!("上游模型列表为空，回退到内置静态列表");
                static_models()
            }
            Err(e) => {
                tracing::warn!("上游模型列表获取失败，回退到内置静态列表: {}", e);
                static_models()
            }
        },
        None => static_models(),
    };

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// 用上游真实元数据直接构造 `/v1/models` 列表。
///
/// 上游 [`UpstreamModel`] 已带 `modelId`/`modelName`/`maxOutputTokens`，直接映射为
/// Anthropic 格式的 [`Model`]。对支持 thinking 的 Claude 系模型额外追加 `-thinking` 变体
/// （沿用 kiro-rs 私有约定）。`max_tokens` 用上游 `maxOutputTokens` 真值（如 Opus 4.7 = 128000），
/// 缺失时回退 64000。
fn models_from_upstream(upstream: &[crate::kiro::provider::UpstreamModel]) -> Vec<Model> {
    // 上游已逝时间戳无意义，统一用一个固定 created（不影响客户端使用）
    const CREATED: i64 = 1778400000;
    let mut out = Vec::with_capacity(upstream.len() * 2);
    for m in upstream {
        // `auto` 是路由别名，不作为可选模型对外暴露
        if m.model_id == "auto" {
            continue;
        }
        let max_tokens = m.max_output_tokens.unwrap_or(64000);
        let is_claude = m.model_id.starts_with("claude");
        out.push(Model {
            id: m.model_id.clone(),
            object: "model".to_string(),
            created: CREATED,
            owned_by: if is_claude {
                "anthropic".to_string()
            } else {
                "kiro".to_string()
            },
            display_name: m.model_name.clone(),
            model_type: "chat".to_string(),
            max_tokens,
        });
        // Claude 系支持 thinking：追加 -thinking 变体
        if is_claude {
            out.push(Model {
                id: format!("{}-thinking", m.model_id),
                object: "model".to_string(),
                created: CREATED,
                owned_by: "anthropic".to_string(),
                display_name: format!("{} (Thinking)", m.model_name),
                model_type: "chat".to_string(),
                max_tokens,
            });
        }
    }
    if out.is_empty() { static_models() } else { out }
}

/// 内置静态模型列表（上游获取失败时的回退，也是过滤的元数据来源）。
fn static_models() -> Vec<Model> {
    vec![
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1778400000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1778400000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ]
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );
    // 注入自定义系统提示词
    inject_system_prompt(&mut payload, &state.prompt_config);

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = saturating_to_i32(token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ));

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 估算输入 tokens（发给 Kiro 上游的"全量" input tokens；也用于 prompt cache 的
    // 85% 上限与 TTL 分桶计算）
    let input_tokens = saturating_to_i32(token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ));

    // ===== Prompt Cache 决定（多断点精确计费 + conversation_id 复用）=====
    let cache_decision = lookup_prompt_cache(&state.prompt_cache, &payload, input_tokens);

    // 转换请求（命中时强制 conversation_id）
    let conversion_result =
        match convert_request_with_options(&payload, cache_decision.forced_conversation_id.clone())
        {
            Ok(result) => result,
            Err(e) => {
                let (error_type, message) = match &e {
                    ConversionError::UnsupportedModel(model) => {
                        ("invalid_request_error", format!("模型不支持: {}", model))
                    }
                    ConversionError::EmptyMessages => {
                        ("invalid_request_error", "消息列表为空".to_string())
                    }
                };
                tracing::warn!("请求转换失败: {}", e);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(error_type, message)),
                )
                    .into_response();
            }
        };

    // 请求转换成功后回写 cache：写入断点 fingerprint + conversation_id 复用映射
    record_cache_outcome(
        &state.prompt_cache,
        &cache_decision,
        &conversion_result.conversation_state.conversation_id,
    );

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 客户端可见的 input_tokens 应该剔除 cache_read + cache_creation 两部分
    // —— Anthropic 协议规定 input_tokens / cache_creation_input_tokens / cache_read_input_tokens
    // 三字段互斥不重叠。下游计费系统（如 sub2api）会按三者独立计价相加，重叠会导致溢价。
    let input_tokens_for_client = (input_tokens
        - cache_decision.cache_read_input_tokens
        - cache_decision.cache_creation_input_tokens)
        .max(0);

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens_for_client,
            thinking_enabled,
            tool_name_map,
            cache_decision.cache_creation_input_tokens,
            cache_decision.cache_read_input_tokens,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens_for_client,
            extract_thinking,
            tool_name_map,
            cache_decision.cache_creation_input_tokens,
            cache_decision.cache_read_input_tokens,
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx =
        StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);
    ctx.cache_creation_input_tokens = cache_creation_input_tokens;
    ctx.cache_read_input_tokens = cache_read_input_tokens;

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(response, ctx, initial_events);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS))),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束
                            let final_events = ctx.generate_final_events();
                            // 记录请求完成日志（含 token 用量与耗时）
                            ctx.log_completion();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                        None => {
                            // 流结束，发送最终事件
                            let final_events = ctx.generate_final_events();
                            // 记录请求完成日志（含 token 用量与耗时）
                            ctx.log_completion();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> Response {
    let start_time = std::time::Instant::now();
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            // 详细错误（含上游 URL/IP/超时细节）只记 log，对外只返通用文案
            // 防内部地址/代理信息通过错误响应泄露给客户端
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "Failed to read upstream response body",
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_default();
                            buffer.push_str(&tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(buffer).unwrap_or_else(|e| {
                                        tracing::warn!(
                                            "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                            e,
                                            tool_use.tool_use_id
                                        );
                                        serde_json::json!({})
                                    })
                                };

                                let original_name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            // clamp 防上游异常返回 NaN/负/>100，避免 input_tokens 错乱
                            let pct = super::stream::clamp_context_percentage(
                                context_usage.context_usage_percentage,
                            );
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens = (pct * (window_size as f64) / 100.0)
                                .clamp(0.0, i32::MAX as f64)
                                as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if pct >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}% (clamped from {}), 计算 input_tokens: {}",
                                pct,
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Exception { exception_type, .. }
                            if exception_type == "ContentLengthExceededException" =>
                        {
                            stop_reason = "max_tokens".to_string();
                        }
                        Event::Unknown {
                            event_type,
                            payload_preview,
                        } => {
                            // 非流式路径同样记录未处理事件（如 reasoningContentEvent）
                            tracing::warn!(
                                "收到未处理的上游事件(非流式): event_type={} payload_preview={:?}",
                                event_type,
                                payload_preview
                            );
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块。
        // 块顺序与流式路径保持一致：<thinking> 之前的正文 → thinking → 之后的正文。
        let (before_text, thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(before) = before_text {
            if !before.trim().is_empty() {
                content.push(json!({
                    "type": "text",
                    "text": before
                }));
            }
        }

        if let Some(thinking_text) = thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens（"全量"），再扣去 cache_read + cache_creation 让
    // 客户端看到的三个字段（input_tokens / cache_creation_input_tokens / cache_read_input_tokens）
    // 互斥不重叠 —— 这是 Anthropic 官方协议语义，下游计费系统（如 sub2api）会按三者独立计价相加，
    // 若不扣 cache_creation 会被 sub2api 重复算一次 input_price，导致 MISS 时溢出 ~25%。
    //
    // 注：handler 入口 input_tokens 参数已扣过 cache_read + cache_creation（input_tokens_for_client），
    // 所以 unwrap_or 回退分支需要"反加"两者得到全量，再统一减得到非缓存 input。
    let total_input_tokens = context_input_tokens
        .unwrap_or(input_tokens + cache_read_input_tokens + cache_creation_input_tokens);
    let final_input_tokens =
        (total_input_tokens - cache_read_input_tokens - cache_creation_input_tokens).max(0);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": canonical_anthropic_model(model),
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": final_input_tokens,
            "cache_creation_input_tokens": cache_creation_input_tokens,
            "cache_read_input_tokens": cache_read_input_tokens,
            "output_tokens": output_tokens,
            "service_tier": "standard"
        }
    });

    // 记录请求完成日志（含 token 用量与耗时）
    tracing::info!(
        model = %model,
        input_tokens = final_input_tokens,
        cache_creation_input_tokens = cache_creation_input_tokens,
        cache_read_input_tokens = cache_read_input_tokens,
        output_tokens = output_tokens,
        stop_reason = %stop_reason,
        elapsed_ms = start_time.elapsed().as_millis() as u64,
        "请求处理完成（非流式）"
    );

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 注入自定义系统提示词 & 剥离限制
///
/// 两个独立动作：
/// 1. 若 `strip_system_restrictions` 为 true，先剥离客户端 system prompt 中的限制片段
/// 2. 若总开关 `enabled`，调 `build_injection_text` 拼接 preset + custom，按 `position` 插入
fn inject_system_prompt(payload: &mut MessagesRequest, shared: &SharedPromptConfig) {
    // 取一次快照，立即释放读锁
    let (injection, position, strip_restrictions) = {
        let cfg = shared.read();
        (
            cfg.build_injection_text(),
            cfg.position,
            cfg.strip_system_restrictions,
        )
    };

    // 1. 剥离限制
    if strip_restrictions {
        if let Some(ref mut system) = payload.system {
            for msg in system.iter_mut() {
                let stripped = super::prompt_filter::strip_restrictions(&msg.text);
                if stripped.len() != msg.text.len() {
                    tracing::info!(
                        "剥离系统提示词限制: {} → {} bytes",
                        msg.text.len(),
                        stripped.len()
                    );
                    msg.text = stripped;
                }
            }
        }
    }

    // 2. 注入
    let Some(text) = injection else {
        return;
    };
    let injected = SystemMessage {
        text,
        cache_control: None,
    };

    match &mut payload.system {
        Some(existing) => match position {
            SystemPromptPosition::Prepend => existing.insert(0, injected),
            SystemPromptPosition::Append => existing.push(injected),
        },
        None => {
            payload.system = Some(vec![injected]);
        }
    }
}

/// 模型名 / Thinking 配置规范化
///
/// 处理两类情况：
///
/// 1. **Opus 4.7 thinking 兼容性修正**（无论 model 名是否带 "thinking" 后缀）：
///    Opus 4.7 在 Kiro/Bedrock 上**不支持** `thinking.type = "enabled"`，
///    必须使用 `"adaptive"`（参考: AWS Bedrock Opus 4.7 文档）。
///    如果客户端传了 `enabled`，自动降级为 `adaptive` 并补一个默认 `effort`。
///
/// 2. **`*-thinking` 后缀 → 强制启用 thinking**（原行为）：
///    - Opus 4.6/4.7 → `adaptive`（带 `effort: high`）
///    - 其他模型 → `enabled`，budget_tokens=20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    let is_opus = model_lower.contains("opus");
    let is_opus_4_7 = is_opus && (model_lower.contains("4-7") || model_lower.contains("4.7"));
    let is_opus_4_6 = is_opus && (model_lower.contains("4-6") || model_lower.contains("4.6"));
    let is_opus_4_6_or_newer = is_opus_4_6 || is_opus_4_7;
    let has_thinking_suffix = model_lower.contains("thinking");

    // Case 1: Opus 4.7 强制 adaptive — 不论是否带 thinking 后缀
    if is_opus_4_7 {
        if let Some(ref mut t) = payload.thinking {
            if t.thinking_type == "enabled" {
                tracing::info!(
                    model = %payload.model,
                    "Opus 4.7 不支持 thinking.type=\"enabled\"，自动降级为 \"adaptive\""
                );
                t.thinking_type = "adaptive".to_string();
            }
            // 4.7 上游默认 display=omitted，强制设 summarized 让 Kiro 吐 thinking 文本
            if t.display.is_none() {
                t.display = Some("summarized".to_string());
            }
            // adaptive 需要 output_config.effort，缺省补 "high"
            if payload.output_config.is_none() {
                payload.output_config = Some(OutputConfig {
                    effort: "high".to_string(),
                });
            }
        }
    }

    // Case 2: model 名带 "*-thinking" 后缀 → 强制开启 thinking
    if !has_thinking_suffix {
        return;
    }

    let thinking_type = if is_opus_4_6_or_newer {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
        // adaptive 模式（4.6/4.7）默认 summarized 让 Kiro 吐 thinking 文本
        display: if thinking_type == "adaptive" {
            Some("summarized".to_string())
        } else {
            None
        },
    });

    if is_opus_4_6_or_newer {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    );

    Json(CountTokensResponse {
        input_tokens: saturating_to_i32(total_tokens.max(1)),
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 注入自定义系统提示词
    inject_system_prompt(&mut payload, &state.prompt_config);

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = saturating_to_i32(token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ));

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 估算输入 tokens（全量；也用于 prompt cache 的 85% 上限与 TTL 分桶）
    let input_tokens = saturating_to_i32(token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ));

    // ===== Prompt Cache 决定（多断点精确计费 + conversation_id 复用）=====
    let cache_decision = lookup_prompt_cache(&state.prompt_cache, &payload, input_tokens);

    // 转换请求（命中时强制 conversation_id）
    let conversion_result =
        match convert_request_with_options(&payload, cache_decision.forced_conversation_id.clone())
        {
            Ok(result) => result,
            Err(e) => {
                let (error_type, message) = match &e {
                    ConversionError::UnsupportedModel(model) => {
                        ("invalid_request_error", format!("模型不支持: {}", model))
                    }
                    ConversionError::EmptyMessages => {
                        ("invalid_request_error", "消息列表为空".to_string())
                    }
                };
                tracing::warn!("请求转换失败: {}", e);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(error_type, message)),
                )
                    .into_response();
            }
        };

    record_cache_outcome(
        &state.prompt_cache,
        &cache_decision,
        &conversion_result.conversation_state.conversation_id,
    );

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 客户端可见的 input_tokens 应该剔除 cache_read + cache_creation 两部分（Anthropic 协议三字段互斥）
    let input_tokens_for_client = (input_tokens
        - cache_decision.cache_read_input_tokens
        - cache_decision.cache_creation_input_tokens)
        .max(0);

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens_for_client,
            thinking_enabled,
            tool_name_map,
            cache_decision.cache_creation_input_tokens,
            cache_decision.cache_read_input_tokens,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens_for_client,
            extract_thinking,
            tool_name_map,
            cache_decision.cache_creation_input_tokens,
            cache_decision.cache_read_input_tokens,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(
        model,
        estimated_input_tokens,
        thinking_enabled,
        tool_name_map,
    );
    ctx.cache_creation_input_tokens = cache_creation_input_tokens;
    ctx.cache_read_input_tokens = cache_read_input_tokens;

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(response, ctx);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                // 记录请求完成日志（含 token 用量与耗时）
                                ctx.log_completion();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                // 记录请求完成日志（含 token 用量与耗时）
                                ctx.log_completion();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_models_map_to_anthropic_format() {
        use crate::kiro::provider::UpstreamModel;
        let mk = |id: &str, name: &str, max_out: Option<i32>| UpstreamModel {
            model_id: id.to_string(),
            model_name: name.to_string(),
            max_output_tokens: max_out,
        };
        let upstream = vec![
            mk("auto", "Auto", Some(64000)),
            mk("claude-opus-4.7", "Claude Opus 4.7", Some(128000)),
            mk("deepseek-3.2", "Deepseek v3.2", Some(64000)),
        ];
        let models = models_from_upstream(&upstream);
        // auto 被剔除；claude 有 thinking 变体；deepseek 无 thinking
        assert!(!models.iter().any(|m| m.id == "auto"));
        let opus = models.iter().find(|m| m.id == "claude-opus-4.7").unwrap();
        assert_eq!(opus.max_tokens, 128000, "用上游 maxOutputTokens 真值");
        assert_eq!(opus.owned_by, "anthropic");
        assert!(
            models.iter().any(|m| m.id == "claude-opus-4.7-thinking"),
            "claude 应有 thinking 变体"
        );
        let ds = models.iter().find(|m| m.id == "deepseek-3.2").unwrap();
        assert_eq!(ds.owned_by, "kiro");
        assert!(
            !models.iter().any(|m| m.id == "deepseek-3.2-thinking"),
            "非 claude 不应有 thinking 变体"
        );
    }

    #[test]
    fn upstream_empty_falls_back_to_static() {
        let models = models_from_upstream(&[]);
        assert_eq!(models.len(), static_models().len());
    }

    /// 构造最小可用的 MessagesRequest，便于 thinking 覆写逻辑测试
    fn make_req(model: &str, thinking: Option<Thinking>) -> MessagesRequest {
        // 借 serde_json 绕过 MessagesRequest 字段非 Default 的限制
        let mut payload: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": model,
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("构造 MessagesRequest 应成功");
        payload.thinking = thinking;
        payload
    }

    fn thinking(thinking_type: &str, display: Option<&str>) -> Thinking {
        Thinking {
            thinking_type: thinking_type.to_string(),
            budget_tokens: 5000,
            display: display.map(String::from),
        }
    }

    // === Case 1: Opus 4.7 强制 adaptive ===

    #[test]
    fn opus_4_7_enabled_downgrades_to_adaptive() {
        let mut req = make_req("claude-opus-4-7", Some(thinking("enabled", None)));
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().expect("应保留 thinking");
        assert_eq!(t.thinking_type, "adaptive", "enabled 应被降级");
        assert_eq!(t.display.as_deref(), Some("summarized"), "应补 display");
        let oc = req.output_config.as_ref().expect("应补 output_config");
        assert_eq!(oc.effort, "high");
    }

    #[test]
    fn opus_4_7_dot_form_also_downgrades() {
        // claude-opus-4.7（点形式）也应触发
        let mut req = make_req("claude-opus-4.7", Some(thinking("enabled", None)));
        override_thinking_from_model_name(&mut req);
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "adaptive");
    }

    #[test]
    fn opus_4_7_adaptive_keeps_existing_display() {
        let mut req = make_req(
            "claude-opus-4-7",
            Some(thinking("adaptive", Some("omitted"))),
        );
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(
            t.display.as_deref(),
            Some("omitted"),
            "已有 display 不应被覆盖"
        );
    }

    #[test]
    fn opus_4_7_without_thinking_field_is_noop() {
        let mut req = make_req("claude-opus-4-7", None);
        override_thinking_from_model_name(&mut req);
        assert!(req.thinking.is_none(), "无 thinking 字段应保持无");
        assert!(req.output_config.is_none(), "也不应主动注入 output_config");
    }

    // === Case 2: 模型名带 -thinking 后缀 ===

    #[test]
    fn opus_4_7_thinking_suffix_sets_adaptive() {
        let mut req = make_req("claude-opus-4-7-thinking", None);
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().expect("后缀应注入 thinking");
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.display.as_deref(), Some("summarized"));
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn opus_4_6_thinking_suffix_sets_adaptive() {
        let mut req = make_req("claude-opus-4-6-thinking", None);
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive", "4.6 也走 adaptive");
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn sonnet_thinking_suffix_sets_enabled() {
        let mut req = make_req("claude-sonnet-4-5-thinking", None);
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "enabled", "非 opus 4.6+ 应保留 enabled");
        assert_eq!(t.budget_tokens, 20000);
        assert!(req.output_config.is_none(), "enabled 不需要 output_config");
    }

    // === Anti-regression: 普通模型不应被改写 ===

    #[test]
    fn plain_model_without_suffix_is_noop() {
        let mut req = make_req("claude-sonnet-4-5", Some(thinking("enabled", None)));
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "enabled", "普通模型 enabled 不应被改");
        assert_eq!(t.display, None);
    }

    // === Thinking.display 反序列化校验 ===

    #[test]
    fn display_accepts_valid_values() {
        for v in ["summarized", "omitted"] {
            let t: Thinking = serde_json::from_value(serde_json::json!({
                "type": "adaptive",
                "display": v,
            }))
            .expect("有效值应解析成功");
            assert_eq!(t.display.as_deref(), Some(v));
        }
    }

    #[test]
    fn display_rejects_invalid_value_silently() {
        let t: Thinking = serde_json::from_value(serde_json::json!({
            "type": "adaptive",
            "display": "raw",
        }))
        .expect("无效值不应导致解析失败");
        assert_eq!(t.display, None, "脏值应被降级为 None");
    }

    #[test]
    fn effective_display_fallback() {
        let t = thinking("adaptive", None);
        assert_eq!(t.effective_display(), "summarized");
        let t = thinking("adaptive", Some("omitted"));
        assert_eq!(t.effective_display(), "omitted");
    }

    // === Bug 3: saturating_to_i32 防 u64 → i32 wrap ===

    #[test]
    fn saturating_to_i32_normal() {
        assert_eq!(saturating_to_i32(0), 0);
        assert_eq!(saturating_to_i32(100), 100);
        assert_eq!(saturating_to_i32(1_000_000), 1_000_000);
    }

    #[test]
    fn saturating_to_i32_max_boundary() {
        assert_eq!(saturating_to_i32(i32::MAX as u64), i32::MAX);
        // 边界 +1 应饱和
        assert_eq!(saturating_to_i32(i32::MAX as u64 + 1), i32::MAX);
    }

    #[test]
    fn saturating_to_i32_overflow_saturates() {
        // 之前的 `as i32` 会把 u64::MAX wrap 成 -1（i32 视图）
        // saturating 版本应饱和到 i32::MAX，永不为负
        assert_eq!(saturating_to_i32(u64::MAX), i32::MAX);
        assert!(saturating_to_i32(u64::MAX) >= 0, "结果不应为负");
        // 模拟大请求场景
        assert_eq!(saturating_to_i32(5_000_000_000), i32::MAX);
    }
}
