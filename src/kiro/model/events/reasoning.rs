//! 推理内容事件
//!
//! 处理 reasoningContentEvent 类型的事件（4.8+ 模型原生 thinking 流）

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 推理内容事件
///
/// 4.8+ 模型通过 `reasoningContentEvent` 发送 thinking 内容，
/// 而非嵌入在 `assistantResponseEvent` 的 `<thinking>` 标签中。
///
/// 有两种 payload 形式：
/// - `{"text": "..."}` — thinking 内容增量
/// - `{"signature": "..."}` — thinking 块结束签名
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningContentEvent {
    /// thinking 内容增量（与 signature 互斥）
    #[serde(default)]
    pub text: Option<String>,

    /// thinking 块结束签名（与 text 互斥）
    #[serde(default)]
    pub signature: Option<String>,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}
