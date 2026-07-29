//! 流式上下文 StreamContext（Kiro event → Anthropic SSE，热路径核心）

use super::thinking::{
    find_char_boundary, find_real_thinking_end_tag, find_real_thinking_end_tag_at_buffer_end,
    find_real_thinking_start_tag,
};
use super::*;
use crate::anthropic::converter::get_context_window_size;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// Prompt cache：首次创建（未命中）的 prefix tokens
    pub cache_creation_input_tokens: i32,
    /// Prompt cache：命中时复用的 prefix tokens
    pub cache_read_input_tokens: i32,
    /// 上游 tokenUsageEvent 派生的精确计量。**仅用于覆盖 `output_tokens`**(含 thinking,
    /// 本地字符估算漏算)。输入侧三字段仍走本地缓存账,原因见 `resolved_usage` 文档。
    /// `None` 表示上游未发该事件(旧模型/异常)，output 回退本地估算。
    pub real_usage: Option<crate::kiro::model::events::BillingSplit>,
    /// 上游 meteringEvent 的 credit 计量(纯观测，不改计费)。判断 Kiro 是否真正应用
    /// 缓存折扣的唯一真值信号；log_completion 时与对外上报口径一起打日志，供对账真实命中率。
    pub metering_credit: Option<f64>,
    /// 请求开始时间（用于完成日志计算耗时）
    pub start_time: Instant,
    /// metrics 记录句柄；log_completion 时凭此把最终 token 回填到 metrics
    pub record: Option<RecordHandle>,
    /// 客户端请求的输出预算（来自 Anthropic 请求的 max_tokens）。
    ///
    /// Kiro 上游协议无任何限长入参（conversationState/userInputMessage 里没有
    /// max_tokens/maxOutputTokens 字段），上游按 model_id 固定上限自由产出。
    /// 这会让输出超过客户端侧闸门（如 Claude Code 默认 64000）导致客户端 abort。
    /// 故在中转层用此预算累计 output_tokens，到顶主动截断 + stop_reason=max_tokens
    /// 并断开上游流（参考 kirocc）。`None` 或 `<=0` 表示不限制。
    /// thinking 与正文共享同一预算（与客户端 max_tokens 口径一致）。
    pub max_output_tokens: Option<i32>,
    /// 输出预算已耗尽标志。置位后 unfold 循环停止读上游、走 finish 收尾，
    /// drop reqwest response 即关闭上游连接，避免为客户端收不到的内容继续付费。
    pub budget_exceeded: bool,
}

impl StreamContext {
    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: generate_message_id(),
            input_tokens,
            context_input_tokens: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            real_usage: None,
            metering_credit: None,
            start_time: Instant::now(),
            record: None,
            max_output_tokens: None,
            budget_exceeded: false,
        }
    }

    /// 设置客户端输出预算（来自 Anthropic 请求 max_tokens）。
    /// `<=0` 视为不限制。
    pub fn set_max_output_tokens(&mut self, max_tokens: i32) {
        self.max_output_tokens = if max_tokens > 0 {
            Some(max_tokens)
        } else {
            None
        };
    }

    /// 检查输出是否已达预算。达到则置 `budget_exceeded` + stop_reason=max_tokens，
    /// 返回当前剩余可发的 output token 配额（用于截断当前 delta）。
    ///
    /// 返回 `None` 表示未设预算（不限制）。返回 `Some(remaining)`：
    /// - `remaining > 0`：还能发 remaining 个 token，发完即到顶
    /// - `remaining <= 0`：已超预算，应丢弃当前 delta
    fn output_budget_remaining(&self) -> Option<i32> {
        self.max_output_tokens
            .map(|budget| budget - self.output_tokens)
    }

    /// 在输出预算约束下处理一段文本增量（thinking 与正文共用此逻辑）。
    ///
    /// - 无预算：原样累计 + 返回原文
    /// - 预算够：累计 + 返回原文
    /// - 预算不够：按 token 截断到剩余配额，置 `budget_exceeded` + stop_reason=max_tokens，
    ///   返回截断后的文本（可能为空，表示这段全部超预算应丢弃）
    ///
    /// 参考 kirocc `applyMaxTokensBudget`：到顶即停，避免输出超过客户端闸门。
    fn apply_output_budget(&mut self, text: &str) -> String {
        let Some(remaining) = self.output_budget_remaining() else {
            // 无预算限制
            self.output_tokens += estimate_tokens(text);
            return text.to_string();
        };

        if remaining <= 0 {
            // 已耗尽预算，丢弃这段
            self.budget_exceeded = true;
            self.state_manager.set_stop_reason("max_tokens");
            return String::new();
        }

        let text_tokens = estimate_tokens(text);
        if text_tokens <= remaining {
            // 预算够，整段放行
            self.output_tokens += text_tokens;
            return text.to_string();
        }

        // 预算不够：按 token 截断到剩余配额
        let truncated = truncate_text_to_token_budget(text, remaining);
        self.output_tokens += estimate_tokens(&truncated);
        self.budget_exceeded = true;
        self.state_manager.set_stop_reason("max_tokens");
        truncated
    }

    /// 解析客户端最终可见的 usage 口径。
    /// 返回 `(input_tokens, output_tokens, cache_creation, cache_read)`，全部 clamp ≥0。
    /// `generate_final_events`（对外 SSE）与 `log_completion`（metrics 回填）共用，保证口径一致。
    ///
    /// **口径来源(关键)**：输入侧三字段(input / cache_creation / cache_read)**始终**用本地
    /// 缓存账——perceived 假缓存或真断点账才是对外计费的权威口径。上游 `tokenUsageEvent`
    /// 只接管 `output_tokens`(含 thinking，本地字符估算漏算)。
    ///
    /// 为什么不让上游真值覆盖输入侧:
    /// 1. Kiro 多数模型 `cacheReadInputTokens` 恒 0/None → 直接采用会把 perceived 算出的
    ///    cache_read 顶成 0，NewAPI 只看到 cache_creation、看不到读取(实测症状)。
    /// 2. 上游 `uncached_input_tokens` 含 Kiro 自带 agent prompt(~6500)+ 注入 preset，
    ///    直接当 input 会让客户端发 1 个字也显示几千 token、被检测判为用量异常。
    fn resolved_usage(&self) -> (i32, i32, i32, i32) {
        let output_tokens = match self.real_usage {
            // 上游真值含 thinking，优先；为 0 时不可信，回退本地估算
            Some(split) if split.output_tokens > 0 => split.output_tokens,
            _ => self.output_tokens,
        };
        (
            self.input_tokens.max(0),
            output_tokens.max(0),
            self.cache_creation_input_tokens.max(0),
            self.cache_read_input_tokens.max(0),
        )
    }

    /// output_tokens 是否取自上游真值（用于日志区分）。
    fn output_from_upstream(&self) -> bool {
        matches!(self.real_usage, Some(split) if split.output_tokens > 0)
    }

    /// 记录请求完成日志（流式）
    ///
    /// 包含 model、input/output tokens、cache 命中、stop_reason、耗时
    pub fn log_completion(&self) {
        // 与 generate_final_events 同口径：真值优先、估算兜底
        let (final_input_tokens, final_output_tokens, cache_creation, cache_read) =
            self.resolved_usage();
        let elapsed_ms = self.start_time.elapsed().as_millis() as u64;
        tracing::info!(
            model = %self.model,
            input_tokens = final_input_tokens,
            cache_creation_input_tokens = cache_creation,
            cache_read_input_tokens = cache_read,
            output_tokens = final_output_tokens,
            output_source = if self.output_from_upstream() { "upstream" } else { "estimate" },
            upstream_credit = self.metering_credit,
            stop_reason = %self.state_manager.get_stop_reason(),
            elapsed_ms = elapsed_ms,
            "请求处理完成（流式）"
        );

        // 把最终 token 回填到 metrics（与对外 usage 口径一致）
        if let Some(record) = &self.record {
            record.update_tokens(
                Some(final_input_tokens as u32),
                Some(final_output_tokens as u32),
                Some(cache_read as u32),
            );
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        // 字段对齐官方 API（jp.pincc.ai 实测）：含 stop_details、usage.cache_creation 嵌套、
        // inference_geo，否则结构完整性校验会因字段缺失扣分。
        json!({
            "type": "message_start",
            "message": {
                "model": crate::anthropic::converter::canonical_anthropic_model(&self.model),
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "stop_details": null,
                "usage": {
                    "input_tokens": self.input_tokens,
                    "cache_creation_input_tokens": self.cache_creation_input_tokens,
                    "cache_read_input_tokens": self.cache_read_input_tokens,
                    "cache_creation": crate::anthropic::cache_accounting::cache_creation_breakdown(
                        self.cache_creation_input_tokens,
                    ),
                    "output_tokens": 1,
                    "service_tier": "standard",
                    "inference_geo": "not_available"
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                // clamp 防上游异常返回 NaN/负/>100，避免 input_tokens 错乱
                let pct = clamp_context_percentage(context_usage.context_usage_percentage);
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (pct * (window_size as f64) / 100.0).clamp(0.0, i32::MAX as f64) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if pct >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                // 探针：token 双计审计（cctest 实测三字段和=真实上下文 2.7 倍）。
                // 打出原始 pct / window / 反推值 / 本地扣缓存后估算，一次实测即可区分：
                // - 若 raw_pct≈3% 而真实上下文≈1.1%×window → 上游 pct 语义/口径问题
                // - 若 reasoning_window 与上游真实窗口不符 → window 常数错
                // 只读，不改计量行为。
                tracing::warn!(
                    target: "kiro::probe::context_usage",
                    model = %self.model,
                    raw_pct = context_usage.context_usage_percentage,
                    clamped_pct = pct,
                    window_size = window_size,
                    derived_context_input = actual_input_tokens,
                    local_input_after_cache = self.input_tokens,
                    cache_creation = self.cache_creation_input_tokens,
                    cache_read = self.cache_read_input_tokens,
                    "contextUsageEvent 探针（token 双计根因定位）"
                );
                Vec::new()
            }
            Event::TokenUsage(token_usage) => {
                // 上游精确计量：派生 Anthropic 三段不重叠计费口径，覆盖本地估算。
                let split = token_usage.billing_split();
                // 探针：原始 payload 真值 vs 派生 split vs 本地估算，一次实测即可核对口径
                // （尤其 uncached 是否含 cacheWrite —— 决定 fresh input 的减法是否正确）。
                tracing::info!(
                    target: "kiro::probe::token_usage",
                    model = %self.model,
                    raw_uncached = token_usage.uncached_input_tokens,
                    raw_output = token_usage.output_tokens,
                    raw_total = token_usage.total_tokens,
                    raw_cache_read = token_usage.cache_read_input_tokens.unwrap_or(0),
                    raw_cache_write = token_usage.cache_write_input_tokens.unwrap_or(0),
                    extra = ?token_usage.extra,
                    derived_input = split.input_tokens,
                    derived_cache_creation = split.cache_creation_input_tokens,
                    derived_cache_read = split.cache_read_input_tokens,
                    derived_output = split.output_tokens,
                    local_est_input = self.input_tokens,
                    local_est_output = self.output_tokens,
                    "tokenUsageEvent 精确计量（覆盖本地估算 → 修正 NewAPI 计费）"
                );
                if token_usage.has_real_usage() {
                    self.real_usage = Some(split);
                }
                Vec::new()
            }
            Event::Metering(metering) => {
                // 纯观测：记录 Kiro 真实 credit，log_completion 时与对外上报口径对账。
                // 不改任何计费行为（always-high 上报仍按本地缓存账）。
                if metering.has_usage() {
                    self.metering_credit = Some(metering.usage);
                }
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                }
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                Vec::new()
            }
            Event::Unknown {
                event_type,
                payload_preview,
            } => {
                // 上游出现我们尚未处理的事件类型。重点关注 reasoningContentEvent：
                // 若上游改用原生 reasoning 事件发 thinking，纯文本协议会漏接，需扩展解析。
                tracing::warn!(
                    "收到未处理的上游事件: event_type={} payload_preview={:?}",
                    event_type,
                    payload_preview
                );
                Vec::new()
            }
        }
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        if content.is_empty() {
            return Vec::new();
        }

        // 应用输出预算：累计 output_tokens，超预算则按 token 截断当前 delta，
        // 并置 budget_exceeded + stop_reason=max_tokens（unfold 循环据此断上游流）。
        // thinking 文本协议下 content 含 <thinking> 标签，一并计入预算（与客户端口径一致）。
        let content = self.apply_output_budget(content);
        if content.is_empty() {
            return Vec::new();
        }
        let content = content.as_str();

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            return self.process_content_with_thinking(content);
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        self.create_text_delta_events(content)
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": "",
                                "signature": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    // 发送 signature_delta 收尾，再发送 content_block_stop（协议要求）
                    if let Some(thinking_index) = self.thinking_block_index {
                        // thinking 块以 signature_delta 收尾
                        events.push(self.create_signature_delta_event(thinking_index, ""));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 剥离 `</thinking>\n\n`（find_real_thinking_end_tag 已确认 \n\n 存在）
                    self.thinking_buffer =
                        self.thinking_buffer[end_pos + "</thinking>\n\n".len()..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>\n\n".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    events.extend(self.create_text_delta_events(&remaining));
                }
                break;
            }
        }

        events
    }

    /// 处理 reasoningContentEvent（4.8+ 原生 thinking 流）
    ///
    /// 上游通过独立的 reasoningContentEvent 发送 thinking 内容，
    /// 不再嵌入 `<thinking>` 标签。此方法将其转换为标准的 thinking SSE 事件。
    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            return Vec::new();
        }

        let mut events = Vec::new();

        if let Some(text) = &reasoning.text {
            // thinking 内容增量
            if !self.in_thinking_block {
                // 首次收到 reasoning 内容，开启 thinking 块
                self.in_thinking_block = true;
                self.thinking_extracted = false;
                let thinking_index = self.state_manager.next_block_index();
                self.thinking_block_index = Some(thinking_index);
                let start_events = self.state_manager.handle_content_block_start(
                    thinking_index,
                    "thinking",
                    json!({
                        "type": "content_block_start",
                        "index": thinking_index,
                        "content_block": {
                            "type": "thinking",
                            "thinking": "",
                            "signature": ""
                        }
                    }),
                );
                events.extend(start_events);
            }

            if !text.is_empty() {
                if let Some(thinking_index) = self.thinking_block_index {
                    // thinking 与正文共享输出预算：累计 + 超预算则截断当前 delta
                    let text = self.apply_output_budget(text);
                    if !text.is_empty() {
                        events.push(self.create_thinking_delta_event(thinking_index, &text));
                    }
                }
            }
        } else if let Some(signature) = &reasoning.signature {
            // signature 标志 thinking 块结束
            if !self.in_thinking_block && self.thinking_block_index.is_none() {
                // 上游只发了 signature、从未发过 reasoning text（模型实际没有思考内容）。
                // 对齐官方 API 行为：thinking_tokens=0 时**不发 thinking 块**。
                // 此前的实现会创建一个滞后的空 thinking 块，但 Kiro 上游在不思考时
                // 先发答案 text 才补这个空 signature，导致 thinking 块出现在 text 之后
                // 且与 text 块重叠（违反 SSE 块串行 + thinking 在前的协议）。
                // 故直接丢弃这个孤立 signature，产出干净的 text-only 响应。
                self.thinking_extracted = true;
                tracing::debug!(
                    "reasoning 仅含 signature 且无 thinking 内容（模型未思考），丢弃以对齐官方 text-only 结构"
                );
                let _ = signature;
            } else if self.in_thinking_block {
                // 正常路径：thinking 块已开启，发 signature 关闭它
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 发送 signature_delta
                    events.push(SseEvent::new(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": thinking_index,
                            "delta": {
                                "type": "signature_delta",
                                "signature": signature
                            }
                        }),
                    ));
                    // content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }
            }
        }

        events
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    ///
    /// Anthropic 协议要求 thinking 块在 content_block_stop 之前以 signature_delta 收尾。
    /// 当上游提供真实 signature 时直接透传；文本协议路径下生成格式正确的伪 signature。
    fn create_signature_delta_event(&self, index: i32, signature: &str) -> SseEvent {
        let sig = if signature.is_empty() {
            generate_fake_signature_for_model(&self.model)
        } else {
            signature.to_string()
        };
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": sig
                }
            }),
        )
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &thinking_content),
                        );
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // thinking 块以 signature_delta 收尾
                    events.push(self.create_signature_delta_event(thinking_index, ""));
                    // 再发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use.tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // 发送参数增量 (ToolUseEvent.input 是 String 类型)
        if !tool_use.input.is_empty() {
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4; // 估算 token

            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": tool_use.input
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        // 如果是完整的工具调用（stop=true），发送 content_block_stop
        if tool_use.stop {
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 关闭 thinking 块：发送 signature_delta 收尾，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_signature_delta_event(thinking_index, ""));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &self.thinking_buffer),
                        );
                    }
                    // 关闭 thinking 块：发送 signature_delta 收尾，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // thinking 块以 signature_delta 收尾
                        events.push(self.create_signature_delta_event(thinking_index, ""));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        // 注：不再注入"兜底空 thinking 块"。对齐官方 API：thinking 开启但模型实际
        // 没有思考内容（thinking_tokens=0）时，官方**不发 thinking 块**，直接 text-only。
        // 旧的兜底会在流末尾注入一个空 thinking 块，但因 Kiro 不思考时先发 text、后补
        // 空 signature，注入的块会落在 text 之后并与之重叠，违反 SSE 块串行 + thinking
        // 在前的协议，正是结构完整性扣分点。

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // 客户端可见 usage：输入侧三字段(input/cache_creation/cache_read)用本地缓存账
        // (perceived 假缓存或真断点)，上游 tokenUsageEvent 只接管 output_tokens(含 thinking)。
        // 详见 resolved_usage 的口径来源说明。
        let (final_input_tokens, final_output_tokens, cache_creation, cache_read) =
            self.resolved_usage();

        // 生成最终事件
        events.extend(self.state_manager.generate_final_events(
            final_input_tokens,
            final_output_tokens,
            cache_creation,
            cache_read,
        ));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::events::{Event, ReasoningContentEvent, ToolUseEvent};

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    /// 辅助：从事件列表中找 thinking 类型 content_block_start 的数量
    fn count_thinking_block_starts(events: &[SseEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "thinking"
            })
            .count()
    }

    /// 辅助：取 message_delta 的 usage 三段 + output（对外 SSE 客户端可见口径）
    fn final_usage(events: &[SseEvent]) -> (i64, i64, i64, i64) {
        let md = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        let u = &md.data["usage"];
        (
            u["input_tokens"].as_i64().unwrap_or(-1),
            u["output_tokens"].as_i64().unwrap_or(-1),
            u["cache_creation_input_tokens"].as_i64().unwrap_or(-1),
            u["cache_read_input_tokens"].as_i64().unwrap_or(-1),
        )
    }

    /// 回归：上游 tokenUsageEvent 报 cacheRead=0(Kiro 多数模型如此)时，
    /// **不得**把本地缓存账(perceived 假缓存)算出的 cache_read 顶成 0。
    /// 这是"NewAPI 只看到 cache_creation、看不到 cache_read"症状的根因守卫。
    /// 同时验证 output_tokens 仍采用上游真值(含 thinking)。
    #[test]
    fn token_usage_zero_cache_read_does_not_clobber_local_perceived_cache() {
        use crate::kiro::model::events::TokenUsageEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 12, false, HashMap::new());
        // 模拟本地 perceived 假缓存账：input=12、cache_read=4800、creation=0
        ctx.cache_read_input_tokens = 4800;
        ctx.cache_creation_input_tokens = 0;

        // 上游下发精确计量：output 含 thinking=678，但 cacheRead/cacheWrite 都不报(Kiro 常态)
        let ev = Event::TokenUsage(TokenUsageEvent {
            uncached_input_tokens: 6500, // 含 Kiro agent prompt，不应外泄成 input
            output_tokens: 678,
            total_tokens: 7178,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            extra: Default::default(),
        });
        let _ = ctx.process_kiro_event(&ev);

        let events = ctx.generate_final_events();
        let (input, output, creation, read) = final_usage(&events);

        assert_eq!(
            read, 4800,
            "本地 perceived cache_read 必须保留，不被上游 0 顶掉"
        );
        assert_eq!(creation, 0, "cache_creation 仍走本地账");
        assert_eq!(
            input, 12,
            "input 用本地纯客户端口径，不用上游含 agent prompt 的 6500"
        );
        assert_eq!(output, 678, "output 采用上游真值(含 thinking)");
    }

    /// 上游报了真实 cacheWrite(创建)时，本地账仍权威——输入侧不被上游覆盖。
    /// （本地 perceived/真断点账才是对外计费口径，上游只接管 output。）
    #[test]
    fn token_usage_does_not_override_input_side_even_with_upstream_cache_write() {
        use crate::kiro::model::events::TokenUsageEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 20, false, HashMap::new());
        ctx.cache_read_input_tokens = 9000;
        ctx.cache_creation_input_tokens = 0;

        let ev = Event::TokenUsage(TokenUsageEvent {
            uncached_input_tokens: 5000,
            output_tokens: 100,
            total_tokens: 5300,
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(300),
            extra: Default::default(),
        });
        let _ = ctx.process_kiro_event(&ev);

        let (input, output, creation, read) = final_usage(&ctx.generate_final_events());
        assert_eq!(read, 9000, "本地 cache_read 权威");
        assert_eq!(creation, 0, "本地 cache_creation 权威，不取上游 300");
        assert_eq!(input, 20, "input 用本地口径");
        assert_eq!(output, 100, "output 用上游真值");
    }

    /// output 兜底：上游 output=0(异常/未报)时回退本地估算，不把 output 清零。
    #[test]
    fn token_usage_zero_output_falls_back_to_local_estimate() {
        use crate::kiro::model::events::TokenUsageEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 10, false, HashMap::new());
        ctx.output_tokens = 42; // 本地累计估算
        ctx.cache_read_input_tokens = 1000;

        // has_real_usage 为 true(uncached>0)，但 output_tokens=0 → output 不可信
        let ev = Event::TokenUsage(TokenUsageEvent {
            uncached_input_tokens: 5000,
            output_tokens: 0,
            total_tokens: 5000,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            extra: Default::default(),
        });
        let _ = ctx.process_kiro_event(&ev);

        let (_, output, _, read) = final_usage(&ctx.generate_final_events());
        assert_eq!(output, 42, "上游 output=0 不可信，回退本地估算 42");
        assert_eq!(read, 1000, "cache_read 仍走本地账");
        assert!(!ctx.output_from_upstream(), "output 来源应判定为 estimate");
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.process_tool_use(&ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        }));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    // === 任务 A: Opus 4.7 thinking 兜底注入 ===

    #[test]
    fn thinking_enabled_no_block_injects_empty() {
        // thinking 开启 + 全流程无 thinking 内容 → 对齐官方：不注入空 thinking 块（text-only）。
        // 官方实测 thinking_tokens=0 时根本不发 thinking content_block。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4-7", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 不发送任何文本内容，直接结束流
        let final_events = ctx.generate_final_events();

        assert_eq!(
            count_thinking_block_starts(&final_events),
            0,
            "无 thinking 内容时不应注入空 thinking 块（对齐官方 text-only）"
        );
    }

    #[test]
    fn thinking_enabled_with_real_block_not_injected() {
        // thinking 开启 + 真实 <thinking>x</thinking> 已被处理 → 不应再注入第二个空块
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4-7", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(
            ctx.process_assistant_response("<thinking>\nreal reasoning</thinking>\n\nfinal answer"),
        );
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            count_thinking_block_starts(&all_events),
            1,
            "已有真实 thinking 块时不应再注入空 thinking 块"
        );
    }

    #[test]
    fn thinking_disabled_no_injection() {
        // thinking 关闭 + 无 thinking 块 → 不应注入任何 thinking 结构
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-5", 1, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("hello world"));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            count_thinking_block_starts(&all_events),
            0,
            "thinking 关闭时不应注入任何 thinking 块"
        );
    }

    #[test]
    fn thinking_enabled_only_text_response() {
        // thinking 开启 + 纯文本响应（模型完全没思考）
        // → 对齐官方：不注入 thinking 块，只有 text block（text-only）。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4-7", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        // 注意：thinking 开启时短文本可能被 buffer 拦截等待 <thinking>，
        // 用足够长的文本确保被 flush 成 text_delta
        all_events.extend(ctx.process_assistant_response(
            "Hi! This is a direct answer without any thinking tag whatsoever.",
        ));
        all_events.extend(ctx.generate_final_events());

        // 不注入 thinking 块（对齐官方）
        assert_eq!(
            count_thinking_block_starts(&all_events),
            0,
            "纯文本响应不应注入 thinking 块（对齐官方 text-only）"
        );

        // text block 应存在
        let has_text_block = all_events
            .iter()
            .any(|e| e.event == "content_block_start" && e.data["content_block"]["type"] == "text");
        assert!(has_text_block, "text block 应正常存在");

        // 验证 SSE 结构完整：message_stop 在最后
        let last_message_stop_idx = all_events.iter().rposition(|e| e.event == "message_stop");
        assert!(last_message_stop_idx.is_some(), "应有 message_stop 收尾");
    }

    #[test]
    fn test_reasoning_content_event_streaming() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        // 首次 text delta 应开启 thinking 块
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("Hello ".to_string()),
            signature: None,
        }));
        // content_block_start + content_block_delta
        assert!(events.len() >= 2);
        assert_eq!(events[0].data["type"], "content_block_start");
        assert_eq!(events[0].data["content_block"]["type"], "thinking");
        assert_eq!(events[1].data["delta"]["type"], "thinking_delta");
        assert_eq!(events[1].data["delta"]["thinking"], "Hello ");

        // 后续 text delta 不再开新块
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("world".to_string()),
            signature: None,
        }));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["delta"]["thinking"], "world");

        // signature 关闭 thinking 块
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: None,
            signature: Some("sig123".to_string()),
        }));
        // signature_delta + content_block_stop
        assert!(events.len() >= 2);
        assert_eq!(events[0].data["delta"]["type"], "signature_delta");
        assert_eq!(events[0].data["delta"]["signature"], "sig123");
        assert_eq!(events[1].data["type"], "content_block_stop");

        // thinking 已结束，后续 assistant response 应作为 text 块
        assert!(ctx.thinking_extracted);
        assert!(!ctx.in_thinking_block);
    }

    // === 输出预算（max_tokens）截断：StreamContext 集成部分 ===

    #[test]
    fn no_budget_means_no_truncation() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, false, HashMap::new());
        // 未设预算
        assert!(ctx.max_output_tokens.is_none());
        let out = ctx.apply_output_budget("hello world this is a long text");
        assert_eq!(out, "hello world this is a long text");
        assert!(!ctx.budget_exceeded);
    }

    #[test]
    fn budget_truncates_and_sets_stop_reason() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, false, HashMap::new());
        ctx.set_max_output_tokens(2); // 预算 2 token

        // 第一段刚好 2 token：放行，到顶
        let out = ctx.apply_output_budget("aaaaaaaa"); // 2 token
        assert!(estimate_tokens(&out) <= 2);
        assert!(ctx.output_tokens <= 2);

        // 后续任何内容都应被丢弃（预算已用尽）
        let out2 = ctx.apply_output_budget("more text here");
        assert_eq!(out2, "", "预算耗尽后应丢弃后续内容");
        assert!(ctx.budget_exceeded, "应置 budget_exceeded");
        assert_eq!(ctx.state_manager.get_stop_reason(), "max_tokens");
    }

    #[test]
    fn budget_partial_truncation_midway() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, false, HashMap::new());
        ctx.set_max_output_tokens(3);
        // 一段超预算文本：应截断到 3 token 并置位
        let out = ctx.apply_output_budget("aaaaaaaaaaaaaaaaaaaa"); // 20 字符 = 5 token
        assert!(estimate_tokens(&out) <= 3, "截断后 ≤ 预算");
        assert!(!out.is_empty());
        assert!(ctx.budget_exceeded);
        assert_eq!(ctx.state_manager.get_stop_reason(), "max_tokens");
    }

    #[test]
    fn budget_shared_between_thinking_and_text() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, true, HashMap::new());
        let _ = ctx.generate_initial_events();
        ctx.set_max_output_tokens(2);

        // thinking 先吃满预算
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("aaaaaaaa".to_string()), // 2 token
            signature: None,
        }));
        assert!(ctx.output_tokens <= 2);

        // 预算耗尽后再来 thinking 内容应被丢弃
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("more thinking".to_string()),
            signature: None,
        }));
        // 不应产生新的 thinking_delta（内容被预算截没了）
        let has_delta = events
            .iter()
            .any(|e| e.data["delta"]["type"] == "thinking_delta");
        assert!(!has_delta, "预算耗尽后 thinking 不应再发 delta");
        assert!(ctx.budget_exceeded);
        assert_eq!(ctx.state_manager.get_stop_reason(), "max_tokens");
    }

    // === SSE 结构完整性 ===

    #[test]
    fn thinking_content_block_start_has_signature_field() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, true, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 触发 <thinking> 文本协议路径开块
        let events = ctx.process_assistant_response("<thinking>reasoning");
        let start = events
            .iter()
            .find(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "thinking"
            })
            .expect("应有 thinking content_block_start");
        assert_eq!(
            start.data["content_block"]["signature"], "",
            "thinking content_block_start 必须含 signature 字段"
        );
    }

    #[test]
    fn thinking_block_closes_with_signature_delta() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 100, true, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 完整 thinking 块 + 正文：</thinking>\n\n 在同一 chunk，关闭发生在 process_assistant_response
        let mut all_events = ctx.process_assistant_response("<thinking>think</thinking>\n\nanswer");
        all_events.extend(ctx.generate_final_events());

        // 关闭 thinking 块应发 signature_delta，而非空 thinking_delta
        let has_sig = all_events
            .iter()
            .any(|e| e.data["delta"]["type"] == "signature_delta");
        assert!(has_sig, "thinking 块应以 signature_delta 收尾");
        // 且 thinking 块必须有对应的 content_block_stop
        let has_stop = all_events.iter().any(|e| e.event == "content_block_stop");
        assert!(has_stop, "thinking 块应有 content_block_stop");
    }
}
