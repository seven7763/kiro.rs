//! 计费事件 (meteringEvent)
//!
//! Kiro 在 `generateAssistantResponse` 流末端会下发一个 `meteringEvent`，
//! 携带本次请求消耗的**积分（credit）计量**，字段（camelCase）：
//!
//! ```json
//! { "unit": "credit", "unitPlural": "credits", "usage": 0.0125 }
//! ```
//!
//! 注意区分 [`super::TokenUsageEvent`]：`meteringEvent` 只带 credit 计量，**不带 token 真值**；
//! token 精确计量在 `tokenUsageEvent`。本事件历史上被丢成 `Metering(())`。
//!
//! 救回来的用途（**当前仅观测，不改计费行为**）：credit 是判断「Kiro 上游有没有真正应用
//! prefix 缓存折扣」的唯一真值信号——命中时 credit 明显低于全价。运营开了 perceived 假缓存
//! always-high 上报时，靠它对账「真实命中率」与「自己补贴的差额」。

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计量单位（如 "credit"）
    #[serde(default)]
    pub unit: Option<String>,
    /// 计量单位复数形式（如 "credits"）
    #[serde(default)]
    pub unit_plural: Option<String>,
    /// 本次请求消耗的数量（credit）
    #[serde(default)]
    pub usage: f64,
    /// 容错：Kiro 后续可能加新字段，全部塞这里不丢，便于探针观测
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        // 偶发空 payload 时回退到 default，避免整条流崩
        match frame.payload_as_json::<Self>() {
            Ok(ev) => Ok(ev),
            Err(_) => Ok(Self::default()),
        }
    }
}

impl MeteringEvent {
    /// 是否带了有效 credit（用于判断要不要打观测日志）
    pub fn has_usage(&self) -> bool {
        self.usage > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_from_kiro_camelcase() {
        let json = r#"{"unit":"credit","unitPlural":"credits","usage":0.0125}"#;
        let ev: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.usage, 0.0125);
        assert_eq!(ev.unit.as_deref(), Some("credit"));
        assert_eq!(ev.unit_plural.as_deref(), Some("credits"));
        assert!(ev.has_usage());
    }

    #[test]
    fn extra_fields_captured_not_dropped() {
        let json = r#"{"unit":"credit","usage":1.5,"futureField":"abc","another":42}"#;
        let ev: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.usage, 1.5);
        assert_eq!(ev.extra.len(), 2);
        assert!(ev.extra.contains_key("futureField"));
    }

    #[test]
    fn empty_usage_not_counted() {
        assert!(!MeteringEvent::default().has_usage());
    }
}
