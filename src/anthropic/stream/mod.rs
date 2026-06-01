//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理
//!
//! 按职责拆分为子模块：
//! - `signature`：伪 signature / message ID 生成
//! - `thinking`：thinking 标签检测与提取
//! - `sse_state`：SSE 事件与块状态管理
//! - `context`：StreamContext 流式核心（热路径）
//! - `buffered`：BufferedStreamContext（/cc 端点）
//! - `tokens`：token 估算与截断
//!
//! 单元测试随实现下沉到各子模块的 `#[cfg(test)] mod tests` 中。

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use serde_json::json;

use crate::kiro::metrics::RecordHandle;
use crate::kiro::model::events::Event;

mod buffered;
mod context;
mod signature;
mod sse_state;
mod thinking;
mod tokens;

pub(crate) use buffered::BufferedStreamContext;
pub(crate) use context::StreamContext;
pub(crate) use signature::{generate_fake_signature_for_model, generate_message_id};
pub(crate) use sse_state::{SseEvent, SseStateManager};
pub(crate) use thinking::extract_thinking_from_complete_text;
pub(crate) use tokens::{clamp_context_percentage, estimate_tokens, truncate_text_to_token_budget};
