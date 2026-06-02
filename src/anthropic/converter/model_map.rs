//! 模型名映射与上下文窗口判断

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID
///
/// 按照用户要求：
/// - sonnet 4.6/4-6 → claude-sonnet-4.6
/// - 其他 sonnet → claude-sonnet-4.5
/// - opus 4.7/4-7 → claude-opus-4.7
/// - opus 4.5/4-5 → claude-opus-4.5
/// - 其他 opus → claude-opus-4.6
/// - 所有 haiku → claude-haiku-4.5
pub fn map_model(model: &str) -> Option<String> {
    let model_lower = model.to_lowercase();

    if model_lower.contains("sonnet") {
        if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else {
            Some("claude-sonnet-4.5".to_string())
        }
    } else if model_lower.contains("opus") {
        if model_lower.contains("4-8") || model_lower.contains("4.8") {
            // 预埋：Kiro 上架 opus 4.8 后自动生效（上游别名预期为 claude-opus-4.8）。
            // 上架前若有人硬请求 4.8，上游会返回"未知模型"——诚实失败，
            // 优于静默降级到 4.6 让用户误以为在用 4.8。
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
    if lower.contains("haiku") {
        return "claude-haiku-4-5-20251001".to_string();
    }
    if lower.contains("sonnet") {
        if lower.contains("4-6") || lower.contains("4.6") {
            return "claude-sonnet-4-6".to_string();
        }
        return "claude-sonnet-4-5-20250929".to_string();
    }
    if lower.contains("opus") {
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
/// Opus 4.7 同样为 1M 上下文。
pub fn get_context_window_size(model: &str) -> i32 {
    match map_model(model) {
        Some(mapped)
            if mapped == "claude-sonnet-4.6"
                || mapped == "claude-opus-4.6"
                || mapped == "claude-opus-4.7"
                || mapped == "claude-opus-4.8" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}
