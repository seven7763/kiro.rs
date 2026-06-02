//! 缓冲式流上下文 BufferedStreamContext（/cc 端点，等 contextUsageEvent 后再发 message_start）

use super::*;

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 估算的 input_tokens（用于回退）
    estimated_input_tokens: i32,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
    /// Prompt cache：首次创建（未命中）的 prefix tokens
    /// 改这两个字段会自动同步到 inner StreamContext，调用方可直接 `ctx.cache_creation_input_tokens = X`
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let inner = StreamContext::new_with_thinking(
            model,
            estimated_input_tokens,
            thinking_enabled,
            tool_name_map,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            estimated_input_tokens,
            initial_events_generated: false,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 同步 cache_*_input_tokens 到 inner StreamContext（在事件生成前）
    /// 2. 生成最终事件（message_delta, message_stop）
    /// 3. 用正确的 input_tokens 更正 message_start 事件
    /// 4. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 把外部设置的 cache tokens 同步进 inner（必须在生成初始/最终事件之前）
        self.inner.cache_creation_input_tokens = self.cache_creation_input_tokens;
        self.inner.cache_read_input_tokens = self.cache_read_input_tokens;

        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 生成最终事件
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // message_start 的 usage 更正：输入侧三字段用本地缓存账(estimated_input_tokens 是
        // 注入前纯客户端口径 + 外部 cache 字段)，与 inner 的 message_delta 严格同口径。
        // 不取上游 real_usage 输入侧——原因见 stream::context::resolved_usage 文档(上游 cacheRead
        // 恒 0 会顶掉 perceived cache_read；上游 input 含 Kiro agent prompt 会虚高)。
        let final_input_tokens = self.estimated_input_tokens.max(0);
        let start_cache_creation = self.cache_creation_input_tokens;
        let start_cache_read = self.cache_read_input_tokens;

        // 更正 message_start 事件中的 usage 字段
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(final_input_tokens);
                        usage["cache_creation_input_tokens"] =
                            serde_json::json!(start_cache_creation);
                        usage["cache_read_input_tokens"] = serde_json::json!(start_cache_read);
                        usage["cache_creation"] =
                            crate::anthropic::cache_accounting::cache_creation_breakdown(
                                start_cache_creation,
                            );
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }

    /// 记录请求完成日志（缓冲流式，转发到 inner StreamContext）
    pub fn log_completion(&self) {
        self.inner.log_completion();
    }

    /// 设置 metrics 记录句柄（转发到 inner，log_completion 时回填 token）
    pub fn set_record(&mut self, record: RecordHandle) {
        self.inner.record = Some(record);
    }

    /// 设置客户端输出预算（转发到 inner StreamContext）
    pub fn set_max_output_tokens(&mut self, max_tokens: i32) {
        self.inner.set_max_output_tokens(max_tokens);
    }

    /// 输出预算是否已耗尽（转发自 inner）。缓冲流据此提前收尾并断开上游连接。
    pub fn budget_exceeded(&self) -> bool {
        self.inner.budget_exceeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_stream_updates_nested_cache_creation_usage() {
        let mut ctx = BufferedStreamContext::new("claude-sonnet-4-5", 42, false, HashMap::new());
        ctx.cache_creation_input_tokens = 123;
        ctx.cache_read_input_tokens = 456;

        let events = ctx.finish_and_get_all_events();
        let start = events
            .iter()
            .find(|e| e.event == "message_start")
            .expect("应有 message_start");
        let usage = &start.data["message"]["usage"];

        assert_eq!(usage["input_tokens"], 42);
        assert_eq!(usage["cache_creation_input_tokens"], 123);
        assert_eq!(usage["cache_read_input_tokens"], 456);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 123);
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 0);
    }
}
