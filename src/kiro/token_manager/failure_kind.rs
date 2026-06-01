//! 上游瞬态错误分类
//!
//! 从 `token_manager/mod.rs` 抽出。这是 provider 与 token_manager 之间的公共契约：
//! provider 把上游 HTTP 响应分类成 [`TransientFailureKind`] 后上报，token_manager 据此
//! 选择 cooldown 时长。[`extract_suspicious_directory_key`] 供 directory 级风控分组使用。

use serde::Serialize;

/// 上游瞬态错误分类（用于 cooldown 时长选择 + 观测）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransientFailureKind {
    /// HTTP 429 Too Many Requests（限流）
    RateLimit,
    /// HTTP 408 Request Timeout
    Timeout,
    /// HTTP 5xx 上游服务错误
    UpstreamError,
    /// HTTP 402 + `OVERAGE_REQUEST_LIMIT_EXCEEDED`
    ///
    /// 用户已开启 overage 付费，但当前 hour/day 速率窗口已满。
    /// 与 [`Self::RateLimit`] 区别：cooldown 时长更长（窗口刷新粒度通常以
    /// 小时/天计），不应误判为永久 disable。
    OverageRequestLimit,
    /// 429 + "suspicious activity" directory 级别封禁
    ///
    /// Kiro 返回 "Due to suspicious activity, we are imposing temporary limits"
    /// 表示 directory 维度的风控触发（所有共享 directory_id 的凭据同时受限）。
    /// 使用比普通 429 更长的 cooldown（默认 300s），避免号池在短时间内反复被打爆。
    SuspiciousActivity,
}

impl TransientFailureKind {
    /// 由 HTTP 状态码推断分类
    pub fn from_status(status: u16) -> Self {
        match status {
            429 => Self::RateLimit,
            408 => Self::Timeout,
            _ => Self::UpstreamError, // 5xx 及兜底
        }
    }

    /// 从 429 响应体中检测是否为 "suspicious activity" directory 封禁
    pub fn classify_429(body: &str) -> Self {
        if body.contains("suspicious activity") || body.contains("imposing temporary limits") {
            Self::SuspiciousActivity
        } else {
            Self::RateLimit
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Timeout => "timeout",
            Self::UpstreamError => "upstream_error",
            Self::OverageRequestLimit => "overage_request_limit",
            Self::SuspiciousActivity => "suspicious_activity",
        }
    }
}

/// 从 Kiro suspicious activity 响应体中提取 directory key。
///
/// 上游常见格式类似：
/// `account (d-9067c98495.b428a438-...)`。冷却只需要 directory 前缀
/// `d-9067c98495`，不要把后面的账号实例 ID 纳入分组。
pub fn extract_suspicious_directory_key(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        if bytes[i] == b'd' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let start = i;
            i += 2;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                i += 1;
            }
            if i > start + 2 {
                return Some(body[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_suspicious_directory_key() {
        let body = "Due to suspicious activity, we are imposing temporary limits on account (d-9067c98495.b428a438-70d1-70cd-d36b-a7e66ef63e4d).";
        assert_eq!(
            extract_suspicious_directory_key(body).as_deref(),
            Some("d-9067c98495")
        );
        assert!(extract_suspicious_directory_key("plain rate limit").is_none());
    }
}
