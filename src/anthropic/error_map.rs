//! 上游错误 → Anthropic 错误协议映射
//!
//! 从 handlers.rs 抽出：把 `KiroProvider` 的 [`anyhow::Error`] 翻译成符合 Anthropic
//! 错误协议的 HTTP 响应，并按错误类型附带合适的 `Retry-After`，控制 Claude Code
//! 客户端的 retry 行为。

use anyhow::Error;
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};

use super::types::ErrorResponse;

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
pub(crate) fn map_provider_error(err: Error) -> Response {
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
pub(crate) fn build_retryable_error(
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
