//! 模型名映射与上下文窗口判断

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID
///
/// 按照用户要求：
/// - Claude 5 系列：opus-5 / fable-5 / sonnet-5 → 直接映射
/// - sonnet 4.6/4-6 → claude-sonnet-4.6
/// - 其他 sonnet → claude-sonnet-4.5
/// - opus 4.7/4-7 → claude-opus-4.7
/// - opus 4.5/4-5 → claude-opus-4.5
/// - 其他 opus → claude-opus-4.6
/// - 所有 haiku → claude-haiku-4.5
pub fn map_model(model: &str) -> Option<String> {
    let model_lower = model.to_lowercase();

    // Claude 5 系列优先匹配（避免被 "opus" / "sonnet" 通用分支截获）
    if model_lower.contains("fable") {
        return Some("claude-fable-5".to_string());
    }

    if model_lower.contains("sonnet") {
        // 精确匹配 "sonnet-5"：宽松的 contains("5") 会误伤带日期的 ID
        // （如 claude-sonnet-4-6-2025xx15，日期含 5 却不含 4-5）
        if model_lower.contains("sonnet-5") {
            return Some("claude-sonnet-5".to_string());
        }
        if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else {
            Some("claude-sonnet-4.5".to_string())
        }
    } else if model_lower.contains("opus") {
        // 精确匹配 "opus-5"，理由同 sonnet-5
        if model_lower.contains("opus-5") {
            return Some("claude-opus-5".to_string());
        }
        if model_lower.contains("4-8") || model_lower.contains("4.8") {
            Some("claude-opus-4.8".to_string())
        } else if model_lower.contains("4-7") || model_lower.contains("4.7") {
            Some("claude-opus-4.7".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-opus-4.5".to_string())
        } else {
            Some("claude-opus-4.6".to_string())
        }
    } else if model_lower.contains("haiku") {
        Some("claude-haiku-4.5".to_string())
    } else {
        None
    }
}

/// 把请求里的模型名规范化为 Anthropic 官方带日期版本号的 model ID。
/// 用于响应体里的 `model` 字段，让做"模型签名校验"的检测方拿到一个
/// 真正的官方版本号而不是回声请求字段或者 Kiro 内部别名。
///
/// 优先级：
/// 1. 如果输入已经是带日期的官方 ID（含 8 位数字），直接返回（去掉 "-thinking" 后缀）。
/// 2. 否则按系列映射到当前最新的官方 ID。
pub fn canonical_anthropic_model(requested: &str) -> String {
    // 去掉 "-thinking" / "_thinking" 后缀（kiro-rs 私有约定）
    let cleaned = requested
        .trim_end_matches("-thinking")
        .trim_end_matches("_thinking")
        .to_string();

    // 已带 8 位日期版本号 → 视为官方 ID 直接返回
    let has_date_suffix = cleaned
        .rsplit('-')
        .next()
        .map(|s| s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);
    if has_date_suffix {
        return cleaned;
    }

    let lower = cleaned.to_lowercase();

    // Claude 5 系列
    if lower.contains("fable") {
        return "claude-fable-5".to_string();
    }
    if lower.contains("haiku") {
        return "claude-haiku-4-5-20251001".to_string();
    }
    if lower.contains("sonnet") {
        if lower.contains("sonnet-5") {
            return "claude-sonnet-5".to_string();
        }
        if lower.contains("4-6") || lower.contains("4.6") {
            return "claude-sonnet-4-6".to_string();
        }
        return "claude-sonnet-4-5-20250929".to_string();
    }
    if lower.contains("opus") {
        if lower.contains("opus-5") {
            return "claude-opus-5".to_string();
        }
        if lower.contains("4-8") || lower.contains("4.8") {
            return "claude-opus-4-8".to_string();
        }
        if lower.contains("4-7") || lower.contains("4.7") {
            return "claude-opus-4-7".to_string();
        }
        if lower.contains("4-6") || lower.contains("4.6") {
            return "claude-opus-4-6".to_string();
        }
        if lower.contains("4-5") || lower.contains("4.5") {
            return "claude-opus-4-5-20251101".to_string();
        }
        return "claude-opus-4-6".to_string();
    }
    // 兜底：未识别就原样返回
    cleaned
}

/// 根据模型名称返回对应的上下文窗口大小
///
/// 复用 `map_model` 的映射逻辑，确保窗口大小判断与模型映射一致。
/// Kiro 于 2026-03-24 将 Opus 4.6 和 Sonnet 4.6 升级至 1M 上下文。
/// Opus 4.7 / 4.8 / 5 / Fable 5 / Sonnet 5 同样为 1M 上下文。
pub fn get_context_window_size(model: &str) -> i32 {
    match map_model(model) {
        Some(mapped)
            if mapped == "claude-sonnet-4.6"
                || mapped == "claude-opus-4.6"
                || mapped == "claude-opus-4.7"
                || mapped == "claude-opus-4.8"
                || mapped == "claude-opus-5"
                || mapped == "claude-fable-5"
                || mapped == "claude-sonnet-5" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_5_family_maps_directly() {
        assert_eq!(map_model("claude-opus-5").as_deref(), Some("claude-opus-5"));
        assert_eq!(
            map_model("claude-opus-5-thinking").as_deref(),
            Some("claude-opus-5")
        );
        assert_eq!(
            map_model("claude-sonnet-5").as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            map_model("claude-fable-5").as_deref(),
            Some("claude-fable-5")
        );
    }

    /// 精确匹配的核心价值：带日期的旧 model ID 里含 "5" 但不能被判成 Claude 5。
    /// 宽松的 `contains("5")` 写法会让这些全部误伤。
    #[test]
    fn dated_legacy_ids_are_not_mistaken_for_claude_5() {
        let cases = [
            ("claude-opus-4-5-20251101", "claude-opus-4.5"),
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4.5"),
            // 假想的含 5 日期：4.6 不能被吞成 5
            ("claude-opus-4-6-20260515", "claude-opus-4.6"),
            ("claude-sonnet-4-6-20260525", "claude-sonnet-4.6"),
        ];
        for (input, expect) in cases {
            assert_eq!(
                map_model(input).as_deref(),
                Some(expect),
                "{input} 被误判为 Claude 5"
            );
        }
    }

    #[test]
    fn claude_5_family_gets_1m_context() {
        for m in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
        ] {
            assert_eq!(get_context_window_size(m), 1_000_000, "{m} 应为 1M 上下文");
        }
        assert_eq!(get_context_window_size("claude-opus-4-5-20251101"), 200_000);
        assert_eq!(get_context_window_size("claude-haiku-4-5-20251001"), 200_000);
    }

    #[test]
    fn canonical_model_keeps_dated_ids_and_maps_claude_5() {
        // 带 8 位日期的官方 ID 原样返回
        assert_eq!(
            canonical_anthropic_model("claude-opus-4-5-20251101"),
            "claude-opus-4-5-20251101"
        );
        // thinking 后缀剥离
        assert_eq!(
            canonical_anthropic_model("claude-opus-5-thinking"),
            "claude-opus-5"
        );
        assert_eq!(canonical_anthropic_model("claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(canonical_anthropic_model("claude-fable-5"), "claude-fable-5");
    }
}
