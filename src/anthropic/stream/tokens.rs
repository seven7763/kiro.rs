//! token 估算与按预算截断

/// 判断字符是否属于 CJK 类（按 1.5 字符/token 估算）
///
/// 覆盖范围：
/// - 平假名 `3040-309F`、片假名 `30A0-30FF`、片假名扩展 `31F0-31FF`
/// - CJK 基本 `4E00-9FFF`
/// - CJK 扩展 A `3400-4DBF`
/// - CJK 兼容汉字 `F900-FAFF`
/// - 韩文音节 `AC00-D7AF`
/// - CJK 扩展 B-D `20000-2B81F`
///
/// 之前仅识别基本块（4E00-9FFF），日韩文本按英文计算，token 偏低约 2.5 倍。
fn is_cjk_like(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}'   // 平假名
        | '\u{30A0}'..='\u{30FF}' // 片假名
        | '\u{31F0}'..='\u{31FF}' // 片假名扩展
        | '\u{3400}'..='\u{4DBF}' // CJK 扩展 A
        | '\u{4E00}'..='\u{9FFF}' // CJK 基本
        | '\u{AC00}'..='\u{D7AF}' // 韩文音节
        | '\u{F900}'..='\u{FAFF}' // CJK 兼容
        | '\u{20000}'..='\u{2A6DF}' // CJK 扩展 B
        | '\u{2A700}'..='\u{2B73F}' // CJK 扩展 C
        | '\u{2B740}'..='\u{2B81F}' // CJK 扩展 D
    )
}

/// 把上游返回的 `context_usage_percentage` 限定到合法范围 `[0.0, 100.0]`
///
/// 防御性 clamp，避免上游 Kiro 偶发异常值导致 input_tokens 错乱：
/// - `NaN` / `inf` → 0.0
/// - 负数 → 0.0
/// - `>100` → 100.0
///
/// 使用 `is_finite` 先过滤是因为 `f64::clamp(NaN, _, _)` 会传播 `NaN`。
pub(crate) fn clamp_context_percentage(pct: f64) -> f64 {
    if !pct.is_finite() {
        return 0.0;
    }
    pct.clamp(0.0, 100.0)
}

/// 简单的 token 估算
pub(crate) fn estimate_tokens(text: &str) -> i32 {
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in text.chars() {
        if is_cjk_like(c) {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // CJK 约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

/// 把文本按 token 预算截断（与 [`estimate_tokens`] 同口径：CJK≈1.5 字符/token，
/// 其他≈4 字符/token）。逐字符累计 token，达到 `budget` 即在字符边界切断。
///
/// 用于输出预算到顶时截断当前 delta，保证发给客户端的内容不超过 max_tokens。
/// `budget <= 0` 返回空串。
pub(crate) fn truncate_text_to_token_budget(text: &str, budget: i32) -> String {
    if budget <= 0 {
        return String::new();
    }

    let mut chinese_count: i32 = 0;
    let mut other_count: i32 = 0;
    let mut byte_end = 0;

    for (idx, c) in text.char_indices() {
        let (next_cjk, next_other) = if is_cjk_like(c) {
            (chinese_count + 1, other_count)
        } else {
            (chinese_count, other_count + 1)
        };
        // 与 estimate_tokens 完全一致的分段累计
        let tokens = (next_cjk * 2 + 2) / 3 + (next_other + 3) / 4;
        if tokens > budget {
            break;
        }
        chinese_count = next_cjk;
        other_count = next_other;
        byte_end = idx + c.len_utf8();
    }

    text[..byte_end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    // === Bug 2: estimate_tokens 多语言覆盖 ===

    #[test]
    fn estimate_tokens_japanese_kana() {
        // 5 个假名字符，按 CJK 算应约 (5*2+2)/3 = 4 token；不应像之前那样按英文算成 (5+3)/4 = 2 token
        let tokens = estimate_tokens("こんにちは");
        assert_eq!(tokens, 4, "5 个平假名应按 CJK 计算 ≈ 4 token");

        let tokens_katakana = estimate_tokens("コンピューター");
        assert!(tokens_katakana >= 4, "7 个片假名应按 CJK 计算");
    }

    #[test]
    fn estimate_tokens_korean() {
        // 5 个韩字音节，按 CJK 算应约 4 token
        let tokens = estimate_tokens("안녕하세요");
        assert_eq!(tokens, 4, "5 个韩字应按 CJK 计算 ≈ 4 token");
    }

    #[test]
    fn estimate_tokens_extended_cjk() {
        // CJK 扩展 A 字符（\u{3400}-\u{4DBF}）也应识别
        let s = "\u{3400}\u{3401}\u{3402}\u{3403}\u{3404}\u{3405}"; // 6 个扩展 A 字符
        let tokens = estimate_tokens(s);
        // 6 个 CJK 字符: (6*2+2)/3 = 4 token
        assert_eq!(tokens, 4, "CJK 扩展 A 字符应被识别");
    }

    #[test]
    fn is_cjk_like_classification() {
        // 正确识别
        assert!(is_cjk_like('中'), "基本汉字");
        assert!(is_cjk_like('あ'), "平假名");
        assert!(is_cjk_like('ア'), "片假名");
        assert!(is_cjk_like('한'), "韩字");
        assert!(is_cjk_like('\u{3400}'), "CJK 扩展 A");
        assert!(is_cjk_like('\u{F900}'), "CJK 兼容");

        // 不应被识别
        assert!(!is_cjk_like('a'), "英文小写");
        assert!(!is_cjk_like('A'), "英文大写");
        assert!(!is_cjk_like('1'), "数字");
        assert!(!is_cjk_like(' '), "空格");
        assert!(!is_cjk_like('é'), "拉丁扩展");
        assert!(
            !is_cjk_like('\u{3000}'),
            "CJK 标点不算 CJK 字符（避免标点抬高估算）"
        );
    }

    // === Bug 4: clamp_context_percentage 防上游异常 ===

    #[test]
    fn clamp_percentage_normal() {
        assert_eq!(clamp_context_percentage(0.0), 0.0);
        assert_eq!(clamp_context_percentage(50.0), 50.0);
        assert_eq!(clamp_context_percentage(99.9), 99.9);
        assert_eq!(clamp_context_percentage(100.0), 100.0);
    }

    #[test]
    fn clamp_percentage_over_100() {
        assert_eq!(clamp_context_percentage(100.001), 100.0);
        assert_eq!(clamp_context_percentage(250.0), 100.0);
        assert_eq!(clamp_context_percentage(1e9), 100.0);
        assert_eq!(clamp_context_percentage(f64::MAX), 100.0);
    }

    #[test]
    fn clamp_percentage_negative() {
        assert_eq!(clamp_context_percentage(-0.0001), 0.0);
        assert_eq!(clamp_context_percentage(-10.0), 0.0);
        assert_eq!(clamp_context_percentage(-1e9), 0.0);
        assert_eq!(clamp_context_percentage(f64::MIN), 0.0);
    }

    #[test]
    fn clamp_percentage_nan_and_inf_safe() {
        // NaN 应被 is_finite 过滤，回退到 0
        assert_eq!(clamp_context_percentage(f64::NAN), 0.0);
        // 正负 inf 同样处理
        assert_eq!(clamp_context_percentage(f64::INFINITY), 0.0);
        assert_eq!(clamp_context_percentage(f64::NEG_INFINITY), 0.0);
    }

    // === 输出预算（max_tokens）截断：纯函数部分 ===

    #[test]
    fn truncate_to_budget_ascii() {
        // estimate_tokens("aaaaaaaa") = (8+3)/4 = 2 tokens
        assert_eq!(estimate_tokens("aaaaaaaa"), 2);
        // 预算 1 token：截到 ≤1 token 的最长前缀（4 字符 → (4+3)/4=1）
        let t = truncate_text_to_token_budget("aaaaaaaa", 1);
        assert!(estimate_tokens(&t) <= 1, "截断后不应超预算: {:?}", t);
        assert!(!t.is_empty(), "1 token 预算应能放下部分内容");
    }

    #[test]
    fn truncate_to_budget_zero_is_empty() {
        assert_eq!(truncate_text_to_token_budget("hello world", 0), "");
        assert_eq!(truncate_text_to_token_budget("hello", -5), "");
    }

    #[test]
    fn truncate_to_budget_respects_char_boundary() {
        // CJK 多字节字符不应被切在字节中间
        let s = "你好世界你好世界";
        let t = truncate_text_to_token_budget(s, 3);
        assert!(s.starts_with(&t), "截断结果应是原串前缀: {:?}", t);
        assert!(estimate_tokens(&t) <= 3);
        // 合法 UTF-8（能再 estimate 不 panic 即证明边界正确）
        let _ = estimate_tokens(&t);
    }
}
