//! Token 使用事件 (tokenUsageEvent)
//!
//! Kiro 后端在 `generateAssistantResponse` 流末端会下发 `tokenUsageEvent`，
//! 字段对齐 Amazon Q 的 TokenUsage：
//!
//! ```json
//! {
//!   "uncachedInputTokens": 12345,
//!   "outputTokens": 678,
//!   "totalTokens": 13023,
//!   "cacheReadInputTokens": 0,
//!   "cacheWriteInputTokens": 0
//! }
//! ```
//!
//! 这是 Kiro **精确**的 token 计量（含 thinking 输出 + 缓存读/写明细）。
//! 此前我们不认这个事件、被当 `Unknown` 丢弃，只能用本地字符数启发式估算，
//! 导致 NewAPI 永远按缓存/输入/输出算不准——这是计费偏差的根因。
//!
//! 字段语义（推断，需一次生产实测核对）：
//! - `uncachedInputTokens`：本轮**未命中缓存**的输入（按 Amazon Q 口径通常**含**首次写入缓存的部分）
//! - `cacheReadInputTokens`：命中缓存读取（Anthropic 计费 0.1×）
//! - `cacheWriteInputTokens`：写入缓存（Anthropic 计费 1.25×）
//! - `outputTokens`：输出（**含 thinking**）
//! - `totalTokens`：总计 = uncached + cacheRead + output

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Token 使用事件
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageEvent {
    /// 未命中缓存的输入 token（按 Amazon Q 口径通常含首次写入缓存的部分）
    #[serde(default)]
    pub uncached_input_tokens: i64,
    /// 输出 token（精确，**含 thinking**）
    #[serde(default)]
    pub output_tokens: i64,
    /// 总 token（uncached + cacheRead + output）
    #[serde(default)]
    pub total_tokens: i64,
    /// 命中缓存读取的 token（部分模型不报，None = 未上报）
    #[serde(default)]
    pub cache_read_input_tokens: Option<i64>,
    /// 写入缓存的 token（部分模型不报，None = 未上报）
    #[serde(default)]
    pub cache_write_input_tokens: Option<i64>,
    /// 容错：Kiro 后续可能加新字段，全部塞这里不丢，便于探针观测
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl EventPayload for TokenUsageEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        // 偶发空 payload 时回退到 default，避免整条流崩
        match frame.payload_as_json::<Self>() {
            Ok(ev) => Ok(ev),
            Err(_) => Ok(Self::default()),
        }
    }
}

/// 把上游真值映射为 Anthropic `/v1/messages` 的**三段不重叠**计费口径。
///
/// Anthropic usage 语义：`input_tokens`（fresh，1×）、`cache_read_input_tokens`（0.1×）、
/// `cache_creation_input_tokens`（1.25×）三者**互不重叠**，真实总输入 = 三者之和。
///
/// 故：
/// - `input_tokens = uncached − cacheWrite`（剥掉写缓存部分，得纯 fresh）
/// - `cache_creation_input_tokens = cacheWrite`
/// - `cache_read_input_tokens = cacheRead`
///
/// guard：极少数上游可能把 `uncached` 报成 fresh-only（不含 write），此时
/// `cacheWrite > uncached`，直接减会得负/低估，故不减、原样用 uncached。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingSplit {
    /// fresh 输入（1× 计费）
    pub input_tokens: i32,
    /// 写缓存（1.25× 计费）
    pub cache_creation_input_tokens: i32,
    /// 读缓存（0.1× 计费）
    pub cache_read_input_tokens: i32,
    /// 输出（含 thinking）
    pub output_tokens: i32,
}

/// i64 → i32 饱和转换并 clamp 到非负（防上游异常大数 wrap 成负）
fn clamp_i32(v: i64) -> i32 {
    v.clamp(0, i32::MAX as i64) as i32
}

impl TokenUsageEvent {
    /// 派生 Anthropic 三段不重叠计费口径。见 [`BillingSplit`] 文档。
    pub fn billing_split(&self) -> BillingSplit {
        let uncached = self.uncached_input_tokens.max(0);
        let cache_read = self.cache_read_input_tokens.unwrap_or(0).max(0);
        let cache_write = self.cache_write_input_tokens.unwrap_or(0).max(0);
        let output = self.output_tokens.max(0);

        // fresh = uncached − cacheWrite（guard：write 超过 uncached 时不减）
        let fresh_input = if cache_write <= uncached {
            uncached - cache_write
        } else {
            uncached
        };

        BillingSplit {
            input_tokens: clamp_i32(fresh_input),
            cache_creation_input_tokens: clamp_i32(cache_write),
            cache_read_input_tokens: clamp_i32(cache_read),
            output_tokens: clamp_i32(output),
        }
    }

    /// 上游是否带了任何有效的计量真值（用于判定是否覆盖估算）
    pub fn has_real_usage(&self) -> bool {
        self.uncached_input_tokens > 0
            || self.output_tokens > 0
            || self.total_tokens > 0
            || self.cache_read_input_tokens.unwrap_or(0) > 0
            || self.cache_write_input_tokens.unwrap_or(0) > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_from_kiro_camelcase() {
        let json = r#"{"uncachedInputTokens":12345,"outputTokens":678,"totalTokens":13023,"cacheReadInputTokens":100,"cacheWriteInputTokens":50}"#;
        let ev: TokenUsageEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.uncached_input_tokens, 12345);
        assert_eq!(ev.output_tokens, 678);
        assert_eq!(ev.total_tokens, 13023);
        assert_eq!(ev.cache_read_input_tokens, Some(100));
        assert_eq!(ev.cache_write_input_tokens, Some(50));
    }

    #[test]
    fn deserialize_missing_cache_fields_defaults_none() {
        let json = r#"{"uncachedInputTokens":100,"outputTokens":20,"totalTokens":120}"#;
        let ev: TokenUsageEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.cache_read_input_tokens, None);
        assert_eq!(ev.cache_write_input_tokens, None);
        assert!(ev.has_real_usage());
    }

    #[test]
    fn deserialize_extra_fields_captured_not_dropped() {
        let json = r#"{"uncachedInputTokens":1,"outputTokens":2,"totalTokens":3,"futureField":"x","another":42}"#;
        let ev: TokenUsageEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.extra.len(), 2);
        assert!(ev.extra.contains_key("futureField"));
        assert!(ev.extra.contains_key("another"));
    }

    #[test]
    fn billing_split_disjoint_buckets_subtracts_write_from_uncached() {
        // uncached 含 write：fresh = uncached − write，三段不重叠
        let ev = TokenUsageEvent {
            uncached_input_tokens: 1000,
            output_tokens: 200,
            total_tokens: 1500,
            cache_read_input_tokens: Some(300),
            cache_write_input_tokens: Some(400),
            extra: Default::default(),
        };
        let s = ev.billing_split();
        assert_eq!(s.input_tokens, 600, "fresh = 1000 − 400");
        assert_eq!(s.cache_creation_input_tokens, 400);
        assert_eq!(s.cache_read_input_tokens, 300);
        assert_eq!(s.output_tokens, 200);
        // 三段输入和 = uncached + cacheRead（不重复计 write）
        assert_eq!(
            s.input_tokens + s.cache_creation_input_tokens + s.cache_read_input_tokens,
            1000 + 300
        );
    }

    #[test]
    fn billing_split_guard_when_write_exceeds_uncached() {
        // 异常：write > uncached → 不减，原样用 uncached 作 fresh
        let ev = TokenUsageEvent {
            uncached_input_tokens: 100,
            output_tokens: 10,
            total_tokens: 110,
            cache_read_input_tokens: None,
            cache_write_input_tokens: Some(500),
            extra: Default::default(),
        };
        let s = ev.billing_split();
        assert_eq!(s.input_tokens, 100);
        assert_eq!(s.cache_creation_input_tokens, 500);
    }

    #[test]
    fn billing_split_clamps_negative_and_huge() {
        let ev = TokenUsageEvent {
            uncached_input_tokens: -5,
            output_tokens: i64::MAX,
            total_tokens: 0,
            cache_read_input_tokens: Some(-10),
            cache_write_input_tokens: None,
            extra: Default::default(),
        };
        let s = ev.billing_split();
        assert_eq!(s.input_tokens, 0);
        assert_eq!(s.cache_read_input_tokens, 0);
        assert_eq!(s.output_tokens, i32::MAX);
    }

    #[test]
    fn empty_usage_not_real() {
        let ev = TokenUsageEvent::default();
        assert!(!ev.has_real_usage());
    }
}
