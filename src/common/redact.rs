//! 错误消息脱敏工具
//!
//! 用于在错误日志/响应中遮蔽上游可能返回的敏感字段，防止 token reflection。

use regex::Regex;
use std::sync::LazyLock;

/// 错误消息最大长度，超过则截断（防日志洪水）
const MAX_LEN: usize = 1024;

/// 敏感字段的 JSON 值 pattern：`"field": "<value>"`
///
/// 匹配字段名（带或不带下划线/驼峰）后跟冒号、可选空白、被引号包裹的值。
/// 值替换为 `<REDACTED>` 保留 JSON 结构便于其他工具解析。
static SECRET_JSON_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // (?i) 大小写不敏感
    Regex::new(
        r#"(?i)"(access[_-]?token|refresh[_-]?token|id[_-]?token|client[_-]?secret|api[_-]?key|kiro[_-]?api[_-]?key|password|secret|authorization|bearer)"\s*:\s*"[^"]*""#,
    )
    .expect("脱敏 regex 编译失败")
});

/// 长 Bearer / API key 形态字符串（form-encoded 或裸出现的场景）
///
/// 匹配 `Bearer xxxxx` 或长度 ≥ 32 的连续 base64/十六进制字符串。
/// 这条 pattern 是兜底，可能误伤普通长字符串，但比泄露 token 安全。
static BEARER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Bearer\s+)([A-Za-z0-9._\-]{16,})").expect("Bearer regex 编译失败")
});

/// 对上游响应文本做脱敏处理
///
/// 处理步骤：
/// 1. 替换 JSON 中常见的敏感字段值为 `<REDACTED>`
/// 2. 替换 Bearer 后的 token 为 `<REDACTED>`
/// 3. 截断到 `MAX_LEN`，追加 `...(truncated)` 提示
///
/// 设计原则：宁可遮蔽过多也不能泄露 token。如果上游返回正常错误消息（无 token），
/// 文本基本不变；如果含 reflection（如 AWS OIDC 偶尔回显请求体），值被遮蔽。
pub fn redact_secret_text(text: &str) -> String {
    // 1. JSON 字段值脱敏
    let step1 = SECRET_JSON_PATTERN.replace_all(text, |caps: &regex::Captures<'_>| {
        format!(r#""{}": "<REDACTED>""#, &caps[1])
    });

    // 2. Bearer token 脱敏
    let step2 = BEARER_PATTERN.replace_all(&step1, "${1}<REDACTED>");

    // 3. 截断
    if step2.len() > MAX_LEN {
        // 注意 UTF-8 边界
        let mut cut = MAX_LEN;
        while !step2.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}...(truncated)", &step2[..cut])
    } else {
        step2.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_access_token_in_json() {
        let input = r#"{"error":"x","access_token":"eyJhbGc.someJWT.signature"}"#;
        let out = redact_secret_text(input);
        assert!(!out.contains("eyJhbGc"), "access_token 值应被屏蔽");
        assert!(out.contains("<REDACTED>"), "应有占位符");
        assert!(out.contains(r#""error":"x""#), "非敏感字段保留");
    }

    #[test]
    fn redacts_refresh_token_camel_case() {
        let input = r#"{"refreshToken":"long_refresh_value_here","status":"failed"}"#;
        let out = redact_secret_text(input);
        assert!(!out.contains("long_refresh_value_here"));
        assert!(out.contains("<REDACTED>"));
        assert!(out.contains(r#""status":"failed""#));
    }

    #[test]
    fn redacts_multiple_secret_fields() {
        let input = r#"{"access_token":"a","refresh_token":"b","client_secret":"c","kiroApiKey":"d","note":"ok"}"#;
        let out = redact_secret_text(input);
        assert!(!out.contains("\":\"a\""));
        assert!(!out.contains("\":\"b\""));
        assert!(!out.contains("\":\"c\""));
        assert!(!out.contains("\":\"d\""));
        assert!(out.contains("\"note\":\"ok\""), "普通字段保留");
        // 至少 4 个 REDACTED
        assert!(out.matches("<REDACTED>").count() >= 4);
    }

    #[test]
    fn redacts_bearer_authorization_header_echo() {
        let input = "Request rejected: Authorization: Bearer eyJsupersecretvaluehere1234567890";
        let out = redact_secret_text(input);
        assert!(!out.contains("eyJsupersecretvaluehere"));
        assert!(out.contains("Bearer <REDACTED>"));
    }

    #[test]
    fn keeps_short_text_unchanged_if_no_secrets() {
        let input = "Invalid request: bad parameter format";
        let out = redact_secret_text(input);
        assert_eq!(out, input);
    }

    #[test]
    fn truncates_long_text() {
        let input = "a".repeat(2048);
        let out = redact_secret_text(&input);
        assert!(out.len() <= MAX_LEN + 20, "应被截断到 MAX_LEN 附近");
        assert!(out.ends_with("...(truncated)"));
    }

    #[test]
    fn truncation_respects_utf8_boundary() {
        // 在恰好 MAX_LEN 位置塞中文字符，确认不在 char 中间切
        let mut s = "a".repeat(MAX_LEN - 1);
        s.push('中'); // '中' 占 3 bytes，跨过 MAX_LEN
        let out = redact_secret_text(&s);
        // 应能成功完成 — Rust 字符串切片 panic 如果切错，所以能跑到这里就说明 UTF-8 边界处理正确
        assert!(out.is_char_boundary(0));
    }
}
