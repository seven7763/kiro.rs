//! 模型名映射与上下文窗口判断
//!
//! 结构对标 Kiro-Go（`proxy/translator.go` 的 `ParseModelAndThinking`）：
//! 别名表 → 正则版本规范化 → 直通。取代原先「每个型号一个 `contains` 分支、
//! 不在白名单就 `None`」的写法。
//!
//! 为什么换：白名单模式下上游每上架一个型号都要改代码，漏改的直接对客户端
//! 报「模型不支持」。生产实测上游 `ListAvailableModels` 返回 28 个 ID，其中
//! `glm-5` / `minimax-m2.5` / `minimax-m2.1` / `qwen3-coder-next` 全部因为
//! 没有分支而不可用。
//!
//! 相比 Kiro-Go 的改进：它只给 `claude-sonnet-4-20250514` 做了日期别名，
//! opus / haiku 的带日期 ID 会被原样直通到上游（上游没有这些 ID）。这里
//! 三个家族都补齐。

use regex::Regex;
use std::sync::LazyLock;

/// 需要显式改写的模型名：带日期的快照、跨家族的历史 ID、非 Anthropic 别名。
///
/// 匹配方式是子串包含，所以**顺序有意义**——更长更具体的键必须在前，
/// 否则 `gpt-4` 会先截获 `gpt-4-turbo`。
///
/// 纯粹的 `-N-M` → `-N.M` 连字符转点号由 [`CLAUDE_VERSION_PATTERN`] 处理，
/// 不要往这里加。
const MODEL_ALIASES: &[(&str, &str)] = &[
    // 只有 major、没有 minor 的历史快照：剥掉日期后是 `claude-opus-4` /
    // `claude-haiku-4`，上游没有这两个 ID，必须显式指到在售型号。
    // （`claude-sonnet-4` 上游确实有，剥日期后直通即可，但显式列出更清楚。）
    ("claude-sonnet-4-20250514", "claude-sonnet-4"),
    ("claude-opus-4-20250514", "claude-opus-4.5"),
    ("claude-haiku-4-20250514", "claude-haiku-4.5"),
    // Claude 3 世代：上游早已下架，路由到最接近的在售型号
    ("claude-3-5-sonnet", "claude-sonnet-4.5"),
    ("claude-3-7-sonnet", "claude-sonnet-4.5"),
    ("claude-3-opus", "claude-sonnet-4.5"),
    ("claude-3-sonnet", "claude-sonnet-4"),
    ("claude-3-haiku", "claude-haiku-4.5"),
    // Fable 5 没有版本号可规范化，显式列出
    ("fable", "claude-fable-5"),
    // 非 Anthropic 历史别名（OpenAI 旧型号）。注意上游现在有真实的
    // gpt-5.6-* 系列，那些走直通，不能被这里截获——键都带 "-4"/"-3.5"，
    // 与 "gpt-5.6-sol" 不重叠。
    ("gpt-4-turbo", "claude-sonnet-4.5"),
    ("gpt-4o", "claude-sonnet-4.5"),
    ("gpt-4", "claude-sonnet-4.5"),
    ("gpt-3.5-turbo", "claude-sonnet-4.5"),
];

/// 把 `claude-{family}-N-M` 规范成 `claude-{family}-N.M`（上游只认点号形式）。
///
/// minor 限制 1~2 位且要求词边界，这样带日期的快照 ID
/// （`claude-opus-4-20250514`，minor 位置是 8 位数字）不会被误改写。
static CLAUDE_VERSION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"claude-(opus|sonnet|haiku)-(\d+)-(\d{1,2})\b").unwrap());

/// 提取 `claude-{family}-{major}[.{minor}]` 的版本号，用于上下文窗口判定。
///
/// minor 可缺省，这样 `claude-opus-5` 也能正确分类，而不是落到 200K 默认值。
static CLAUDE_VERSION_EXTRACTOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"claude-(?:opus|sonnet|haiku|fable)-(\d+)(?:[.-](\d+))?").unwrap()
});

/// 匹配结尾的 8 位日期后缀（`-20260515`）。
static DATE_SUFFIX_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{8}$").unwrap());

/// 剥掉 kiro-rs 私有的 thinking 后缀约定。
fn strip_thinking_suffix(model: &str) -> &str {
    model
        .trim_end_matches("-thinking")
        .trim_end_matches("_thinking")
}

/// 模型映射：把客户端传来的模型名解析成上游 Kiro 的 model ID。
///
/// 三级处理，与 Kiro-Go 一致：
/// 1. 别名表命中 → 显式改写（带日期快照、历史 ID、非 Anthropic 别名）
/// 2. `claude-{family}-N-M` → `claude-{family}-N.M`（新版本自动支持，无需改代码）
/// 3. 其余一律直通（上游列表里的 glm-5 / minimax / qwen / gpt-5.6-* 因此可用）
///
/// 只有空白输入返回 `None`。直通不等于什么都收，但也不再由本函数充当
/// 上游型号白名单——上游自己会拒绝它不认识的 ID。
pub fn map_model(model: &str) -> Option<String> {
    let trimmed = strip_thinking_suffix(model.trim());
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();

    // 1. 别名表（子串匹配，顺序敏感）
    for (key, value) in MODEL_ALIASES {
        if lower.contains(key) {
            return Some((*value).to_string());
        }
    }

    // 2. 剥掉 claude 系的 8 位日期后缀。
    //    上游只认无日期短名，而带日期的 ID 走不到步骤 3 的版本规范化
    //    （`\d{1,2}\b` 的词边界会拒绝 8 位数字，这正是为了不把日期当 minor）。
    //    非 claude 型号不动——它们的 ID 可能本来就带数字尾巴。
    let base = if lower.starts_with("claude-") {
        DATE_SUFFIX_PATTERN.replace(&lower, "").into_owned()
    } else {
        lower
    };

    // 3. 版本号连字符 → 点号
    if CLAUDE_VERSION_PATTERN.is_match(&base) {
        return Some(
            CLAUDE_VERSION_PATTERN
                .replace_all(&base, "claude-$1-$2.$3")
                .into_owned(),
        );
    }

    // 4. 直通（已是点号形式的 claude、以及全部非 claude 型号）
    Some(base)
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
/// 按版本号推导而非硬编码白名单（对标 Kiro-Go 的 `isLargeContextModel`）：
/// - major >= 5 → 1M（claude-opus-5、claude-sonnet-5.1、未来的 6.x …）
/// - major == 4 且 minor >= 6 → 1M（Kiro 于 2026-03-24 把 4.6 升到 1M）
/// - 缺 minor 视作 `.0`（`claude-sonnet-4` → 200K）
/// - 其余 → 200K
///
/// 这个值用来把上游的 `contextUsagePercentage` 还原成绝对 input token 数，
/// 客户端靠它决定何时压缩上下文。窗口取小了会低报 token，客户端来不及压缩。
pub fn get_context_window_size(model: &str) -> i32 {
    let mapped = match map_model(model) {
        Some(m) => m,
        None => return 200_000,
    };

    if let Some(caps) = CLAUDE_VERSION_EXTRACTOR.captures(&mapped) {
        if let Some(major) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) {
            if major > 4 {
                return 1_000_000;
            }
            let minor = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            if major == 4 && minor >= 6 {
                return 1_000_000;
            }
            return 200_000;
        }
    }

    // 非标准 ID 的兜底子串检查（与 Kiro-Go 同策略）
    for tag in ["4.6", "4-6", "4.7", "4-7", "4.8", "4-8", "4.9", "4-9"] {
        if mapped.contains(tag) {
            return 1_000_000;
        }
    }
    200_000
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
        assert_eq!(
            get_context_window_size("claude-haiku-4-5-20251001"),
            200_000
        );
    }

    /// 上游 ListAvailableModels 里的非 Claude 模型必须直通。
    ///
    /// 旧的白名单式 `map_model` 只认 sonnet/opus/haiku，其他一律 `None`
    /// → `ConversionError::UnsupportedModel` → 客户端拿到「模型不支持: glm-5」。
    /// 但这些 ID 确实在上游模型列表里（生产实测 28 条含 glm-5 / minimax / qwen）。
    #[test]
    fn non_claude_upstream_models_pass_through() {
        for m in [
            "glm-5",
            "minimax-m2.5",
            "minimax-m2.1",
            "qwen3-coder-next",
            "deepseek-3.2",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            assert_eq!(
                map_model(m).as_deref(),
                Some(m),
                "{m} 在上游列表里，必须直通而不是被判成不支持"
            );
        }
    }

    /// 未来版本号无需改代码即可正确规范化（`-N-M` → `-N.M`）。
    /// 这是对标 Kiro-Go 的核心收益：上游上架 claude-opus-4.9 当天就能用。
    #[test]
    fn future_claude_versions_normalize_without_code_change() {
        let cases = [
            ("claude-opus-4-9", "claude-opus-4.9"),
            ("claude-sonnet-5-1", "claude-sonnet-5.1"),
            ("claude-opus-6-2", "claude-opus-6.2"),
            ("claude-haiku-5-3", "claude-haiku-5.3"),
            // 已是点号形式 → 原样直通
            ("claude-opus-4.9", "claude-opus-4.9"),
        ];
        for (input, expect) in cases {
            assert_eq!(map_model(input).as_deref(), Some(expect), "{input}");
        }
    }

    /// 上下文窗口按版本号推导，不再是硬编码白名单。
    /// major >= 5 → 1M；4.x 看 minor（>= 6 → 1M）；缺 minor 视作 .0。
    #[test]
    fn context_window_derives_from_version() {
        for m in ["claude-opus-4-9", "claude-sonnet-5-1", "claude-opus-6"] {
            assert_eq!(get_context_window_size(m), 1_000_000, "{m} 应为 1M");
        }
        for m in ["claude-sonnet-4", "claude-haiku-4-5", "claude-opus-4-5"] {
            assert_eq!(get_context_window_size(m), 200_000, "{m} 应为 200K");
        }
    }

    /// 空模型名仍要拒绝——直通不等于什么都收。
    #[test]
    fn blank_model_is_rejected() {
        assert!(map_model("").is_none());
        assert!(map_model("   ").is_none());
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
        assert_eq!(
            canonical_anthropic_model("claude-sonnet-5"),
            "claude-sonnet-5"
        );
        assert_eq!(
            canonical_anthropic_model("claude-fable-5"),
            "claude-fable-5"
        );
    }
}
