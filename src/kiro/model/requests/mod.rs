//! 请求类型模块
//!
//! 包含 Kiro API 请求相关的类型定义

pub mod conversation;
pub mod kiro;
pub mod tool;

// 协议契约 facade：上层 anthropic 转换器通过此处扁平导入，避免穿透到子模块内部。
pub use conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
pub use kiro::KiroRequest;
pub use tool::{InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry};
