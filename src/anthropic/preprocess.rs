//! 请求预处理（system prompt 注入/剥离、thinking 配置规范化）
//!
//! 从 handlers.rs 抽出。handler 在转换前调用：
//! - [`inject_system_prompt`]：按运行时配置剥离客户端 system 限制 + 注入 preset/custom。
//! - [`override_thinking_from_model_name`]：按模型名/版本规范化 thinking 配置
//!   （Opus 4.7 `enabled`→`adaptive` 降级、`*-thinking` 后缀强制开启）。

use crate::model::config::SystemPromptPosition;
use crate::model::runtime::SharedPromptConfig;

use super::types::{MessagesRequest, OutputConfig, SystemMessage, Thinking};

/// 注入自定义系统提示词 & 剥离限制
///
/// 两个独立动作：
/// 1. 若 `strip_system_restrictions` 为 true，先剥离客户端 system prompt 中的限制片段
/// 2. 若总开关 `enabled`，调 `build_injection_text` 拼接 preset + custom，按 `position` 插入
pub(crate) fn inject_system_prompt(payload: &mut MessagesRequest, shared: &SharedPromptConfig) {
    // 取一次快照，立即释放读锁
    let (injection, position, strip_restrictions) = {
        let cfg = shared.read();
        (
            cfg.build_injection_text(),
            cfg.position,
            cfg.strip_system_restrictions,
        )
    };

    // 1. 剥离限制
    if strip_restrictions {
        if let Some(ref mut system) = payload.system {
            for msg in system.iter_mut() {
                let stripped = super::prompt_filter::strip_restrictions(&msg.text);
                if stripped.len() != msg.text.len() {
                    tracing::info!(
                        "剥离系统提示词限制: {} → {} bytes",
                        msg.text.len(),
                        stripped.len()
                    );
                    msg.text = stripped;
                }
            }
        }
    }

    // 2. 注入
    let Some(text) = injection else {
        return;
    };
    let injected = SystemMessage {
        text,
        cache_control: None,
    };

    match &mut payload.system {
        Some(existing) => match position {
            SystemPromptPosition::Prepend => existing.insert(0, injected),
            SystemPromptPosition::Append => existing.push(injected),
        },
        None => {
            payload.system = Some(vec![injected]);
        }
    }
}

/// 模型名 / Thinking 配置规范化
///
/// 处理两类情况：
///
/// 1. **Opus 4.7 thinking 兼容性修正**（无论 model 名是否带 "thinking" 后缀）：
///    Opus 4.7 在 Kiro/Bedrock 上**不支持** `thinking.type = "enabled"`，
///    必须使用 `"adaptive"`（参考: AWS Bedrock Opus 4.7 文档）。
///    如果客户端传了 `enabled`，自动降级为 `adaptive` 并补一个默认 `effort`。
///
/// 2. **`*-thinking` 后缀 → 强制启用 thinking**（原行为）：
///    - Opus 4.6/4.7 → `adaptive`（带 `effort: high`）
///    - 其他模型 → `enabled`，budget_tokens=20000
pub(crate) fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    let is_opus = model_lower.contains("opus");
    let is_opus_4_7 = is_opus && (model_lower.contains("4-7") || model_lower.contains("4.7"));
    let is_opus_4_6 = is_opus && (model_lower.contains("4-6") || model_lower.contains("4.6"));
    let is_opus_4_6_or_newer = is_opus_4_6 || is_opus_4_7;
    let has_thinking_suffix = model_lower.contains("thinking");

    // Case 1: Opus 4.7 强制 adaptive — 不论是否带 thinking 后缀
    if is_opus_4_7 {
        if let Some(ref mut t) = payload.thinking {
            if t.thinking_type == "enabled" {
                tracing::info!(
                    model = %payload.model,
                    "Opus 4.7 不支持 thinking.type=\"enabled\"，自动降级为 \"adaptive\""
                );
                t.thinking_type = "adaptive".to_string();
            }
            // 4.7 上游默认 display=omitted，强制设 summarized 让 Kiro 吐 thinking 文本
            if t.display.is_none() {
                t.display = Some("summarized".to_string());
            }
            // adaptive 需要 output_config.effort，缺省补 "high"
            if payload.output_config.is_none() {
                payload.output_config = Some(OutputConfig {
                    effort: "high".to_string(),
                });
            }
        }
    }

    // Case 2: model 名带 "*-thinking" 后缀 → 强制开启 thinking
    if !has_thinking_suffix {
        return;
    }

    let thinking_type = if is_opus_4_6_or_newer {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
        // adaptive 模式（4.6/4.7）默认 summarized 让 Kiro 吐 thinking 文本
        display: if thinking_type == "adaptive" {
            Some("summarized".to_string())
        } else {
            None
        },
    });

    if is_opus_4_6_or_newer {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

// PREPROCESS_TESTS_PLACEHOLDER

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小可用的 MessagesRequest
    fn make_req(model: &str, thinking: Option<Thinking>) -> MessagesRequest {
        let mut payload: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": model,
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("构造 MessagesRequest 应成功");
        payload.thinking = thinking;
        payload
    }

    fn thinking(thinking_type: &str, display: Option<&str>) -> Thinking {
        Thinking {
            thinking_type: thinking_type.to_string(),
            budget_tokens: 5000,
            display: display.map(String::from),
        }
    }

    // === Case 1: Opus 4.7 强制 adaptive ===

    #[test]
    fn opus_4_7_enabled_downgrades_to_adaptive() {
        let mut req = make_req("claude-opus-4-7", Some(thinking("enabled", None)));
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().expect("应保留 thinking");
        assert_eq!(t.thinking_type, "adaptive", "enabled 应被降级");
        assert_eq!(t.display.as_deref(), Some("summarized"), "应补 display");
        let oc = req.output_config.as_ref().expect("应补 output_config");
        assert_eq!(oc.effort, "high");
    }

    #[test]
    fn opus_4_7_dot_form_also_downgrades() {
        // claude-opus-4.7（点形式）也应触发
        let mut req = make_req("claude-opus-4.7", Some(thinking("enabled", None)));
        override_thinking_from_model_name(&mut req);
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "adaptive");
    }

    #[test]
    fn opus_4_7_adaptive_keeps_existing_display() {
        let mut req = make_req(
            "claude-opus-4-7",
            Some(thinking("adaptive", Some("omitted"))),
        );
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(
            t.display.as_deref(),
            Some("omitted"),
            "已有 display 不应被覆盖"
        );
    }

    #[test]
    fn opus_4_7_without_thinking_field_is_noop() {
        let mut req = make_req("claude-opus-4-7", None);
        override_thinking_from_model_name(&mut req);
        assert!(req.thinking.is_none(), "无 thinking 字段应保持无");
        assert!(req.output_config.is_none(), "也不应主动注入 output_config");
    }

    // === Case 2: 模型名带 -thinking 后缀 ===

    #[test]
    fn opus_4_7_thinking_suffix_sets_adaptive() {
        let mut req = make_req("claude-opus-4-7-thinking", None);
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().expect("后缀应注入 thinking");
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.display.as_deref(), Some("summarized"));
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn opus_4_6_thinking_suffix_sets_adaptive() {
        let mut req = make_req("claude-opus-4-6-thinking", None);
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive", "4.6 也走 adaptive");
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn sonnet_thinking_suffix_sets_enabled() {
        let mut req = make_req("claude-sonnet-4-5-thinking", None);
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "enabled", "非 opus 4.6+ 应保留 enabled");
        assert_eq!(t.budget_tokens, 20000);
        assert!(req.output_config.is_none(), "enabled 不需要 output_config");
    }

    // === Anti-regression: 普通模型不应被改写 ===

    #[test]
    fn plain_model_without_suffix_is_noop() {
        let mut req = make_req("claude-sonnet-4-5", Some(thinking("enabled", None)));
        override_thinking_from_model_name(&mut req);

        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "enabled", "普通模型 enabled 不应被改");
        assert_eq!(t.display, None);
    }

    // === Thinking.display 反序列化校验 ===

    #[test]
    fn display_accepts_valid_values() {
        for v in ["summarized", "omitted"] {
            let t: Thinking = serde_json::from_value(serde_json::json!({
                "type": "adaptive",
                "display": v,
            }))
            .expect("有效值应解析成功");
            assert_eq!(t.display.as_deref(), Some(v));
        }
    }

    #[test]
    fn display_rejects_invalid_value_silently() {
        let t: Thinking = serde_json::from_value(serde_json::json!({
            "type": "adaptive",
            "display": "raw",
        }))
        .expect("无效值不应导致解析失败");
        assert_eq!(t.display, None, "脏值应被降级为 None");
    }

    #[test]
    fn effective_display_fallback() {
        let t = thinking("adaptive", None);
        assert_eq!(t.effective_display(), "summarized");
        let t = thinking("adaptive", Some("omitted"));
        assert_eq!(t.effective_display(), "omitted");
    }
}
