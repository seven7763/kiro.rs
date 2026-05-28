//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

pub mod ide;

pub use ide::IdeEndpoint;

/// Kiro 端点
///
/// 同一个 `KiroProvider` 可持有多个 endpoint 实现，按凭据级字段切换。
pub trait KiroEndpoint: Send + Sync {
    /// 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的取值）
    fn name(&self) -> &'static str;

    /// API endpoint URL
    fn api_url(&self, ctx: &RequestContext<'_>) -> String;

    /// MCP endpoint URL
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String;

    /// ListAvailableModels endpoint URL（上游可用模型列表）
    fn models_url(&self, ctx: &RequestContext<'_>) -> String;

    /// 装饰 API 请求的端点特有 header
    ///
    /// Provider 已经设置好 URL、content-type、Connection 和 body；
    /// 实现负责追加 Authorization、host、user-agent 等端点相关头。
    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 装饰 MCP 请求的端点特有 header
    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 对已序列化的 API 请求体做端点特有加工（如注入 profileArn）
    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String;

    /// 对已序列化的 MCP 请求体做端点特有加工（默认不变）
    fn transform_mcp_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        body.to_string()
    }

    /// 判断响应体是否表示"月度配额用尽"（禁用凭据并转移）
    fn is_monthly_request_limit(&self, body: &str) -> bool {
        default_is_monthly_request_limit(body)
    }

    /// 判断响应体是否表示"开启 overage 后的短窗口速率上限"
    ///
    /// 与 [`Self::is_monthly_request_limit`] 区别：本方法**不**禁用凭据，
    /// 而是触发较长 cooldown 等待 hour/day 窗口刷新。
    fn is_overage_request_limit(&self, body: &str) -> bool {
        default_is_overage_request_limit(body)
    }

    /// 判断响应体是否表示"上游 bearer token 失效"（触发强制刷新）
    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        default_is_bearer_token_invalid(body)
    }
}

/// 装饰请求时可用的上下文
///
/// 包含单次调用已确定的所有运行时信息。引用形式避免无谓 clone。
pub struct RequestContext<'a> {
    /// 当前凭据
    pub credentials: &'a KiroCredentials,
    /// 有效的 access token（API Key 凭据下即 kiroApiKey）
    pub token: &'a str,
    /// 当前凭据对应的 machineId
    pub machine_id: &'a str,
    /// 全局配置
    pub config: &'a Config,
}

/// 默认的 MONTHLY_REQUEST_COUNT 判断逻辑
///
/// 仅匹配账号当月配额已永久耗尽的场景（需禁用并故障转移）。
/// **不**包含 `OVERAGE_REQUEST_LIMIT_EXCEEDED` —— 后者是开启了 overage
/// 付费后的短窗口（小时/天）速率限制，应通过 cooldown 等待恢复，
/// 详见 [`default_is_overage_request_limit`]。
///
/// 同时识别顶层 `reason` 字段和嵌套 `error.reason` 字段。
pub fn default_is_monthly_request_limit(body: &str) -> bool {
    if body.contains("MONTHLY_REQUEST_COUNT") {
        return true;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };

    if value
        .get("reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "MONTHLY_REQUEST_COUNT")
    {
        return true;
    }

    value
        .pointer("/error/reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "MONTHLY_REQUEST_COUNT")
}

/// 判断响应体是否表示"开启 overage 后的短窗口速率上限"
///
/// 触发场景：用户已开启 overage 付费，但当前 hour/day 窗口内的请求速率
/// 达到 Kiro 上游设定的上限。返回 `402 Payment Required` +
/// `reason=OVERAGE_REQUEST_LIMIT_EXCEEDED`。
///
/// 处理策略：进入较长 cooldown（默认 5 分钟）等待窗口刷新，**不禁用**凭据。
pub fn default_is_overage_request_limit(body: &str) -> bool {
    if body.contains("OVERAGE_REQUEST_LIMIT_EXCEEDED") {
        return true;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };

    if value
        .get("reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "OVERAGE_REQUEST_LIMIT_EXCEEDED")
    {
        return true;
    }

    value
        .pointer("/error/reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "OVERAGE_REQUEST_LIMIT_EXCEEDED")
}

/// 默认的 bearer token 失效判断逻辑
pub fn default_is_bearer_token_invalid(body: &str) -> bool {
    body.contains("The bearer token included in the request is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_monthly_request_limit_detects_reason() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_nested_reason() {
        let body = r#"{"error":{"reason":"MONTHLY_REQUEST_COUNT"}}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_false() {
        let body = r#"{"message":"nope","reason":"DAILY_REQUEST_COUNT"}"#;
        assert!(!default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_monthly_request_limit_does_not_match_overage() {
        // 关键：MONTHLY 函数不应误判 OVERAGE（语义不同）
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        assert!(
            !default_is_monthly_request_limit(body),
            "OVERAGE 不应被识别为 MONTHLY 永久耗尽"
        );
    }

    #[test]
    fn test_overage_request_limit_detects_reason() {
        // 真实生产观察到的响应体（402 Payment Required）
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        assert!(default_is_overage_request_limit(body));
    }

    #[test]
    fn test_overage_request_limit_nested_reason() {
        let body = r#"{"error":{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}}"#;
        assert!(default_is_overage_request_limit(body));
    }

    #[test]
    fn test_overage_request_limit_substring() {
        // 即便不是合法 JSON 也应匹配（裸字符串兜底）
        let body = "garbage prefix OVERAGE_REQUEST_LIMIT_EXCEEDED garbage suffix";
        assert!(default_is_overage_request_limit(body));
    }

    #[test]
    fn test_overage_does_not_match_monthly_body() {
        // OVERAGE 函数不应误判 MONTHLY
        let body = r#"{"reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(!default_is_overage_request_limit(body));
    }

    #[test]
    fn test_default_bearer_token_invalid() {
        assert!(default_is_bearer_token_invalid(
            "The bearer token included in the request is invalid"
        ));
        assert!(!default_is_bearer_token_invalid("unrelated error"));
    }
}
